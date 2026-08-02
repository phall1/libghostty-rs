//! Whole-blob terminal snapshot encoding and restoration.
//!
//! The native snapshot format is owned by the linked Ghostty revision and may
//! be rejected as [`Error::InvalidValue`](crate::Error::InvalidValue) by an
//! incompatible decoder. This module intentionally exposes only complete
//! snapshot blobs; it does not parse or expose Ghostty's internal records.

pub mod incremental;

use std::{marker::PhantomData, ops::Deref, ptr::NonNull};

use crate::{
    alloc::Allocator,
    error::{Error, Result, from_result},
    ffi,
    terminal::Terminal,
};

/// Immutable compatibility and feature metadata for the linked snapshot codec.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotCapabilities {
    /// Lowest accepted snapshot envelope version, inclusive.
    pub min_decode_version: u16,
    /// Highest accepted snapshot envelope version, inclusive.
    pub max_decode_version: u16,
    /// Envelope version emitted by [`Terminal::encode_snapshot`].
    pub default_encode_version: u16,
    /// Default encoding preserves the live VT parser continuation.
    pub continuation: bool,
    /// Default encoding has an authenticated renderable READY boundary.
    pub ready: bool,
    /// Default encoding carries history after READY.
    pub history: bool,
}

/// Query the snapshot codec linked into this process.
///
/// Consumers negotiate these values before exchanging opaque snapshot bytes;
/// they never need to parse an envelope themselves.
pub fn capabilities() -> Result<SnapshotCapabilities> {
    let mut raw = ffi::TerminalSnapshotCapabilities {
        size: std::mem::size_of::<ffi::TerminalSnapshotCapabilities>(),
        ..Default::default()
    };
    from_result(unsafe { ffi::ghostty_terminal_snapshot_capabilities(&raw mut raw) })?;
    Ok(SnapshotCapabilities {
        min_decode_version: raw.min_decode_version,
        max_decode_version: raw.max_decode_version,
        default_encode_version: raw.default_encode_version,
        continuation: raw.continuation,
        ready: raw.ready,
        history: raw.history,
    })
}

/// An encoded whole-terminal snapshot owned by libghostty's allocator.
///
/// This is a zero-copy owner for the allocation returned by
/// `ghostty_terminal_snapshot_encode`. Dropping it calls [`ffi::ghostty_free`]
/// exactly once with the same allocator and length used by the encode call.
/// Use [`AsRef`] or [`Deref`] to access the opaque native bytes.
#[derive(Debug)]
pub struct EncodedSnapshot<'alloc> {
    ptr: NonNull<u8>,
    len: usize,
    alloc: *const ffi::Allocator,
    _allocator: PhantomData<&'alloc ffi::Allocator>,
}

impl Drop for EncodedSnapshot<'_> {
    fn drop(&mut self) {
        // SAFETY: Construction records the allocator and exact allocation
        // length returned by libghostty. The lifetime prevents a custom
        // allocator from being dropped before these bytes.
        unsafe { ffi::ghostty_free(self.alloc, self.ptr.as_ptr(), self.len) };
    }
}

impl Deref for EncodedSnapshot<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: The allocation is exclusively owned by self, remains live
        // until Drop, and contains exactly len initialized encoded bytes.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl AsRef<[u8]> for EncodedSnapshot<'_> {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

/// A terminal restored from the first complete snapshot in an input buffer.
///
/// `consumed` is the number of bytes through Ghostty's snapshot FINISH record.
/// Bytes in the original input at `input[consumed..]` are trailing transport
/// data and were not processed by the decoder.
#[derive(Debug)]
pub struct DecodedSnapshot<'alloc: 'cb, 'cb> {
    /// The restored, normally usable terminal.
    pub terminal: Terminal<'alloc, 'cb>,
    /// Number of input bytes consumed by exactly one snapshot.
    pub consumed: usize,
}

impl<'alloc: 'cb, 'cb> Terminal<'alloc, 'cb> {
    /// Encode this terminal and its live VT stream continuation.
    ///
    /// The returned bytes use libghostty's default allocator and are freed by
    /// [`EncodedSnapshot`] without copying into a Rust `Vec`.
    pub fn encode_snapshot(&self) -> Result<EncodedSnapshot<'static>> {
        // SAFETY: A NULL allocator selects libghostty's default allocator,
        // which has no borrowed lifetime.
        unsafe { self.encode_snapshot_inner(std::ptr::null()) }
    }

    /// Encode this terminal using a custom allocator for the returned bytes.
    ///
    /// The allocator is borrowed for the lifetime of the encoded snapshot so
    /// Drop can call `ghostty_free` with the exact same allocator.
    pub fn encode_snapshot_with_alloc<'snapshot, 'ctx: 'snapshot>(
        &self,
        alloc: &'snapshot Allocator<'ctx>,
    ) -> Result<EncodedSnapshot<'snapshot>> {
        // SAFETY: The returned owner is tied to the allocator borrow.
        unsafe { self.encode_snapshot_inner(alloc.to_raw()) }
    }

    unsafe fn encode_snapshot_inner<'snapshot>(
        &self,
        alloc: *const ffi::Allocator,
    ) -> Result<EncodedSnapshot<'snapshot>> {
        let mut raw = ffi::TerminalSnapshot {
            size: std::mem::size_of::<ffi::TerminalSnapshot>(),
            data: std::ptr::null_mut(),
            len: 0,
        };
        let status = unsafe {
            ffi::ghostty_terminal_snapshot_encode(alloc, self.inner.as_raw(), &raw mut raw)
        };

        if let Err(error) = from_result(status) {
            // The C contract clears outputs on failure. Defensively release a
            // non-NULL partial result so an ABI regression cannot leak it.
            unsafe { free_raw_snapshot(alloc, &mut raw) };
            return Err(error);
        }

        let Some(ptr) = NonNull::new(raw.data) else {
            return Err(Error::InvalidValue);
        };
        if raw.len == 0 {
            unsafe { free_raw_snapshot(alloc, &mut raw) };
            return Err(Error::InvalidValue);
        }

        Ok(EncodedSnapshot {
            ptr,
            len: raw.len,
            alloc,
            _allocator: PhantomData,
        })
    }

    /// Decode exactly one snapshot with a custom allocator.
    ///
    /// The returned terminal owns all native allocations made through `alloc`,
    /// so its lifetime cannot outlive the allocator. Malformed, truncated, or
    /// version-incompatible input returns [`Error::InvalidValue`]; allocation
    /// failure returns [`Error::OutOfMemory`]. Trailing bytes are accepted and
    /// reported through [`DecodedSnapshot::consumed`].
    pub fn decode_snapshot_with_alloc<'ctx: 'alloc>(
        alloc: &'alloc Allocator<'ctx>,
        data: &[u8],
    ) -> Result<DecodedSnapshot<'alloc, 'cb>> {
        // SAFETY: The decoded terminal lifetime is tied to the allocator
        // borrow, and the decoder consumes data synchronously without retaining
        // the input slice.
        unsafe { Self::decode_snapshot_inner(alloc.to_raw(), data) }
    }

    unsafe fn decode_snapshot_inner(
        alloc: *const ffi::Allocator,
        data: &[u8],
    ) -> Result<DecodedSnapshot<'alloc, 'cb>> {
        let mut raw = ffi::TerminalSnapshotDecodeResult {
            size: std::mem::size_of::<ffi::TerminalSnapshotDecodeResult>(),
            terminal: std::ptr::null_mut(),
            consumed: 0,
        };
        let status = unsafe {
            ffi::ghostty_terminal_snapshot_decode(alloc, data.as_ptr(), data.len(), &raw mut raw)
        };

        if let Err(error) = from_result(status) {
            // The C operation is transactional and clears terminal on failure.
            // Defensively honor ownership if a future implementation publishes
            // one despite returning an error.
            if !raw.terminal.is_null() {
                unsafe { ffi::ghostty_terminal_free(raw.terminal) };
            }
            return Err(error);
        }

        if raw.terminal.is_null() || raw.consumed == 0 || raw.consumed > data.len() {
            if !raw.terminal.is_null() {
                unsafe { ffi::ghostty_terminal_free(raw.terminal) };
            }
            return Err(Error::InvalidValue);
        }

        // SAFETY: Successful decode returns a uniquely owned terminal whose
        // allocations use alloc. It does not retain data. from_raw installs the
        // same stable callback storage as the normal constructor.
        let terminal = unsafe { Terminal::from_raw(raw.terminal) }?;
        Ok(DecodedSnapshot {
            terminal,
            consumed: raw.consumed,
        })
    }
}

impl<'cb> Terminal<'static, 'cb> {
    /// Decode exactly one snapshot using libghostty's default allocator.
    ///
    /// The decoder does not retain `data`. See
    /// [`Terminal::decode_snapshot_with_alloc`] for error and trailing-byte
    /// behavior.
    pub fn decode_snapshot(data: &[u8]) -> Result<DecodedSnapshot<'static, 'cb>> {
        // SAFETY: A NULL allocator has no borrowed lifetime, and the returned
        // terminal still gets ordinary Rust-owned callback storage.
        unsafe { Self::decode_snapshot_inner(std::ptr::null(), data) }
    }
}

unsafe fn free_raw_snapshot(alloc: *const ffi::Allocator, raw: &mut ffi::TerminalSnapshot) {
    if let Some(ptr) = NonNull::new(raw.data) {
        unsafe { ffi::ghostty_free(alloc, ptr.as_ptr(), raw.len) };
        raw.data = std::ptr::null_mut();
        raw.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, ffi::c_void};

    fn terminal() -> Terminal<'static, 'static> {
        Terminal::new(crate::TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 1000,
        })
        .expect("terminal should initialize")
    }

    fn parse_fixture(value: &str) -> Vec<u8> {
        value
            .lines()
            .flat_map(|line| line.split_once('#').map_or(line, |(data, _)| data).split_whitespace())
            .map(|byte| u8::from_str_radix(byte, 16).expect("fixture hex byte"))
            .collect()
    }

    fn corpus_checksum(bytes: &[u8]) -> u64 {
        bytes.iter().fold(14_695_981_039_346_656_037, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(1_099_511_628_211)
        })
    }

    fn before_ready_error(data: &[u8], end_input: bool) -> incremental::Error {
        use incremental::{DecodeStep, Decoder, DecoderOptions};

        let mut decoder = Decoder::new(DecoderOptions::default()).expect("decoder construction");
        let mut offset = 0;
        loop {
            if offset == data.len() {
                assert!(end_input, "fixture unexpectedly needs more input");
                return decoder
                    .end_input()
                    .expect_err("end of incomplete input must fail")
                    .error;
            }
            match decoder.push(&data[offset..]) {
                Ok(DecodeStep::NeedInput {
                    decoder: next,
                    progress,
                })
                | Ok(DecodeStep::Progress {
                    decoder: next,
                    progress,
                }) => {
                    assert!(progress.consumed > 0);
                    offset += progress.consumed;
                    decoder = next;
                }
                Ok(DecodeStep::Ready { .. }) => {
                    panic!("failure fixture reached READY")
                }
                Err(failure) => return failure.error,
            }
        }
    }

    #[test]
    fn shared_codec_corpus_is_an_opaque_semantic_oracle() {
        let cases = [
            (
                "shell-80x24",
                include_str!("../testdata/snapshot-corpus/shell-80x24-v2.hex"),
                31_920,
                0x7940_94e8_f39f_40d8,
                (80, 24),
            ),
            (
                "rich-200x60",
                include_str!("../testdata/snapshot-corpus/rich-200x60-v2.hex"),
                385_539,
                0x9b74_6bfb_359a_5eeb,
                (200, 60),
            ),
            (
                "history-multipage",
                include_str!("../testdata/snapshot-corpus/history-multipage-v2.hex"),
                771_100,
                0x963a_ccc4_0a87_c60d,
                (80, 24),
            ),
        ];

        for (name, fixture, expected_len, expected_checksum, geometry) in cases {
            let bytes = parse_fixture(fixture);
            assert_eq!(bytes.len(), expected_len, "{name} byte length");
            assert_eq!(
                corpus_checksum(&bytes),
                expected_checksum,
                "{name} byte checksum"
            );

            let decoded = Terminal::decode_snapshot(&bytes).expect("corpus snapshot decode");
            assert_eq!(decoded.consumed, bytes.len(), "{name} consumption");
            assert_eq!(
                (
                    decoded.terminal.cols().expect("decoded columns"),
                    decoded.terminal.rows().expect("decoded rows"),
                ),
                geometry,
                "{name} geometry"
            );
            let reencoded = decoded
                .terminal
                .encode_snapshot()
                .expect("semantic oracle re-encode");
            assert_eq!(reencoded.as_ref(), bytes, "{name} exact semantic oracle");
        }

        let v1 = parse_fixture(include_str!(
            "../testdata/snapshot-corpus/compat-v1.hex"
        ));
        let decoded = Terminal::decode_snapshot(&v1).expect("v1 compatibility decode");
        assert_eq!(decoded.consumed, v1.len());
        let upgraded = decoded
            .terminal
            .encode_snapshot()
            .expect("v1 semantic re-encode");
        Terminal::decode_snapshot(&upgraded).expect("upgraded v1 state decodes");

        let control = parse_fixture(include_str!(
            "../testdata/snapshot-corpus/shell-80x24-v2.hex"
        ));
        let mut future = control.clone();
        future[8] = 3;
        future[9] = 0;
        assert_eq!(
            before_ready_error(&future, false),
            incremental::Error::UnknownVersion
        );

        let mut corrupt = control.clone();
        corrupt[20] ^= 0x80;
        assert_eq!(
            before_ready_error(&corrupt, false),
            incremental::Error::Corruption
        );
        assert_eq!(
            before_ready_error(&control[..20], true),
            incremental::Error::Truncated
        );
    }

    fn assert_equivalent(a: &Terminal<'_, '_>, b: &Terminal<'_, '_>) {
        let a = a.encode_snapshot().expect("first terminal should encode");
        let b = b.encode_snapshot().expect("second terminal should encode");
        assert_eq!(a.as_ref(), b.as_ref());
    }

    #[test]
    fn reports_linked_codec_capabilities() {
        assert_eq!(
            capabilities().expect("capability query"),
            SnapshotCapabilities {
                min_decode_version: 1,
                max_decode_version: 2,
                default_encode_version: 2,
                continuation: true,
                ready: true,
                history: true,
            },
        );
    }

    #[test]
    fn ground_roundtrip_restores_usable_terminal() {
        let mut source = terminal();
        source.vt_write(b"hello\r\nworld");

        let encoded = source.encode_snapshot().expect("snapshot should encode");
        let mut decoded = Terminal::decode_snapshot(&encoded).expect("snapshot should decode");
        assert_eq!(decoded.consumed, encoded.len());
        assert_equivalent(&source, &decoded.terminal);

        source.vt_write(b"\r\nafter restore");
        decoded.terminal.vt_write(b"\r\nafter restore");
        assert_equivalent(&source, &decoded.terminal);
    }

    #[test]
    fn split_csi_continues_after_decode() {
        let mut source = terminal();
        source.vt_write(b"\x1b[31");

        let encoded = source.encode_snapshot().expect("snapshot should encode");
        let mut decoded = Terminal::decode_snapshot(&encoded).expect("snapshot should decode");
        source.vt_write(b"mred");
        decoded.terminal.vt_write(b"mred");

        assert_equivalent(&source, &decoded.terminal);
    }

    #[test]
    fn split_utf8_continues_after_decode_and_accepts_further_writes() {
        let mut source = terminal();
        source.vt_write(b"\xF0\x9F\x98");

        let encoded = source.encode_snapshot().expect("snapshot should encode");
        let mut decoded = Terminal::decode_snapshot(&encoded).expect("snapshot should decode");
        source.vt_write(b"\x80 tail");
        decoded.terminal.vt_write(b"\x80 tail");

        assert_equivalent(&source, &decoded.terminal);
    }

    #[test]
    fn decode_reports_trailing_bytes() {
        let mut source = terminal();
        source.vt_write(b"state");
        let encoded = source.encode_snapshot().expect("snapshot should encode");
        let trailing = b"trailing transport bytes";
        let mut input = Vec::with_capacity(encoded.len() + trailing.len());
        input.extend_from_slice(&encoded);
        input.extend_from_slice(trailing);

        let decoded = Terminal::decode_snapshot(&input).expect("snapshot should decode");
        assert_eq!(decoded.consumed, encoded.len());
        assert_eq!(&input[decoded.consumed..], trailing);
    }

    #[test]
    fn malformed_and_truncated_snapshots_are_rejected() {
        let encoded = terminal()
            .encode_snapshot()
            .expect("snapshot should encode");
        let mut malformed = encoded.to_vec();
        malformed[0] ^= 1;

        for input in [&malformed[..], &encoded[..encoded.len() - 1]] {
            assert!(matches!(
                Terminal::decode_snapshot(input),
                Err(Error::InvalidValue)
            ));
        }
    }

    #[test]
    fn callbacks_can_borrow_host_state_after_decode() {
        let encoded = terminal()
            .encode_snapshot()
            .expect("snapshot should encode");
        let bell_count = Cell::new(0usize);
        let mut decoded = Terminal::decode_snapshot(&encoded).expect("snapshot should decode");

        decoded
            .terminal
            .on_bell(|_| bell_count.set(bell_count.get() + 1))
            .expect("callback should register");
        decoded.terminal.vt_write(b"\x07");
        assert_eq!(bell_count.get(), 1);
    }

    #[derive(Default)]
    struct AllocationCounts {
        allocations: Cell<usize>,
        frees: Cell<usize>,
    }

    unsafe extern "C" fn tracking_alloc(
        ctx: *mut c_void,
        len: usize,
        alignment: u8,
        _ret_addr: usize,
    ) -> *mut c_void {
        let Ok(layout) = std::alloc::Layout::from_size_align(len, 1usize << alignment) else {
            return std::ptr::null_mut();
        };
        let ptr = unsafe { std::alloc::alloc(layout) };
        if !ptr.is_null() {
            let counts = unsafe { &*ctx.cast::<AllocationCounts>() };
            counts.allocations.set(counts.allocations.get() + 1);
        }
        ptr.cast()
    }

    unsafe extern "C" fn tracking_resize(
        _ctx: *mut c_void,
        _memory: *mut c_void,
        _memory_len: usize,
        _alignment: u8,
        _new_len: usize,
        _ret_addr: usize,
    ) -> bool {
        false
    }

    unsafe extern "C" fn tracking_remap(
        _ctx: *mut c_void,
        memory: *mut c_void,
        memory_len: usize,
        alignment: u8,
        new_len: usize,
        _ret_addr: usize,
    ) -> *mut c_void {
        let Ok(layout) = std::alloc::Layout::from_size_align(memory_len, 1usize << alignment)
        else {
            return std::ptr::null_mut();
        };
        unsafe { std::alloc::realloc(memory.cast(), layout, new_len).cast() }
    }

    unsafe extern "C" fn tracking_free(
        ctx: *mut c_void,
        memory: *mut c_void,
        memory_len: usize,
        alignment: u8,
        _ret_addr: usize,
    ) {
        let Ok(layout) = std::alloc::Layout::from_size_align(memory_len, 1usize << alignment)
        else {
            return;
        };
        unsafe { std::alloc::dealloc(memory.cast(), layout) };
        let counts = unsafe { &*ctx.cast::<AllocationCounts>() };
        counts.frees.set(counts.frees.get() + 1);
    }

    static TRACKING_VTABLE: ffi::AllocatorVtable = ffi::AllocatorVtable {
        alloc: Some(tracking_alloc),
        resize: Some(tracking_resize),
        remap: Some(tracking_remap),
        free: Some(tracking_free),
    };

    fn tracking_allocator(counts: &AllocationCounts) -> Allocator<'_> {
        let raw = ffi::Allocator {
            ctx: std::ptr::from_ref(counts).cast_mut().cast(),
            vtable: &TRACKING_VTABLE,
        };
        // SAFETY: The copied allocator points to static functions and counts,
        // which outlives the returned allocator and everything built from it.
        unsafe { Allocator::from_raw(&raw) }
    }

    #[test]
    fn custom_allocator_success_and_failure_paths_free_once() {
        let counts = AllocationCounts::default();
        {
            let alloc = tracking_allocator(&counts);
            let mut source = Terminal::new_with_alloc(
                &alloc,
                crate::TerminalOptions {
                    cols: 80,
                    rows: 24,
                    max_scrollback: 1000,
                },
            )
            .expect("terminal should initialize");
            source.vt_write(b"allocator-owned state");

            let encoded = source
                .encode_snapshot_with_alloc(&alloc)
                .expect("snapshot should encode");
            let decoded = Terminal::decode_snapshot_with_alloc(&alloc, &encoded)
                .expect("snapshot should decode");
            assert_eq!(decoded.consumed, encoded.len());

            let mut malformed = encoded.to_vec();
            malformed[0] ^= 1;
            assert!(matches!(
                Terminal::decode_snapshot_with_alloc(&alloc, &malformed),
                Err(Error::InvalidValue)
            ));
        }

        assert!(counts.allocations.get() > 0);
        assert_eq!(counts.allocations.get(), counts.frees.get());
    }

    unsafe extern "C" fn fail_alloc(
        _ctx: *mut c_void,
        _len: usize,
        _alignment: u8,
        _ret_addr: usize,
    ) -> *mut c_void {
        std::ptr::null_mut()
    }

    static FAILING_VTABLE: ffi::AllocatorVtable = ffi::AllocatorVtable {
        alloc: Some(fail_alloc),
        resize: Some(tracking_resize),
        remap: Some(tracking_remap),
        free: Some(tracking_free),
    };

    #[test]
    fn allocation_failure_maps_to_out_of_memory() {
        let source = terminal();
        let encoded = source.encode_snapshot().expect("snapshot should encode");
        let counts = AllocationCounts::default();
        let raw = ffi::Allocator {
            ctx: std::ptr::from_ref(&counts).cast_mut().cast(),
            vtable: &FAILING_VTABLE,
        };
        // SAFETY: The callback table is static and counts outlives alloc.
        let alloc = unsafe { Allocator::from_raw(&raw) };

        assert!(matches!(
            source.encode_snapshot_with_alloc(&alloc),
            Err(Error::OutOfMemory)
        ));
        assert!(matches!(
            Terminal::decode_snapshot_with_alloc(&alloc, &encoded),
            Err(Error::OutOfMemory)
        ));
    }
}
