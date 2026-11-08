//! Encode and restore the complete state of a terminal via a binary format.
//!
//! A snapshot is an ordered, CRC-protected record stream. Its READY marker
//! follows enough state to render and resume the terminal, including any
//! unfinished VT parser input. Older scrollback pages follow READY and the
//! FINISH marker terminates the complete snapshot.
//!
//! End-of-file before an operation's required READY or FINISH marker is
//! malformed, truncated snapshot data and returns [`Error::InvalidValue`].
//! [`Error::IoError`] is reserved for reader errors.
//!
//! Decoding is done with the dedicated [`Decoder`] struct; encoding, meanwhile,
//! is supported by methods on [`Terminal`] like [`Terminal::encode_snapshot`].
//!
//! # Format
//!
//! Every integer is unsigned and little-endian.
//! The stream begins with this fixed ten-byte envelope:
//!
//! ```text
//! byte  0               8       10
//!       +---------------+--------+
//!       | "GHOSTSNP"    | version|
//!       | 8-byte magic  | u16    |
//!       +---------------+--------+
//! ```
//!
//! The envelope is followed by independently checksummed records. A record's
//! CRC32C covers its encoded tag and payload length followed by its payload;
//! it does not cover the CRC field itself.
//!
//! ```text
//! byte  0       2             6          10             10 + payload_len
//!       +-------+-------------+-----------+----------------+
//!       | tag   | payload_len | CRC32C    | payload        |
//!       | u16   | u32         | u32       | payload_len B  |
//!       +-------+-------------+-----------+----------------+
//!       \____________________/             \______________/
//!          CRC prefix                         CRC suffix
//! ```
//!
//! Record groups occur in this strict order. SCREEN and HISTORY groups contain
//! one entry for each screen declared by TERMINAL. Each manifest is followed by
//! the number of PAGE records it declares. Active SCREEN pages make the terminal
//! renderable; HISTORY pages are older scrollback ordered newest to oldest so
//! an incremental decoder can prepend them as they arrive.
//!
//! ```text
//!
//! +---------------- TERMINAL ----------------+
//! | terminal-wide state and screen count     |
//! +----------------- SCREEN -----------------+  repeated per screen
//! | active-screen manifest                   |
//! +------------------ PAGE ------------------+  repeated per manifest
//! | active screen rows                       |
//! +---------- KITTY_IMAGE_STORAGE -----------+  optional, per screen
//! | images, placements, in-progress load     |
//! +------------- CONTINUATION ---------------+
//! | unfinished VT/UTF-8 input, or ground     |
//! +------------------ READY -----------------+
//! | empty renderable-state marker            |  ready() returns here
//! +---------- KITTY_IMAGE_PAYLOAD -----------+  optional, anywhere until FINISH,
//! | one image's or frame's pixels            |  next() consumes one
//! +----------------- HISTORY ----------------+  repeated per screen
//! | scrollback manifest                      |
//! +------------------ PAGE ------------------+  next() consumes one page
//! | older screen rows                        |
//! +------ KITTY_IMAGE_PAGE_PLACEMENTS -------+  optional, after a PAGE
//! | placements pinned to that page           |
//! +------------------ FINISH ----------------+
//! | empty end-of-snapshot marker             |  next() returns NO_VALUE
//! +------------------------------------------+
//! | trailing transport bytes (not consumed) |
//! +------------------------------------------+
//! ```
//!
//! READY separates the renderable prefix through CONTINUATION from history.
//! FINISH terminates the record sequence. Both are empty records protected by
//! CRC32C, like every other record. Declared record counts, tags, and strict
//! decoding enforce the stream's ordering and completeness.
//!
//! A decoder skips any record whose tag it does not know, wherever it
//! appears. This lets a newer library add optional kinds of records that
//! older libraries ignore, without changing the snapshot version. Skipped
//! records are still checked, so a damaged record fails decoding even when
//! its tag is unknown.
//!
//! The Kitty graphics records are optional in this way. A library built
//! without Kitty graphics skips them and restores the same terminal without
//! its images, and a terminal without images writes none of them.
//!
//! Images are restored at READY with their pixels pending, and the pixels
//! arrive later through [`IncrementalDecoder::next`].
// The Kitty graphics items only exist with the `kitty-graphics` feature, so
// only link to them then; otherwise the links would be broken.
#![cfg_attr(
    feature = "kitty-graphics",
    doc = "A pending image can be placed and queried, but \
           [`Image::data`](crate::kitty::graphics::Image::data) returns \
           `Ok(None)` for it until its pixels arrive. When they do, the image \
           keeps its [`Image::generation`](crate::kitty::graphics::Image::generation), \
           but [`Graphics::generation`](crate::kitty::graphics::Graphics::generation) \
           changes, so a renderer that watches that value picks up the new \
           pixels on its next frame."
)]
#![cfg_attr(
    not(feature = "kitty-graphics"),
    doc = "A pending image can be placed and queried, but `Image::data` \
           returns `Ok(None)` for it until its pixels arrive. When they do, \
           the image keeps its `Image::generation`, but `Graphics::generation` \
           changes, so a renderer that watches that value picks up the new \
           pixels on its next frame."
)]
//!
//! Snapshot format version 1 is a work in progress and does not yet carry a
//! binary-compatibility guarantee.
//!
//! ## See also
//!
//! [Snapshot format and Zig codec documentation](https://github.com/ghostty-org/ghostty/blob/main/src/terminal/snapshot/main.zig)
use std::{
    io::{Read, Write},
    marker::PhantomData,
    mem::MaybeUninit,
};

use crate::{
    alloc::{Allocator, Bytes, Object},
    error::{
        Error, Result, from_optional_result, from_optional_result_uninit,
        from_optional_result_with_len, from_result,
    },
    ffi::{self, SnapshotDecoderData as Data, SnapshotDecoderOption as Opt},
    screen::Screen,
    terminal::Terminal,
};

/// Snapshot-related methods.
impl Terminal<'_, '_> {
    /// Encode a complete terminal snapshot to a writer.
    ///
    /// The terminal's persistent VT stream supplies the continuation bytes
    /// needed to reconstruct unfinished parser state. The caller must prevent
    /// concurrent writes or other terminal mutation for the duration of this
    /// call. The writer callback must not call terminal APIs with the same
    /// terminal handle. A terminal can be encoded with tracking disabled when
    /// its VT parser and UTF-8 decoder are both at ground. If either is
    /// unfinished, tracking must have been enabled before the input that
    /// produced that state was written; otherwise this returns
    /// [`Error::InvalidValue`].
    ///
    /// Encoding begins at the writer's current position. If an error occurs,
    /// the writer may contain a partial snapshot without a valid FINISH
    /// marker. Calls to the writer are synchronous; this function does not
    /// flush or make the caller's destination durable.
    ///
    /// # Errors
    ///
    /// This function returns [`Error::IoError`] if the writer rejects output,
    /// [`Error::LimitExceeded`] if output accounting overflows, or another
    /// error code on failure.
    pub fn encode_snapshot<W: Write>(&mut self, writer: &mut W) -> Result<()> {
        let writer = crate::io::to_writer(writer);
        let result = unsafe { ffi::ghostty_snapshot_encode(self.inner.as_raw(), writer) };
        from_result(result)
    }

    /// Encode a complete terminal snapshot to an allocated buffer.
    ///
    /// The returned buffer is allocated with allocator, or the default
    /// allocator when allocator is `None`.
    ///
    /// A terminal can be encoded with tracking disabled when its VT parser
    /// and UTF-8 decoder are both at ground. If either is unfinished, tracking
    /// must have been enabled before the input that produced that state was
    /// written; otherwise this returns [`Error::InvalidValue`].
    pub fn encode_snapshot_alloc<'a, 'ctx: 'a>(
        &self,
        alloc: Option<&'a Allocator<'ctx>>,
    ) -> Result<Option<Bytes<'a>>> {
        let mut out = std::ptr::null_mut();
        let mut out_len = 0usize;
        let alloc = alloc.map_or(std::ptr::null(), super::alloc::Allocator::to_raw);

        let result = unsafe {
            ffi::ghostty_snapshot_encode_alloc(
                self.inner.as_raw(),
                alloc,
                &raw mut out,
                &raw mut out_len,
            )
        };

        let out = from_optional_result(result, out)?;
        // SAFETY: On success, libghostty hands over `out_len` bytes allocated
        // with `alloc`, or NULL for empty output.
        Ok(out.map(|ptr| unsafe { Bytes::from_raw_parts(ptr, out_len, alloc) }))
    }

    /// Encode a complete terminal snapshot to a caller-provided buffer.
    ///
    /// Pass an empty `buf` to query the required size. A size query returns
    /// [`Error::OutOfSpace`] with the required size, including zero when the
    /// stream is at ground. If a non-empty buffer is too small, the function
    /// has the same result and reports the full required size.
    ///
    /// A terminal can be encoded with tracking disabled when its VT parser
    /// and UTF-8 decoder are both at ground. If either is unfinished, tracking
    /// must have been enabled before the input that produced that state was
    /// written; otherwise this returns [`Error::InvalidValue`].
    pub fn encode_snapshot_buf(&self, buf: &mut [u8]) -> Result<Option<usize>> {
        let mut written = 0usize;

        let result = unsafe {
            ffi::ghostty_snapshot_encode_buf(
                self.inner.as_raw(),
                buf.as_mut_ptr(),
                buf.len(),
                &raw mut written,
            )
        };

        from_optional_result_with_len(result, written)
    }
}

const RECORD_HEADER_BYTES: usize = 10;

/// Own callback storage through its original raw pointer. A movable Box is not
/// enough: deriving userdata from a mutable Box reborrow and then reborrowing
/// through that Box can invalidate the retained pointer's aliasing provenance.
#[derive(Debug)]
struct CallbackContext<T>(std::ptr::NonNull<T>);

impl<T> CallbackContext<T> {
    fn new(value: T) -> Self {
        // Box::into_raw yields a non-null, independently owned base pointer.
        Self(std::ptr::NonNull::new(Box::into_raw(Box::new(value))).unwrap())
    }

    fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }
}

impl<T> std::ops::Deref for CallbackContext<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: The allocation is owned here. Native accesses are synchronous
        // and never overlap one of these short-lived Rust reborrows.
        unsafe { self.0.as_ref() }
    }
}

impl<T> std::ops::DerefMut for CallbackContext<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: As above, rooted at the same raw base native retains.
        unsafe { self.0.as_mut() }
    }
}

impl<T> Drop for CallbackContext<T> {
    fn drop(&mut self) {
        // SAFETY: Native owners release their callback before this field drops.
        // The pointer came from Box::into_raw and is reclaimed exactly once.
        unsafe { drop(Box::from_raw(self.0.as_ptr())) };
    }
}

#[derive(Debug)]
struct FeedReader {
    bytes: Vec<u8>,
    offset: usize,
    max_bytes: usize,
}

const RECORD_TAG_PAGE: u16 = 3;
const RECORD_TAG_HISTORY: u16 = 4;
const RECORD_TAG_FINISH: u16 = 6;
const RECORD_TAG_IMAGE: u16 = 9;
const RECORD_TAG_PLACEMENTS: u16 = 10;

fn staged_record(bytes: &[u8], offset: usize) -> Result<(u16, usize, usize)> {
    let header_end = offset
        .checked_add(RECORD_HEADER_BYTES)
        .ok_or(Error::InvalidValue)?;
    let header = bytes.get(offset..header_end).ok_or(Error::InvalidValue)?;
    let tag = u16::from_le_bytes([header[0], header[1]]);
    let payload_len = u32::from_le_bytes([header[2], header[3], header[4], header[5]]) as usize;
    let end = header_end
        .checked_add(payload_len)
        .filter(|end| *end <= bytes.len())
        .ok_or(Error::InvalidValue)?;
    Ok((tag, header_end, end))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StagedHistoryKind {
    Page { rows: usize, placements: bool },
    Image,
    Finish,
}

/// Validate the complete framing boundary before native can mutate a terminal.
/// CRCs are checked by native's record-only preflight after structural and row
/// validation, but before the decoder can mutate the terminal.
fn validate_staged_history_unit(bytes: &[u8]) -> Result<StagedHistoryKind> {
    let mut offset = 0;
    let mut histories = 0;
    loop {
        let (tag, payload, end) = staged_record(bytes, offset)?;
        match tag {
            RECORD_TAG_HISTORY if histories < 2 => {
                histories += 1;
                offset = end;
            }
            RECORD_TAG_PAGE => {
                let rows_end = payload.checked_add(4).ok_or(Error::InvalidValue)?;
                let header = bytes
                    .get(payload..rows_end)
                    .filter(|_| rows_end <= end)
                    .ok_or(Error::InvalidValue)?;
                return Ok(StagedHistoryKind::Page {
                    rows: u16::from_le_bytes([header[2], header[3]]) as usize,
                    placements: validate_page_suffix(bytes, end)?,
                });
            }
            RECORD_TAG_IMAGE if end == bytes.len() => return Ok(StagedHistoryKind::Image),
            RECORD_TAG_FINISH if end == bytes.len() => return Ok(StagedHistoryKind::Finish),
            // Like native nextHeader, tolerate future optional records before
            // a progress-bearing unit, but never concatenate two units.
            0 | 11..=u16::MAX => offset = end,
            _ => return Err(Error::InvalidValue),
        }
    }
}

fn validate_page_suffix(bytes: &[u8], mut offset: usize) -> Result<bool> {
    if offset == bytes.len() {
        return Ok(false);
    }
    loop {
        let (tag, _, end) = staged_record(bytes, offset)?;
        match tag {
            RECORD_TAG_PLACEMENTS if end == bytes.len() => return Ok(true),
            // Native explicitly reads the placements header in this mode and
            // skips optional unknown records on the way, including their CRCs.
            0 | 11..=u16::MAX => offset = end,
            _ => return Err(Error::InvalidValue),
        }
    }
}

impl FeedReader {
    fn new(max_bytes: usize) -> Result<Self> {
        if max_bytes == 0 {
            return Err(Error::InvalidValue);
        }
        Ok(Self {
            bytes: Vec::new(),
            offset: 0,
            max_bytes,
        })
    }

    fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(total) = self.bytes.len().checked_add(bytes.len()) else {
            return Err(Error::LimitExceeded);
        };
        if total > self.max_bytes {
            return Err(Error::LimitExceeded);
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|_| Error::OutOfMemory)?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn finish_operation(&mut self) -> bool {
        let consumed_all = self.offset == self.bytes.len();
        // Release potentially large READY/page storage as soon as native has
        // consumed the explicit unit. This also makes later feeds start at 0.
        self.bytes.clear();
        self.bytes.shrink_to_fit();
        self.offset = 0;
        consumed_all
    }
}

impl Read for FeedReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = &self.bytes[self.offset..];
        let len = remaining.len().min(buf.len());
        buf[..len].copy_from_slice(&remaining[..len]);
        self.offset += len;
        Ok(len)
    }
}

#[derive(Debug)]
struct FeedCore<'alloc> {
    inner: Object<'alloc, ffi::SnapshotDecoderImpl>,
    // Native retains the original raw userdata pointer. Moving the owner does
    // not retag the pointee; Drop frees native state before this allocation.
    source: CallbackContext<FeedReader>,
    failed: bool,
}

impl FeedCore<'_> {
    unsafe fn new_inner(alloc: *const ffi::Allocator, max_staged_bytes: usize) -> Result<Self> {
        let source = CallbackContext::new(FeedReader::new(max_staged_bytes)?);
        // SAFETY: Source owns the raw base pointer, moves without retagging
        // its pointee, and outlives the native decoder's synchronous callbacks.
        let reader = unsafe { crate::io::to_reader_raw(source.as_ptr()) };
        let mut raw: ffi::SnapshotDecoder = std::ptr::null_mut();
        let result = unsafe { ffi::ghostty_snapshot_decoder_new(alloc, &raw mut raw, reader) };
        from_result(result)?;
        Ok(Self {
            inner: Object::new(raw)?,
            source,
            failed: false,
        })
    }

    fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        if self.failed {
            return Err(Error::InvalidValue);
        }
        self.source.feed(bytes)
    }

    fn complete_operation(&mut self, result: ffi::Result::Type) -> Result<()> {
        let consumed_all = self.source.finish_operation();
        if result != ffi::Result::SUCCESS || !consumed_all {
            self.failed = true;
        }
        from_result(result)?;
        if !consumed_all {
            return Err(Error::InvalidValue);
        }
        Ok(())
    }

    fn get<T>(&self, tag: Data::Type) -> Result<T> {
        let mut value = MaybeUninit::<T>::zeroed();
        let result = unsafe {
            ffi::ghostty_snapshot_decoder_get(self.inner.as_raw(), tag, value.as_mut_ptr().cast())
        };
        from_result(result)?;
        // SAFETY: A successful typed getter initializes its output.
        Ok(unsafe { value.assume_init() })
    }
}

impl Drop for FeedCore<'_> {
    fn drop(&mut self) {
        // End native access to the separately owned callback context before
        // CallbackContext reclaims its Rust allocation.
        unsafe { ffi::ghostty_snapshot_decoder_free(self.inner.as_raw()) };
    }
}

/// An owned, bounded staging source for the pull-based snapshot decoder.
///
/// Network or other nonblocking input should be accumulated with [`Self::feed`]
/// and [`Self::ready`] called only after the complete READY record has arrived.
/// Native code therefore never mistakes temporary starvation for permanent
/// EOF. The staged bytes are released after each decoding operation.
#[derive(Debug)]
pub struct FeedDecoder<'alloc> {
    core: FeedCore<'alloc>,
}

impl FeedDecoder<'static> {
    /// Create a decoder using the default allocator.
    pub fn new(max_staged_bytes: usize) -> Result<Self> {
        // SAFETY: NULL selects the process-lifetime default allocator.
        unsafe { Self::new_inner(std::ptr::null(), max_staged_bytes) }
    }
}

impl<'alloc> FeedDecoder<'alloc> {
    /// Create a decoder using a custom allocator.
    pub fn new_with_alloc<'ctx: 'alloc>(
        alloc: &'alloc Allocator<'ctx>,
        max_staged_bytes: usize,
    ) -> Result<Self> {
        // SAFETY: The returned decoder cannot outlive the borrowed allocator.
        unsafe { Self::new_inner(alloc.to_raw(), max_staged_bytes) }
    }

    unsafe fn new_inner(alloc: *const ffi::Allocator, max_staged_bytes: usize) -> Result<Self> {
        Ok(Self {
            core: unsafe { FeedCore::new_inner(alloc, max_staged_bytes)? },
        })
    }

    /// Append one bounded fragment to the not-yet-decoded READY prefix.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<&mut Self> {
        self.core.feed(bytes)?;
        Ok(self)
    }

    /// Current number of bytes waiting for the next decoding operation.
    #[must_use]
    pub fn staged_bytes(&self) -> usize {
        self.core.source.bytes.len()
    }

    /// Allocated capacity retained by the staging buffer.
    #[must_use]
    pub fn staged_capacity_bytes(&self) -> usize {
        self.core.source.bytes.capacity()
    }

    /// Set the largest accepted non-ground parser continuation.
    pub fn set_max_continuation_bytes(&mut self, value: usize) -> Result<&mut Self> {
        let result = unsafe {
            ffi::ghostty_snapshot_decoder_set(
                self.core.inner.as_raw(),
                Opt::MAX_CONTINUATION_BYTES,
                std::ptr::from_ref(&value).cast(),
            )
        };
        from_result(result)?;
        Ok(self)
    }

    /// Decode the staged prefix through its authenticated READY record.
    pub fn ready<'cb>(mut self) -> Result<FeedIncrementalDecoder<'alloc, 'cb>> {
        let mut raw: ffi::Terminal = std::ptr::null_mut();
        let result =
            unsafe { ffi::ghostty_snapshot_decoder_ready(self.core.inner.as_raw(), &raw mut raw) };
        if let Err(error) = self.core.complete_operation(result) {
            // A successful native READY with trailing staged bytes has already
            // transferred a terminal even though the safe framing contract
            // rejects the unit. Reclaim it before dropping the decoder.
            if !raw.is_null() {
                unsafe { ffi::ghostty_terminal_free(raw) };
            }
            return Err(error);
        }
        let terminal = unsafe { Terminal::from_raw(raw)? };
        Ok(FeedIncrementalDecoder {
            core: self.core,
            ready_terminal: terminal.id,
            terminal,
            finished: false,
        })
    }
}

/// Progress from one complete staged history page or image unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedProgress {
    /// Whether this unit restored a history page or image pixels.
    pub kind: ProgressKind,
    /// Screen associated with the decoded page or image payload.
    pub screen: Screen,
    /// Rows authenticated from the staged PAGE header.
    ///
    /// This is the wire outcome callers should compare with protocol metadata.
    /// It is independent of local history limits and remains nonzero when the
    /// authenticated page is intentionally discarded by local projection.
    /// Image payloads report zero rows; inspect [`Self::kind`] to distinguish
    /// them from pages that were consumed but not retained.
    pub authenticated_rows: usize,
    /// Rows retained in the live terminal, or zero when local limits discard
    /// the authenticated page.
    pub rows: usize,
    /// Pages remaining in this screen's HISTORY sequence, or zero for images.
    pub remaining: u32,
}

/// A READY terminal plus an owned staged source for history records.
#[derive(Debug)]
pub struct FeedIncrementalDecoder<'alloc, 'cb> {
    // Drop order is explicit in FeedCore and into_terminal.
    core: FeedCore<'alloc>,
    terminal: Terminal<'alloc, 'cb>,
    // Initialized at READY before exposing terminal_mut. Never compare raw
    // addresses: allocators may immediately reuse a freed terminal's address.
    ready_terminal: u64,
    finished: bool,
}

impl<'alloc: 'cb, 'cb> FeedIncrementalDecoder<'alloc, 'cb> {
    /// Append bytes belonging to the next complete history unit or FINISH.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<&mut Self> {
        if self.finished {
            return Err(Error::InvalidValue);
        }
        self.core.feed(bytes)?;
        Ok(self)
    }

    /// Current number of bytes waiting for [`Self::next`].
    #[must_use]
    pub fn staged_bytes(&self) -> usize {
        self.core.source.bytes.len()
    }

    /// Allocated capacity retained by the staging buffer.
    #[must_use]
    pub fn staged_capacity_bytes(&self) -> usize {
        self.core.source.bytes.capacity()
    }

    /// Decode one complete staged page or image unit, or authenticate FINISH.
    ///
    /// A PAGE unit may include preceding HISTORY manifests and its immediately
    /// following `KITTY_IMAGE_PAGE_PLACEMENTS` record. An image payload is a
    /// separate unit, not a page. Unknown optional records may precede a unit.
    /// `Ok(None)` is returned only after native validation of FINISH.
    #[expect(
        clippy::should_implement_trait,
        reason = "decoding is fallible and mutates the separately accessible live terminal"
    )]
    pub fn next(&mut self) -> Result<Option<FeedProgress>> {
        self.next_inner(None)
    }

    /// Decode one unit only when its staged PAGE has `expected_rows`.
    ///
    /// PAGE dimensions and complete framing are checked before native applies
    /// the page. FINISH requires zero expected rows. A mismatch releases the
    /// staged allocation, poisons this decoder, and leaves the terminal intact.
    /// Image payloads require zero expected rows and report their own kind.
    /// Successful PAGE results report the same count in
    /// [`FeedProgress::authenticated_rows`], while [`FeedProgress::rows`] may
    /// be zero when local history limits decline to retain those rows.
    pub fn next_checked(&mut self, expected_rows: usize) -> Result<Option<FeedProgress>> {
        self.next_inner(Some(expected_rows))
    }

    fn next_inner(&mut self, expected_rows: Option<usize>) -> Result<Option<FeedProgress>> {
        if self.finished || self.core.failed {
            return Err(Error::InvalidValue);
        }
        if self.ready_terminal != self.terminal.id {
            return Err(Error::InvalidValue);
        }
        let kind = self.validate_staged(expected_rows)?;
        let placements = matches!(
            kind,
            StagedHistoryKind::Page {
                placements: true,
                ..
            }
        );
        // Unlike normal next(), this call does not speculatively read the next
        // record header past the explicitly staged unit.
        let result = unsafe {
            ffi::ghostty_snapshot_decoder_next_staged(self.core.inner.as_raw(), placements)
        };
        if result == ffi::Result::NO_VALUE {
            return self.complete_finish(kind);
        }
        self.core.complete_operation(result)?;
        let authenticated_rows = match kind {
            StagedHistoryKind::Page { rows, .. } => rows,
            StagedHistoryKind::Image => 0,
            StagedHistoryKind::Finish => {
                self.core.failed = true;
                return Err(Error::InvalidValue);
            }
        };
        let progress = self.progress(authenticated_rows)?;
        let matches_wire = matches!(
            (kind, progress.kind),
            (StagedHistoryKind::Page { .. }, ProgressKind::Page { .. })
                | (StagedHistoryKind::Image, ProgressKind::Image { .. })
        );
        if !matches_wire {
            self.core.failed = true;
            return Err(Error::InvalidValue);
        }
        Ok(Some(progress))
    }

    fn validate_staged(&mut self, expected_rows: Option<usize>) -> Result<StagedHistoryKind> {
        let Ok(kind) = validate_staged_history_unit(&self.core.source.bytes) else {
            return self.reject_staged(Error::InvalidValue);
        };
        let mismatched = expected_rows.is_some_and(|expected| match kind {
            StagedHistoryKind::Page { rows, .. } => rows != expected,
            StagedHistoryKind::Finish | StagedHistoryKind::Image => expected != 0,
        });
        if mismatched {
            return self.reject_staged(Error::InvalidValue);
        }
        let bytes = &self.core.source.bytes;
        // SAFETY: The complete bounded unit remains borrowed for this
        // synchronous, allocation-free check. Validate ALL record CRCs before
        // applying a PAGE whose placement/unknown suffix might be corrupt.
        let result = unsafe { ffi::ghostty_snapshot_validate_records(bytes.as_ptr(), bytes.len()) };
        if let Err(error) = from_result(result) {
            return self.reject_staged(error);
        }
        Ok(kind)
    }

    fn complete_finish(&mut self, kind: StagedHistoryKind) -> Result<Option<FeedProgress>> {
        let consumed_all = self.core.source.finish_operation();
        if !consumed_all || kind != StagedHistoryKind::Finish {
            self.core.failed = true;
            return Err(Error::InvalidValue);
        }
        self.finished = true;
        Ok(None)
    }

    fn progress_kind(&self) -> Result<ProgressKind> {
        match self
            .core
            .get::<ffi::SnapshotDecoderProgressKind::Type>(Data::PROGRESS_KIND)?
        {
            ffi::SnapshotDecoderProgressKind::PAGE => Ok(ProgressKind::Page {
                rows: self.core.get(Data::PROGRESS_ROWS)?,
                remaining: self.core.get(Data::PROGRESS_REMAINING)?,
            }),
            ffi::SnapshotDecoderProgressKind::IMAGE => Ok(ProgressKind::Image {
                id: self.core.get(Data::PROGRESS_IMAGE_ID)?,
                frame: self.core.get(Data::PROGRESS_IMAGE_FRAME)?,
                bytes: self.core.get(Data::PROGRESS_IMAGE_BYTES)?,
            }),
            _ => Err(Error::Unsupported),
        }
    }

    fn progress(&self, authenticated_rows: usize) -> Result<FeedProgress> {
        let kind = self.progress_kind()?;
        let (rows, remaining) = match kind {
            ProgressKind::Page { rows, remaining } => (rows, remaining),
            _ => (0, 0),
        };
        Ok(FeedProgress {
            kind,
            screen: self
                .core
                .get::<ffi::TerminalScreen::Type>(Data::PROGRESS_SCREEN)?
                .try_into()
                .map_err(|_| Error::InvalidValue)?,
            authenticated_rows,
            rows,
            remaining,
        })
    }

    fn reject_staged<T>(&mut self, error: Error) -> Result<T> {
        // Rejection occurs before native reads, so offset is necessarily zero.
        let _ = self.core.source.finish_operation();
        self.core.failed = true;
        Err(error)
    }

    /// Borrow the renderable terminal while history is arriving.
    #[must_use]
    pub const fn terminal(&self) -> &Terminal<'alloc, 'cb> {
        &self.terminal
    }

    /// Mutably borrow the terminal for live PTY writes between history units.
    pub fn terminal_mut(&mut self) -> &mut Terminal<'alloc, 'cb> {
        &mut self.terminal
    }

    /// Stop history decoding and take ownership of the live terminal.
    #[must_use]
    pub fn into_terminal(self) -> Terminal<'alloc, 'cb> {
        let Self { core, terminal, .. } = self;
        drop(core);
        terminal
    }
}

#[cfg(all(test, miri))]
mod miri_soundness {
    use super::*;

    #[test]
    fn retained_reader_context_survives_moves_and_short_reborrows() {
        let context = CallbackContext::new(FeedReader::new(32).unwrap());
        // SAFETY: Context owns the raw base, and all callbacks below are
        // synchronous and finish before the context is dropped.
        let reader = unsafe { crate::io::to_reader_raw(context.as_ptr()) };
        let mut owner = Some(context);
        for _ in 0..3 {
            let mut moved = owner.take().unwrap();
            moved.feed(b"data").unwrap();
            let mut buffer = MaybeUninit::<[u8; 8]>::uninit();
            let mut read = 0;
            // SAFETY: Reader receives writable, possibly uninitialized bytes
            // and a valid out pointer, matching native's callback contract.
            assert!(unsafe {
                reader.read.unwrap()(
                    reader.userdata,
                    buffer.as_mut_ptr().cast(),
                    8,
                    &raw mut read,
                )
            });
            assert_eq!(read, 4);
            // SAFETY: The adapter initializes the complete destination slice.
            assert_eq!(unsafe { buffer.assume_init() }, *b"data\0\0\0\0");
            assert!(moved.finish_operation());
            owner = Some(moved);
        }
    }
}

#[cfg(all(test, not(miri)))]
mod feed_tests {
    use super::*;

    /// Split an upstream one-shot stream at the same boundaries the staged
    /// adapter promises. PAGE placements belong to that page, not the next one.
    fn staged_units(bytes: &[u8]) -> (usize, Vec<Vec<u8>>) {
        let ready = Decoder::new_buf(bytes).unwrap().ready().unwrap();
        let prefix = ready.decoder().source_offset().unwrap();
        let mut offset = prefix;
        let mut start = offset;
        let mut units = Vec::new();
        while offset < bytes.len() {
            let (tag, _, mut end) = staged_record(bytes, offset).unwrap();
            if tag == RECORD_TAG_HISTORY {
                offset = end;
                continue;
            }
            if tag == RECORD_TAG_PAGE && end < bytes.len() {
                let (following, _, following_end) = staged_record(bytes, end).unwrap();
                if following == RECORD_TAG_PLACEMENTS {
                    end = following_end;
                }
            }
            units.push(bytes[start..end].to_vec());
            offset = end;
            start = end;
        }
        (prefix, units)
    }

    #[cfg(feature = "kitty-graphics")]
    fn source_with_images() -> Terminal<'static, 'static> {
        let mut source = Terminal::new(200, 3).unwrap();
        source.set_scrollback_max_bytes(Some(usize::MAX)).unwrap();
        source.set_scrollback_max_lines(Some(5000)).unwrap();
        // The first placement ends up pinned to a history page. The second is
        // active at READY; both images' pixels arrive as separate progress.
        source.vt_write(b"\x1b_Ga=T,f=32,s=2,v=1,i=7,q=2;AAAA/wAAAP8=\x1b\\");
        source.vt_write(&b"history\r\n".repeat(1200));
        source.vt_write(b"\x1b_Ga=T,f=32,s=2,v=1,i=8,q=2;AAAA/wAAAP8=\x1b\\");
        source
    }

    #[test]
    #[cfg(feature = "kitty-graphics")]
    fn staged_feed_handles_image_payloads_and_page_placements() {
        let mut source = source_with_images();
        let mut bytes = Vec::new();
        source.encode_snapshot(&mut bytes).unwrap();
        let (prefix, units) = staged_units(&bytes);
        assert!(units.iter().any(|unit| matches!(
            validate_staged_history_unit(unit).unwrap(),
            StagedHistoryKind::Page {
                placements: true,
                ..
            }
        )));
        let mut feed = FeedDecoder::new(bytes.len()).unwrap();
        for byte in bytes[..prefix].chunks(1) {
            feed.feed(byte).unwrap();
        }
        let mut feed = feed.ready().unwrap();
        let mut pages = 0;
        let mut images = Vec::new();
        let mut finished = false;
        for unit in units {
            let expected = match validate_staged_history_unit(&unit).unwrap() {
                StagedHistoryKind::Page { rows, .. } => rows,
                _ => 0,
            };
            for byte in unit.chunks(1) {
                feed.feed(byte).unwrap();
            }
            match feed.next_checked(expected).unwrap() {
                Some(FeedProgress {
                    kind: ProgressKind::Page { .. },
                    ..
                }) => pages += 1,
                Some(FeedProgress {
                    kind: ProgressKind::Image { id, bytes, .. },
                    authenticated_rows,
                    ..
                }) => {
                    assert_eq!(authenticated_rows, 0);
                    images.push((id, bytes));
                }
                None => finished = true,
            }
            assert_eq!(feed.staged_capacity_bytes(), 0);
        }
        assert!(finished);
        assert!(pages > 1);
        images.sort_unstable();
        assert_eq!(images, [(7, 8), (8, 8)]);
        let graphics = feed.terminal().kitty_graphics().unwrap().unwrap();
        assert!(graphics.image(7).unwrap().data().unwrap().is_some());
        assert!(graphics.image(8).unwrap().data().unwrap().is_some());
    }

    #[cfg(feature = "kitty-graphics")]
    fn optional_record() -> Vec<u8> {
        // Unknown tag 0x4000, one byte 'x', CRC32C 0x766a52e8.
        vec![0, 64, 1, 0, 0, 0, 232, 82, 106, 118, b'x']
    }

    #[cfg(feature = "kitty-graphics")]
    fn corrupt_page_suffix(unit: &mut Vec<u8>, unknown: bool) {
        if !unknown {
            *unit.last_mut().unwrap() ^= 1;
            return;
        }
        let mut offset = 0;
        loop {
            let (tag, _, end) = staged_record(unit, offset).unwrap();
            if tag == RECORD_TAG_PAGE {
                let mut record = optional_record();
                *record.last_mut().unwrap() ^= 1;
                unit.splice(end..end, record);
                return;
            }
            offset = end;
        }
    }

    #[test]
    #[cfg(feature = "kitty-graphics")]
    fn corrupt_placements_or_unknown_suffix_do_not_apply_the_page() {
        for unknown in [false, true] {
            let mut source = source_with_images();
            let mut bytes = Vec::new();
            source.encode_snapshot(&mut bytes).unwrap();
            let (prefix, units) = staged_units(&bytes);
            let mut feed = FeedDecoder::new(bytes.len() + 64).unwrap();
            feed.feed(&bytes[..prefix]).unwrap();
            let mut feed = feed.ready().unwrap();
            let mut checked = false;
            for mut unit in units {
                if matches!(
                    validate_staged_history_unit(&unit).unwrap(),
                    StagedHistoryKind::Page {
                        placements: true,
                        ..
                    }
                ) {
                    let before = feed.terminal().scrollback_rows().unwrap();
                    corrupt_page_suffix(&mut unit, unknown);
                    feed.feed(&unit).unwrap();
                    assert!(matches!(feed.next(), Err(Error::InvalidValue)));
                    assert_eq!(feed.terminal().scrollback_rows().unwrap(), before);
                    assert_eq!(feed.staged_capacity_bytes(), 0);
                    checked = true;
                    break;
                }
                feed.feed(&unit).unwrap();
                feed.next().unwrap();
            }
            assert!(checked, "fixture must exercise an actual placement suffix");
        }
    }

    #[test]
    #[cfg(feature = "kitty-graphics")]
    fn valid_unknown_records_are_checked_and_skipped_at_unit_boundaries() {
        let mut source = source_with_images();
        let mut bytes = Vec::new();
        source.encode_snapshot(&mut bytes).unwrap();
        let (prefix, units) = staged_units(&bytes);
        let mut feed = FeedDecoder::new(bytes.len() + 64).unwrap();
        feed.feed(&bytes[..prefix]).unwrap();
        let mut feed = feed.ready().unwrap();
        let mut placement_suffix = false;
        let mut finished = false;
        for mut unit in units {
            if matches!(
                validate_staged_history_unit(&unit).unwrap(),
                StagedHistoryKind::Page {
                    placements: true,
                    ..
                }
            ) {
                let mut offset = 0;
                loop {
                    let (tag, _, end) = staged_record(&unit, offset).unwrap();
                    if tag == RECORD_TAG_PAGE {
                        unit.splice(end..end, optional_record());
                        placement_suffix = true;
                        break;
                    }
                    offset = end;
                }
            }
            // Unknown optional records are legal before image, PAGE and FINISH
            // units too; they must not be mistaken for extra progress steps.
            feed.feed(&optional_record()).unwrap();
            feed.feed(&unit).unwrap();
            finished = feed.next().unwrap().is_none();
        }
        assert!(placement_suffix);
        assert!(finished);
    }

    #[test]
    fn staged_feed_refuses_swapped_terminal_without_consuming_input() {
        let mut source = source_terminal();
        let captured = capture(&mut source).unwrap();
        let mut feed = FeedDecoder::new(captured.bytes.len()).unwrap();
        feed.feed(&captured.ready).unwrap();
        let mut feed = feed.ready().unwrap();
        feed.feed(&captured.history[0].0).unwrap();
        let original = std::mem::replace(feed.terminal_mut(), Terminal::new(200, 3).unwrap());
        assert!(matches!(feed.next(), Err(Error::InvalidValue)));
        assert_eq!(feed.staged_bytes(), captured.history[0].0.len());
        *feed.terminal_mut() = original;
        assert!(feed.next().unwrap().is_some());
    }

    #[test]
    fn staged_feed_validates_history_after_live_reset_or_resize() {
        for reset in [false, true] {
            let mut source = source_terminal();
            let captured = capture(&mut source).unwrap();
            let mut feed = FeedDecoder::new(captured.bytes.len()).unwrap();
            feed.feed(&captured.ready).unwrap();
            let mut feed = feed.ready().unwrap();
            if reset {
                feed.terminal_mut().reset();
            } else {
                feed.terminal_mut().resize(201, 3, 0, 0).unwrap();
            }
            for (unit, _, rows, _) in captured.history {
                feed.feed(&unit).unwrap();
                let progress = feed.next_checked(rows).unwrap().unwrap();
                assert_eq!(progress.authenticated_rows, rows);
                if reset {
                    // A reset need not replace the screen's identity. Keep
                    // upstream's decision to retain or drop a compatible page.
                    assert!(progress.rows == 0 || progress.rows == rows);
                } else {
                    assert_eq!(progress.rows, 0);
                }
            }
            feed.feed(&captured.finish).unwrap();
            assert!(feed.next_checked(0).unwrap().is_none());
        }
    }

    #[test]
    fn staged_feed_routes_primary_history_with_alternate_screen_active() {
        let mut source = source_terminal();
        source.vt_write(b"\x1b[?1049hAlternate");
        let mut bytes = Vec::new();
        source.encode_snapshot(&mut bytes).unwrap();
        let (prefix, units) = staged_units(&bytes);
        let mut feed = FeedDecoder::new(bytes.len()).unwrap();
        feed.feed(&bytes[..prefix]).unwrap();
        let mut feed = feed.ready().unwrap();
        let mut pages = 0;
        for unit in units {
            feed.feed(&unit).unwrap();
            if let Some(progress) = feed.next().unwrap() {
                assert_eq!(progress.screen, Screen::Primary);
                pages += 1;
            }
        }
        assert!(pages > 1);
    }

    #[test]
    fn staged_feed_rejects_corrupt_page_without_retaining_it() {
        let mut source = source_terminal();
        let captured = capture(&mut source).unwrap();
        let mut feed = FeedDecoder::new(captured.bytes.len()).unwrap();
        feed.feed(&captured.ready).unwrap();
        let mut feed = feed.ready().unwrap();
        let before = feed.terminal().scrollback_rows().unwrap();
        let mut unit = captured.history[0].0.clone();
        *unit.last_mut().unwrap() ^= 1;
        feed.feed(&unit).unwrap();
        assert!(feed.next().is_err());
        assert_eq!(feed.terminal().scrollback_rows().unwrap(), before);
        assert_eq!(feed.staged_capacity_bytes(), 0);
        assert!(matches!(feed.feed(&unit), Err(Error::InvalidValue)));
    }

    #[test]
    fn capture_does_not_change_compressed_history_representation() {
        let mut source = source_terminal();
        source
            .compress(crate::terminal::CompressionMode::Full)
            .unwrap();
        let before = source.memory_usage().unwrap();
        let captured = capture(&mut source).unwrap();
        let after = source.memory_usage().unwrap();
        assert_eq!(
            after.primary_compressed_pages,
            before.primary_compressed_pages
        );
        let restored = Decoder::new_buf(&captured.bytes).unwrap().decode().unwrap();
        assert_eq!(
            restored.scrollback_rows().unwrap(),
            source.scrollback_rows().unwrap()
        );
    }

    struct Captured {
        bytes: Vec<u8>,
        ready: Vec<u8>,
        history: Vec<(Vec<u8>, Screen, usize, u32)>,
        finish: Vec<u8>,
    }

    fn source_terminal() -> Terminal<'static, 'static> {
        let mut terminal = Terminal::new(200, 3).expect("terminal");
        terminal
            .set_scrollback_max_lines(Some(5000))
            .expect("scrollback limit");
        terminal
            .set_scrollback_max_bytes(Some(usize::MAX))
            .expect("unlimited scrollback bytes");
        for row in 0..3000 {
            terminal.vt_write(format!("row {row:03}\r\n").as_bytes());
        }
        assert!(terminal.scrollback_rows().expect("scrollback rows") > 1000);
        terminal
    }

    /// Use upstream's writer and decoder to prepare independent feed fixtures.
    fn capture(terminal: &mut Terminal<'_, '_>) -> Result<Captured> {
        let mut bytes = Vec::new();
        terminal.encode_snapshot(&mut bytes)?;
        let (prefix, mut units) = staged_units(&bytes);
        let ready = bytes[..prefix].to_vec();
        let finish = units.pop().expect("FINISH unit");
        let mut decoder = Decoder::new_buf(&bytes)?.ready()?;
        let mut history = Vec::new();
        for unit in units {
            let progress = decoder.next()?.expect("history progress");
            let ProgressKind::Page { rows, remaining } = progress.kind()? else {
                panic!("text-only fixture emitted image pixels")
            };
            history.push((unit, progress.screen()?, rows, remaining));
        }
        assert!(decoder.next()?.is_none(), "authenticated FINISH");
        drop(decoder);
        Ok(Captured {
            bytes,
            ready,
            history,
            finish,
        })
    }

    #[test]
    fn owned_feed_restores_stock_snapshot_history_after_source_mutation() {
        let mut terminal = source_terminal();
        let captured = capture(&mut terminal).expect("stock snapshot");
        assert!(
            captured.history.len() > 1,
            "snapshot should contain multiple history pages"
        );
        terminal.resize(201, 3, 0, 0).expect("source resize");
        terminal.reset();
        terminal
            .set_scrollback_max_lines(Some(1))
            .expect("source prune");
        drop(terminal);

        let mut decoder = FeedDecoder::new(captured.bytes.len()).expect("feed decoder");
        for fragment in captured.ready.chunks(1) {
            decoder.feed(fragment).expect("READY fragment");
        }
        assert!(decoder.staged_capacity_bytes() >= decoder.staged_bytes());
        let mut decoder = decoder.ready().expect("authenticated READY");
        assert_eq!(decoder.staged_bytes(), 0, "READY storage is released");
        assert_eq!(
            decoder.staged_capacity_bytes(),
            0,
            "READY capacity is released"
        );
        decoder.terminal_mut().vt_write(b"\rLIVE");

        for (unit, screen, rows, remaining) in &captured.history {
            for fragment in unit.chunks(1) {
                decoder.feed(fragment).expect("history fragment");
            }
            let progress = decoder.next().expect("valid history").expect("page");
            assert_eq!(progress.screen, *screen);
            assert_eq!(progress.authenticated_rows, *rows);
            assert_eq!(progress.rows, *rows);
            assert_eq!(progress.remaining, *remaining);
            assert_eq!(decoder.staged_bytes(), 0, "page storage is released");
            assert_eq!(
                decoder.staged_capacity_bytes(),
                0,
                "page capacity is released"
            );
        }
        decoder.feed(&captured.finish).expect("FINISH bytes");
        assert!(decoder.next().expect("authenticated FINISH").is_none());
        assert!(decoder.terminal().scrollback_rows().expect("rows") > 0);
    }

    #[test]
    fn staged_decoder_rejects_truncated_finish_instead_of_finishing() {
        let mut terminal = source_terminal();
        let captured = capture(&mut terminal).expect("capture");
        let mut decoder = FeedDecoder::new(captured.bytes.len()).expect("decoder");
        decoder.feed(&captured.ready).expect("READY");
        let mut decoder = decoder.ready().expect("ready");
        for (unit, ..) in &captured.history {
            decoder.feed(unit).expect("history");
            assert!(decoder.next().expect("page").is_some());
        }
        decoder
            .feed(&captured.finish[..captured.finish.len() - 1])
            .expect("truncated FINISH");
        assert!(matches!(decoder.next(), Err(Error::InvalidValue)));
        assert_eq!(decoder.staged_bytes(), 0, "failed staging is released");
    }

    #[test]
    fn staged_decoder_rejects_a_truncated_history_page() {
        let mut terminal = source_terminal();
        let captured = capture(&mut terminal).expect("capture");
        let mut decoder = FeedDecoder::new(captured.bytes.len()).expect("decoder");
        decoder.feed(&captured.ready).expect("READY");
        let mut decoder = decoder.ready().expect("ready");
        let first = &captured.history[0].0;
        decoder
            .feed(&first[..first.len() - 1])
            .expect("truncated history");
        assert!(matches!(decoder.next(), Err(Error::InvalidValue)));
        assert_eq!(decoder.staged_bytes(), 0, "failed staging is released");
    }

    #[test]
    fn staged_decoder_rejects_trailing_record_before_terminal_mutation() {
        let mut terminal = source_terminal();
        let captured = capture(&mut terminal).expect("capture");
        assert!(captured.history.len() > 1);
        let mut decoder = FeedDecoder::new(captured.bytes.len()).expect("decoder");
        decoder.feed(&captured.ready).expect("READY");
        let mut decoder = decoder.ready().expect("ready");
        let rows_before = decoder.terminal().scrollback_rows().expect("rows");

        decoder.feed(&captured.history[0].0).expect("first page");
        decoder.feed(&captured.history[1].0).expect("trailing page");
        assert!(matches!(decoder.next(), Err(Error::InvalidValue)));
        assert_eq!(
            decoder.terminal().scrollback_rows().expect("rows"),
            rows_before,
            "framing rejection must precede native page application"
        );
        assert_eq!(decoder.staged_capacity_bytes(), 0);
    }

    #[test]
    fn staged_decoder_rejects_row_mismatch_before_terminal_mutation() {
        let mut terminal = source_terminal();
        let captured = capture(&mut terminal).expect("capture");
        let (unit, _, rows, _) = &captured.history[0];
        let mut decoder = FeedDecoder::new(captured.bytes.len()).expect("decoder");
        decoder.feed(&captured.ready).expect("READY");
        let mut decoder = decoder.ready().expect("ready");
        let rows_before = decoder.terminal().scrollback_rows().expect("rows");

        decoder.feed(unit).expect("page");
        assert!(matches!(
            decoder.next_checked(rows + 1),
            Err(Error::InvalidValue)
        ));
        assert_eq!(
            decoder.terminal().scrollback_rows().expect("rows"),
            rows_before
        );
        assert_eq!(decoder.staged_capacity_bytes(), 0);
    }

    #[test]
    fn staged_decoder_authenticates_pages_discarded_by_local_limits() {
        let mut terminal = source_terminal();
        let captured = capture(&mut terminal).expect("capture");
        let mut decoder = FeedDecoder::new(captured.bytes.len()).expect("decoder");
        decoder.feed(&captured.ready).expect("READY");
        let mut decoder = decoder.ready().expect("ready");
        decoder
            .terminal_mut()
            .set_scrollback_max_lines(Some(1))
            .expect("local history limit");

        let mut discarded = false;
        for (unit, _, expected_rows, _) in &captured.history {
            decoder.feed(unit).expect("valid page");
            let progress = decoder
                .next_checked(*expected_rows)
                .expect("authenticated page")
                .expect("page outcome");
            assert_eq!(progress.authenticated_rows, *expected_rows);
            assert!(progress.rows == 0 || progress.rows == *expected_rows);
            discarded |= progress.rows == 0;
        }
        assert!(
            discarded,
            "the bounded projection must discard a large page"
        );

        decoder.feed(&captured.finish).expect("FINISH");
        assert!(
            decoder
                .next_checked(0)
                .expect("authenticated FINISH")
                .is_none()
        );
    }

    #[test]
    fn zero_history_still_authenticates_finish() {
        let mut terminal = Terminal::new(80, 24).expect("terminal");
        let captured = capture(&mut terminal).expect("capture");
        assert!(captured.history.is_empty());

        let mut decoder = FeedDecoder::new(captured.bytes.len()).expect("decoder");
        decoder.feed(&captured.ready).expect("READY");
        let mut decoder = decoder.ready().expect("ready");
        decoder
            .feed(&captured.finish)
            .expect("history manifests and FINISH");
        assert!(
            decoder
                .next_checked(0)
                .expect("authenticated FINISH")
                .is_none()
        );
    }
}

/// Opaque handle to a terminal snapshot decoder.
#[derive(Debug)]
pub struct Decoder<'alloc, 'r> {
    inner: Object<'alloc, ffi::SnapshotDecoderImpl>,
    _phan: PhantomData<&'r mut ffi::Reader>,
}

impl<'alloc, 'r> Decoder<'alloc, 'r> {
    /// Create a snapshot decoder that reads from a caller-provided reader.
    ///
    /// Reads are synchronous and occur only during ready, next, or decode calls.
    /// A zero-byte successful read is permanent end-of-file, not temporary
    /// starvation; nonblocking sources must wait outside the decoder or block
    /// in their callback. Reading zero bytes before a required marker
    /// reports truncated snapshot data as [`Error::InvalidValue`].
    pub fn new<R: Read>(r: &'r mut R) -> Result<Self> {
        // SAFETY: A NULL allocator is always valid
        unsafe { Self::new_inner(std::ptr::null(), r) }
    }

    /// Create a new snapshot decoder that reads from a caller-provided reader
    /// with a custom allocator.
    ///
    /// Reads are synchronous and occur only during ready, next, or decode calls.
    /// A zero-byte successful read is permanent end-of-file, not temporary
    /// starvation; nonblocking sources must wait outside the decoder or block
    /// in their callback. The read callback must not call APIs on or drop the
    /// decoder that owns it. Reading zero bytes before a required marker
    /// reports truncated snapshot data as [`Error::InvalidValue`].
    ///
    /// See the [crate-level documentation](crate#memory-management-and-lifetimes)
    /// regarding custom memory management and lifetimes.
    pub fn new_with_alloc<'ctx: 'alloc, R: Read>(
        alloc: &'alloc Allocator<'ctx>,
        r: &'r mut R,
    ) -> Result<Self> {
        // SAFETY: Borrow checking should forbid invalid allocators
        unsafe { Self::new_inner(alloc.to_raw(), r) }
    }

    unsafe fn new_inner<R: Read>(alloc: *const ffi::Allocator, r: &'r mut R) -> Result<Self> {
        let reader = crate::io::to_reader(r);
        let mut raw: ffi::SnapshotDecoder = std::ptr::null_mut();
        let result = unsafe { ffi::ghostty_snapshot_decoder_new(alloc, &raw mut raw, reader) };
        from_result(result)?;
        Ok(Self {
            inner: Object::new(raw)?,
            _phan: PhantomData,
        })
    }

    /// Create a snapshot decoder over a borrowed byte buffer.
    ///
    /// The bytes are not copied. Bytes after FINISH are not consumed;
    /// query [`Decoder::source_offset`] to locate them. To do so, decode with
    /// [`Decoder::ready`] and read it through [`IncrementalDecoder::decoder`]
    /// after FINISH.
    pub fn new_buf(buf: &'r [u8]) -> Result<Self> {
        // SAFETY: A NULL allocator is always valid
        unsafe { Self::new_buf_inner(std::ptr::null(), buf) }
    }

    /// Create a new snapshot decoder over a borrowed byte buffer
    /// with a custom allocator.
    ///
    /// The bytes are not copied. Bytes after FINISH are not consumed;
    /// query [`Decoder::source_offset`] to locate them. To do so, decode with
    /// [`Decoder::ready`] and read it through [`IncrementalDecoder::decoder`]
    /// after FINISH.
    ///
    /// See the [crate-level documentation](crate#memory-management-and-lifetimes)
    /// regarding custom memory management and lifetimes.
    pub fn new_buf_with_alloc<'ctx: 'alloc>(
        alloc: &'alloc Allocator<'ctx>,
        buf: &'r [u8],
    ) -> Result<Self> {
        // SAFETY: Borrow checking should forbid invalid allocators
        unsafe { Self::new_buf_inner(alloc.to_raw(), buf) }
    }

    unsafe fn new_buf_inner(alloc: *const ffi::Allocator, buf: &[u8]) -> Result<Self> {
        let mut raw: ffi::SnapshotDecoder = std::ptr::null_mut();
        let result = unsafe {
            ffi::ghostty_snapshot_decoder_new_buf(alloc, &raw mut raw, buf.as_ptr(), buf.len())
        };
        from_result(result)?;
        Ok(Self {
            inner: Object::new(raw)?,
            _phan: PhantomData,
        })
    }

    /// Decode and validate one complete snapshot.
    ///
    /// This is the one-shot form of READY followed by all history pages
    /// through FINISH. It may only be called before decoding starts. Bytes
    /// following FINISH are left unread. On success this returns a
    /// caller-owned terminal with its persistent VT stream restored.
    /// Continuation tracking on the returned terminal is disabled by default.
    /// When [`Self::set_retain_continuation`] is enabled, the decoder's
    /// maximum continuation size is applied to the terminal, and the terminal
    /// continuation APIs export the exact current continuation when that limit
    /// is nonzero. Tracking remains enabled even if the exported continuation
    /// is empty. Callers that do not need ongoing tracking must call
    /// [`Terminal::set_continuation_max_bytes`] with zero after export and
    /// before writing any post-snapshot bytes, because later input may change
    /// it.
    ///
    /// A decoding, I/O, or allocation error after input consumption begins
    /// poisons the decoder, after which it must be dropped. An invalid
    /// argument or lifecycle error detected before the operation consumes
    /// input does not poison it.    
    pub fn decode<'cb>(self) -> Result<Terminal<'alloc, 'cb>> {
        let mut raw: ffi::Terminal = std::ptr::null_mut();
        let result =
            unsafe { ffi::ghostty_snapshot_decoder_decode(self.inner.as_raw(), &raw mut raw) };
        from_result(result)?;
        unsafe { Terminal::from_raw(raw) }
    }

    /// Decode and validate the renderable snapshot prefix through READY.
    ///
    /// On success, terminal receives a caller-owned terminal with its
    /// persistent VT stream already restored from the snapshot continuation.
    /// The terminal is immediately usable for rendering and live input.
    /// Older scrollback remains to be restored with [`IncrementalDecoder::next`].
    ///
    /// The restored parser state may be unfinished. By default, terminal
    /// continuation tracking is disabled and
    /// [`Terminal::continuation_max_bytes`] returns zero.
    /// When [`Self::set_retain_continuation`] is enabled, the decoder's
    /// maximum continuation size is applied to the terminal, and the terminal
    /// continuation APIs export the exact current continuation when that limit
    /// is nonzero. Tracking remains enabled even if the exported continuation
    /// is empty. Callers that do not need ongoing tracking must call
    /// [`Terminal::set_continuation_max_bytes`] with zero after export and
    /// before writing any post-snapshot bytes, because later input may change
    /// it.
    ///
    /// A decoding, I/O, or allocation error after input consumption begins
    /// poisons the decoder, after which it must be dropped. An invalid
    /// argument or lifecycle error detected before the operation consumes
    /// input does not poison it.    
    pub fn ready<'cb>(self) -> Result<IncrementalDecoder<'alloc, 'r, 'cb>> {
        let mut raw: ffi::Terminal = std::ptr::null_mut();
        let result =
            unsafe { ffi::ghostty_snapshot_decoder_ready(self.inner.as_raw(), &raw mut raw) };
        from_result(result)?;
        let terminal = unsafe { Terminal::from_raw(raw)? };
        Ok(IncrementalDecoder {
            decoder: self,
            ready_terminal: terminal.id,
            finished: false,
            terminal,
        })
    }

    fn get<T>(&self, tag: Data::Type) -> Result<T> {
        let mut value = MaybeUninit::<T>::zeroed();
        let result = unsafe {
            ffi::ghostty_snapshot_decoder_get(self.inner.as_raw(), tag, value.as_mut_ptr().cast())
        };
        from_result(result)?;
        // SAFETY: Value should be initialized after successful call.
        Ok(unsafe { value.assume_init() })
    }
    fn get_optional<T>(&self, tag: Data::Type) -> Result<Option<T>> {
        let mut value = MaybeUninit::<T>::zeroed();
        let result = unsafe {
            ffi::ghostty_snapshot_decoder_get(self.inner.as_raw(), tag, value.as_mut_ptr().cast())
        };
        from_optional_result_uninit(result, value)
    }
    fn set<T>(&self, tag: Opt::Type, v: &T) -> Result<()> {
        let result = unsafe {
            ffi::ghostty_snapshot_decoder_set(
                self.inner.as_raw(),
                tag,
                std::ptr::from_ref(v).cast(),
            )
        };
        from_result(result)
    }

    /// Current maximum accepted continuation size.
    ///
    /// This value is available in every non-failed decoder state.
    pub fn max_continuation_bytes(&self) -> Result<usize> {
        self.get(Data::MAX_CONTINUATION_BYTES)
    }

    /// Largest non-ground continuation the decoder will accept.
    ///
    /// A value of zero accepts only snapshots whose VT parser is in the ground
    /// state. The decoder default matches the largest built-in APC protocol
    /// buffer limit, currently 65 MiB.
    ///
    /// This is primarily an input validation limit. When
    /// [`Self::set_retain_continuation`] is enabled, the same value also
    /// becomes the continuation tracking limit on the returned terminal.
    pub fn set_max_continuation_bytes(&mut self, v: usize) -> Result<&mut Self> {
        self.set(Opt::MAX_CONTINUATION_BYTES, &v)?;
        Ok(self)
    }

    /// Whether decoded continuation tracking is retained on returned
    /// terminals.
    ///
    /// This value is available in every non-failed decoder state.
    pub fn retain_continuation(&self) -> Result<bool> {
        self.get(Data::RETAIN_CONTINUATION)
    }

    /// Retain the decoded continuation on the returned terminal.
    ///
    /// When true, terminals returned by [`Self::ready`] and [`Self::decode`]
    /// use [`Self::max_continuation_bytes`] as their continuation tracking
    /// limit. The existing continuation APIs such as
    /// [`Terminal::continuation_buf`] can then export the exact unfinished VT
    /// or UTF-8 input restored from the snapshot.
    ///
    /// This is false by default. A maximum continuation size of zero leaves
    /// tracking disabled. With a nonzero maximum, tracking remains enabled
    /// even when the decoded continuation is empty. Exporting an empty
    /// continuation does not disable it. Callers that do not need ongoing
    /// tracking must still call [`Terminal::set_continuation_max_bytes`] with
    /// zero after export and before writing post-snapshot input.
    pub fn set_retain_continuation(&mut self, value: bool) -> Result<&mut Self> {
        self.set(Opt::RETAIN_CONTINUATION, &value)?;
        Ok(self)
    }

    /// Whether history is compressed while it is restored.
    ///
    /// See [`Self::set_compress_history_enabled`]. This value is available in
    /// every non-failed decoder state.
    pub fn is_compress_history_enabled(&self) -> Result<bool> {
        self.get(Data::COMPRESS_HISTORY)
    }

    /// Compress scrollback history while it is restored.
    ///
    /// By default, restoring a snapshot leaves all of its scrollback history
    /// uncompressed, even if the terminal that produced the snapshot had
    /// compressed it. The history stays that size until the application calls
    /// [`Terminal::compress`]. For a terminal with a lot of scrollback, that
    /// can be many times more memory than the terminal needed before.
    ///
    /// When this option is true, the decoder compresses each history page
    /// right after restoring it. The restored terminal starts out compressed,
    /// and the decode never holds more than one uncompressed history page at a
    /// time. The result is the same as decoding normally and then calling
    /// [`Terminal::compress`] with
    /// [`CompressionMode::Full`](crate::terminal::CompressionMode::Full),
    /// without the memory spike in between.
    ///
    /// A history page that is on screen when it is restored stays
    /// uncompressed. This only happens if the viewport is scrolled to the top
    /// of the scrollback during an incremental decode. Compressed history is
    /// uncompressed automatically when it is accessed later, for example by
    /// scrolling or searching.
    ///
    /// This only changes how the restored terminal stores its history in
    /// memory. The snapshot format is unchanged, so it works with any
    /// snapshot. On platforms that do not support scrollback compression, this
    /// option is accepted and has no effect.
    ///
    /// This is false by default.
    pub fn set_compress_history_enabled(&mut self, enabled: bool) -> Result<&mut Self> {
        self.set(Opt::COMPRESS_HISTORY, &enabled)?;
        Ok(self)
    }

    // The storage limit pair is gated on `kitty-graphics` like its terminal
    // counterparts in `kitty::graphics`, which the docs below link to. The
    // -sys keys exist either way, but without the feature there's no
    // `Terminal` API to read or change a restored terminal's limit, so the
    // decoder option would be the odd one out.

    /// Kitty graphics image storage limit applied to each restored screen.
    ///
    /// See [`Self::set_kitty_image_storage_limit`]. This value is available
    /// in every non-failed decoder state.
    #[cfg(feature = "kitty-graphics")]
    pub fn kitty_image_storage_limit(&self) -> Result<u64> {
        self.get(Data::KITTY_IMAGE_STORAGE_LIMIT)
    }

    /// Kitty graphics image storage limit, in bytes, for each restored
    /// screen.
    ///
    /// The limit of the terminal that wrote the snapshot is not part of the
    /// snapshot, so the decoder always applies this one. Images are restored
    /// oldest first, and an image that does not fit evicts older images,
    /// exactly as a live transmission would. The newest images are kept, and
    /// the placements of evicted images are dropped with them. Zero disables
    /// Kitty graphics on the restored terminal.
    ///
    /// The default is the storage limit of a new terminal. Change the limit
    /// of a restored terminal later with
    /// [`Terminal::set_kitty_image_storage_limit`].
    #[cfg(feature = "kitty-graphics")]
    pub fn set_kitty_image_storage_limit(&mut self, limit: u64) -> Result<&mut Self> {
        self.set(Opt::KITTY_IMAGE_STORAGE_LIMIT, &limit)?;
        Ok(self)
    }

    /// Number of snapshot source bytes consumed so far.
    ///
    /// At FINISH this identifies the first byte after the snapshot. Trailing
    /// bytes are not consumed. This value is unavailable after a decoding
    /// error, because the decoder can no longer guarantee its source position.
    pub fn source_offset(&self) -> Result<usize> {
        self.get(Data::SOURCE_OFFSET)
    }
    /// Advisory complete logical history extent for the primary screen.
    ///
    /// The value counts rows before the active area, including any resident
    /// overlap carried before READY. It becomes available after READY validates.
    pub fn history_rows_primary(&self) -> Result<u64> {
        self.get(Data::HISTORY_ROWS_PRIMARY)
    }
    /// Advisory complete logical history extent for the alternate screen.
    ///
    /// The value has the same semantics and lifetime as [`Decoder::history_rows_primary`]
    /// Querying it returns `Ok(None)` when the snapshot does not declare an
    /// alternate screen.
    pub fn history_rows_alternate(&self) -> Result<Option<u64>> {
        self.get_optional(Data::HISTORY_ROWS_ALTERNATE)
    }
}

impl Drop for Decoder<'_, '_> {
    fn drop(&mut self) {
        unsafe {
            ffi::ghostty_snapshot_decoder_free(self.inner.as_raw());
        }
    }
}

/// A [`Decoder`] that incrementally decodes history pages, and the pixels of
/// Kitty graphics images, into the terminal, obtained by calling
/// [`Decoder::ready`].
///
/// Call [`IncrementalDecoder::next`] repeatedly until `Ok(None)` is returned
/// to keep decoding history and image pixels from the snapshot.
///
/// The terminal is accessible for use during the decode process via methods
/// like [`IncrementalDecoder::terminal`] and [`IncrementalDecoder::terminal_mut`],
/// while obtaining ownership of the terminal requires halting the decode
/// process via [`IncrementalDecoder::into_terminal`].
#[derive(Debug)]
pub struct IncrementalDecoder<'alloc, 'r, 'cb> {
    // Drop order is significant here.
    // First drop the decoder, then the terminal.
    decoder: Decoder<'alloc, 'r>,
    terminal: Terminal<'alloc, 'cb>,
    // The identity of the terminal returned by READY. libghostty retains its
    // handle inside the decoder and writes history into it on every `next`
    // call, but `terminal_mut` lets safe code swap `terminal` for another one
    // (e.g. via `std::mem::replace`) and drop the original. `next` checks this
    // against `terminal` so it only lets libghostty touch the handle while we
    // own it.
    //
    // This compares `Terminal::id` rather than the handle: a replacement
    // terminal can be allocated at the freed original's address, and would
    // then receive the rest of the history.
    ready_terminal: u64,
    // Whether FINISH was reached. libghostty doesn't touch the terminal after
    // that, so the check above no longer applies.
    finished: bool,
}

impl<'alloc, 'r, 'cb> IncrementalDecoder<'alloc, 'r, 'cb> {
    /// Decode one history page, or the pixels of one Kitty graphics image,
    /// into the terminal returned by READY.
    ///
    /// Each `Ok(Some(progress))` result consumes and validates one record: a
    /// history page together with the Kitty graphics placements pinned to
    /// it, or the pixels of one image or animation frame. Pixels may arrive
    /// at any point, even between history pages. Query [`Progress::kind`]
    /// and the progress values for that kind on the returned `progress`
    /// before calling [`IncrementalDecoder::next`] again. `Ok(None)` means
    /// FINISH was validated; repeated calls after FINISH also return
    /// `Ok(None)`.
    ///
    /// ```rust
    /// use libghostty_vt::{
    ///     Terminal,
    ///     screen::Screen,
    ///     snapshot::{Decoder, ProgressKind},
    /// };
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let mut source = Terminal::new(80, 24)?;
    /// # for i in 0..5000 {
    /// #     source.vt_write(format!("line {i}\r\n").as_bytes());
    /// # }
    /// # // A 2x1 RGBA image: 2 * 1 * 4 = 8 bytes of pixels.
    /// # source.vt_write(b"\x1b_Ga=T,f=32,s=2,v=1,i=7,q=2;AAAA/wAAAP8=\x1b\\");
    /// # let mut snapshot = Vec::new();
    /// # source.encode_snapshot(&mut snapshot)?;
    /// let mut incremental = Decoder::new_buf(&snapshot)?.ready()?;
    ///
    /// // How many scrollback rows were restored, and how many image bytes the
    /// // primary screen received, to show progress while they arrive.
    /// let mut restored = 0;
    /// let mut received = 0;
    ///
    /// while let Some(progress) = incremental.next()? {
    ///     match progress.kind()? {
    ///         // Scrollback grew by this many rows.
    ///         ProgressKind::Page { rows, .. } => restored += rows,
    ///         // An image's pixels arrived.
    ///         ProgressKind::Image { bytes, .. } => {
    ///             if progress.screen()? == Screen::Primary {
    ///                 received += bytes;
    ///             }
    ///         }
    ///         // `ProgressKind` is non-exhaustive, so a later version of
    ///         // this crate may decode more kinds of records.
    ///         _ => {}
    ///     }
    /// }
    ///
    /// // `Ok(None)` means the snapshot is complete. Dropped pixels report
    /// // zero bytes, so `received` can end up below what was sent.
    /// # assert!(restored > 0);
    /// # // A library built without Kitty graphics skips the image.
    /// # #[cfg(feature = "kitty-graphics")]
    /// # assert_eq!(received, 8);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// The terminal may be rendered, resized, and fed live PTY input between
    /// calls. If a history page can no longer be applied safely, it is still
    /// consumed and validated and progress reports zero rows. Image pixels
    /// work the same way: if live input deleted the image, or sent a new
    /// image with the same ID, the pixels are consumed and validated and
    /// progress reports zero bytes. The decoder applies history and pixels to
    /// the terminal produced by its READY operation.
    ///
    /// If [`Decoder::set_compress_history_enabled`] is enabled, the page is
    /// compressed before this returns, unless it is visible in the terminal's
    /// viewport.
    ///
    /// If that terminal has been replaced through
    /// [`IncrementalDecoder::terminal_mut`] (e.g. with [`std::mem::replace`]),
    /// this returns [`Error::InvalidValue`] without consuming input. Unlike a
    /// decoding error, this doesn't invalidate the decoder: putting the READY
    /// terminal back allows decoding to continue. After FINISH, the terminal
    /// may be replaced freely.
    ///
    /// A decoding error invalidates the decoder's source position. The terminal
    /// remains usable with its already-restored history, but the decoder can
    /// only be dropped.
    pub fn next<'d>(&'d mut self) -> Result<Option<Progress<'alloc, 'r, 'd>>> {
        if self.finished {
            return Ok(None);
        }
        // libghostty applies history to the handle it retained at READY, not to
        // whatever terminal we hold now. If we no longer hold the READY
        // terminal, it may already have been freed, so calling into
        // libghostty would be a use-after-free.
        if self.terminal.id != self.ready_terminal {
            return Err(Error::InvalidValue);
        }
        let result = unsafe { ffi::ghostty_snapshot_decoder_next(self.decoder.inner.as_raw()) };
        self.finished = result == ffi::Result::NO_VALUE;
        from_optional_result(
            result,
            Progress {
                decoder: &self.decoder,
            },
        )
    }

    /// Get a reference to the underlying decoder.
    ///
    /// It can be queried between pages and after FINISH, e.g. for
    /// [`Decoder::source_offset`] and the history row counts. After a
    /// decoding error only the history row counts remain available; the other
    /// values return [`Error::InvalidValue`].
    #[must_use]
    pub fn decoder(&self) -> &Decoder<'alloc, 'r> {
        // Shared only: the setters need `&mut`, and upstream rejects options
        // once READY has run anyway.
        &self.decoder
    }
    /// Return a shared reference to the terminal being decoded.
    #[must_use]
    pub fn terminal(&self) -> &Terminal<'alloc, 'cb> {
        &self.terminal
    }
    /// Return an exclusive reference to the terminal being decoded.
    ///
    /// Replacing the terminal behind this reference makes
    /// [`IncrementalDecoder::next`] fail until the original is put back.
    pub fn terminal_mut(&mut self) -> &mut Terminal<'alloc, 'cb> {
        &mut self.terminal
    }
    /// Stop decoding and obtain the final, fully decoded terminal.
    #[must_use]
    pub fn into_terminal(self) -> Terminal<'alloc, 'cb> {
        self.terminal
    }
}

/// The current progress of the decode process: what the most recent
/// successful [`IncrementalDecoder::next`] call decoded, a history page or the
/// pixels of a Kitty graphics image.
#[derive(Debug, Clone, Copy)]
pub struct Progress<'alloc, 'r, 'd> {
    decoder: &'d Decoder<'alloc, 'r>,
}

impl<'alloc, 'r, 'd> Progress<'alloc, 'r, 'd> {
    /// Screen associated with the most recently decoded history page or
    /// image pixels.
    pub fn screen(&self) -> Result<Screen> {
        self.decoder
            .get::<ffi::TerminalScreen::Type>(Data::PROGRESS_SCREEN)
            .and_then(|v| v.try_into().map_err(|_| Error::Unsupported))
    }

    /// What the most recent successful [`IncrementalDecoder::next`] call
    /// decoded: a history page or the pixels of a Kitty graphics image.
    ///
    /// A kind these bindings don't know yet returns [`Error::InvalidValue`].
    pub fn kind(&self) -> Result<ProgressKind> {
        use ffi::SnapshotDecoderProgressKind as Kind;

        // Upstream only gives the page keys a value for a page and the image
        // keys a value for an image; the others return NO_VALUE. So read the
        // tag first, then only the keys that apply to it, the same way
        // `osc::Command::command_type` does. Every key read here has a value
        // for its kind, so plain `get` is enough: a NO_VALUE would be an
        // upstream bug, and surfaces as an error rather than a made-up zero.
        match self.decoder.get::<Kind::Type>(Data::PROGRESS_KIND)? {
            Kind::PAGE => Ok(ProgressKind::Page {
                rows: self.decoder.get(Data::PROGRESS_ROWS)?,
                remaining: self.decoder.get(Data::PROGRESS_REMAINING)?,
            }),
            Kind::IMAGE => Ok(ProgressKind::Image {
                id: self.decoder.get(Data::PROGRESS_IMAGE_ID)?,
                frame: self.decoder.get(Data::PROGRESS_IMAGE_FRAME)?,
                bytes: self.decoder.get(Data::PROGRESS_IMAGE_BYTES)?,
            }),
            _ => Err(Error::Unsupported),
        }
    }

    /// Get a reference to the underlying decoder.
    #[must_use]
    pub fn as_decoder(self) -> &'d Decoder<'alloc, 'r> {
        self.decoder
    }
}

/// What the most recent successful [`IncrementalDecoder::next`] call
/// decoded.
///
/// Query it with [`Progress::kind`]; each variant carries the progress values
/// that apply to that kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProgressKind {
    /// One history page, together with the Kitty graphics placements pinned
    /// to it. `rows` and `remaining` describe it.
    Page {
        /// Rows prepended by the most recently decoded history page.
        ///
        /// Zero means the page was consumed and validated but could not be
        /// applied to the live terminal.
        rows: usize,
        /// Page records remaining in the same screen's HISTORY sequence.
        ///
        /// This is not a count of all pages remaining in the snapshot.
        remaining: u32,
    },
    /// The pixels of one Kitty graphics image or animation frame. `id`,
    /// `frame`, and `bytes` describe it.
    Image {
        /// ID of the Kitty graphics image whose pixels the most recent
        /// [`IncrementalDecoder::next`] call decoded.
        id: u32,
        /// Which frame of the image those pixels are for, numbered as the
        /// Kitty graphics protocol numbers frames. 1 is the image itself, and
        /// 2 and up are its animation frames.
        frame: u32,
        /// Number of pixel bytes the most recent [`IncrementalDecoder::next`]
        /// call attached to its image.
        ///
        /// Zero means the pixels were consumed and validated but dropped.
        /// That happens when live input deleted the image or sent a new image
        /// with the same ID, when the image did not fit the storage limit, or
        /// when its screen was removed or replaced.
        bytes: usize,
    },
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    /// A snapshot of a terminal stopped in the middle of `ESC [31`.
    fn unfinished_snapshot() -> Vec<u8> {
        let mut terminal = Terminal::new(8, 2).unwrap();
        terminal.set_continuation_max_bytes(1024).unwrap();
        terminal.vt_write(b"\x1b[31");
        let mut bytes = Vec::new();
        terminal.encode_snapshot(&mut bytes).unwrap();
        bytes
    }

    fn continuation(terminal: &Terminal<'_, '_>) -> Option<Vec<u8>> {
        let mut buf = [0; 16];
        let len = terminal.continuation_buf(&mut buf).unwrap()?;
        Some(buf[..len].to_vec())
    }

    /// A snapshot of an 80x24 terminal with enough scrollback that some of it
    /// is encoded as HISTORY pages after READY, so that
    /// [`IncrementalDecoder::next`] has something to apply.
    fn snapshot_with_history() -> Vec<u8> {
        let mut terminal = Terminal::new(80, 24).unwrap();
        for i in 0..5000 {
            terminal.vt_write(format!("line {i}\r\n").as_bytes());
        }
        let mut bytes = Vec::new();
        terminal.encode_snapshot(&mut bytes).unwrap();
        bytes
    }

    /// Decode the rest of the history and return the rows it restored.
    ///
    /// A snapshot without Kitty images has only pages after READY, and with a
    /// single (primary) screen they form one HISTORY sequence, so `remaining`
    /// counts down by one per page and reaches zero on the last.
    fn decode_pages(incremental: &mut IncrementalDecoder<'_, '_, '_>) -> usize {
        let mut rows = 0;
        let mut last_remaining = None;
        while let Some(progress) = incremental.next().unwrap() {
            assert_eq!(progress.screen().unwrap(), Screen::Primary);
            let ProgressKind::Page {
                rows: page_rows,
                remaining,
            } = progress.kind().unwrap()
            else {
                panic!("expected only history pages");
            };
            if let Some(last) = last_remaining {
                assert_eq!(remaining + 1, last);
            }
            last_remaining = Some(remaining);
            rows += page_rows;
        }
        assert_eq!(last_remaining, Some(0));
        rows
    }

    #[test]
    fn next_restores_history() {
        let bytes = snapshot_with_history();
        let mut incremental = Decoder::new_buf(&bytes).unwrap().ready().unwrap();
        assert!(decode_pages(&mut incremental) > 0);
    }

    /// The pixels of a Kitty image arrive through `next` as their own
    /// progress step, among the history pages.
    #[test]
    #[cfg(feature = "kitty-graphics")]
    fn next_reports_kitty_image_pixels() {
        let mut terminal = Terminal::new(80, 24).unwrap();
        for i in 0..5000 {
            terminal.vt_write(format!("line {i}\r\n").as_bytes());
        }
        // A 2x1 RGBA image (f=32), placed on the active screen after the
        // history so no scrollback pruning can take it with it. Its pixels
        // are 2 * 1 * 4 = 8 bytes: two opaque black pixels. The decoder's
        // default storage limit (that of a new terminal) easily fits them.
        terminal.vt_write(b"\x1b_Ga=T,f=32,s=2,v=1,i=7,q=2;AAAA/wAAAP8=\x1b\\");
        let mut bytes = Vec::new();
        terminal.encode_snapshot(&mut bytes).unwrap();

        let mut incremental = Decoder::new_buf(&bytes).unwrap().ready().unwrap();
        // READY restores image metadata before `next` attaches the pixels.
        {
            let graphics = incremental
                .terminal()
                .kitty_graphics()
                .unwrap()
                .expect("kitty graphics enabled");
            let image = graphics.image(7).unwrap();
            assert_eq!(image.data().unwrap(), None);
            assert_eq!(image.data_len().unwrap(), 8);
        }
        let mut images = Vec::new();
        let mut pages = 0;
        while let Some(progress) = incremental.next().unwrap() {
            // Upstream only promises that pixels arrive somewhere before
            // FINISH, so don't depend on where among the pages they are.
            match progress.kind().unwrap() {
                ProgressKind::Page { .. } => pages += 1,
                image => images.push((image, progress.screen().unwrap())),
            }
        }
        assert!(pages > 0);
        assert_eq!(
            images,
            [(
                ProgressKind::Image {
                    id: 7,
                    frame: 1,
                    bytes: 8,
                },
                Screen::Primary,
            )]
        );

        // The reported bytes are the pixels that were attached to the image.
        let graphics = incremental
            .terminal()
            .kitty_graphics()
            .unwrap()
            .expect("kitty graphics enabled");
        let image = graphics.image(7).unwrap();
        assert_eq!(
            image.data().unwrap(),
            Some(&[0, 0, 0, 255, 0, 0, 0, 255][..])
        );
    }

    /// Swapping the READY terminal out of the incremental decoder and dropping
    /// it must not let [`IncrementalDecoder::next`] write into the freed
    /// terminal.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn next_after_swapping_out_the_terminal_is_not_a_use_after_free() {
        let bytes = snapshot_with_history();
        let alloc = crate::alloc::testing::Guard::allocator();

        let decoder = Decoder::new_buf_with_alloc(&alloc, &bytes).unwrap();
        let mut incremental = decoder.ready().unwrap();

        let original = std::mem::replace(
            incremental.terminal_mut(),
            Terminal::new_with_alloc(&alloc, 80, 24).unwrap(),
        );
        drop(original);

        // The READY terminal is gone, so libghostty must not be asked to
        // write history into it.
        assert!(matches!(incremental.next(), Err(Error::InvalidValue)));
    }

    #[test]
    fn terminal_may_be_replaced_after_finish() {
        let bytes = snapshot_with_history();
        let mut incremental = Decoder::new_buf(&bytes).unwrap().ready().unwrap();
        while incremental.next().unwrap().is_some() {}

        drop(std::mem::replace(
            incremental.terminal_mut(),
            Terminal::new(80, 24).unwrap(),
        ));
        assert!(incremental.next().unwrap().is_none());
    }

    #[test]
    fn next_resumes_once_the_ready_terminal_is_put_back() {
        let bytes = snapshot_with_history();
        let mut incremental = Decoder::new_buf(&bytes).unwrap().ready().unwrap();

        let original =
            std::mem::replace(incremental.terminal_mut(), Terminal::new(80, 24).unwrap());
        assert!(matches!(incremental.next(), Err(Error::InvalidValue)));

        // The rejected call consumed nothing, so decoding carries on as if the
        // swap never happened.
        *incremental.terminal_mut() = original;
        assert!(decode_pages(&mut incremental) > 0);
    }

    /// The allocator usually hands a terminal created right after the
    /// original was freed the same address. It must still be rejected,
    /// rather than receive the rest of another terminal's history.
    #[test]
    fn next_rejects_a_new_terminal_at_the_old_address() {
        let bytes = snapshot_with_history();
        let mut incremental = Decoder::new_buf(&bytes).unwrap().ready().unwrap();

        let original =
            std::mem::replace(incremental.terminal_mut(), Terminal::new(80, 24).unwrap());
        drop(original);
        *incremental.terminal_mut() = Terminal::new(80, 24).unwrap();
        assert!(matches!(incremental.next(), Err(Error::InvalidValue)));
    }

    #[test]
    fn decoded_continuation_is_not_retained_by_default() {
        let bytes = unfinished_snapshot();
        let decoder = Decoder::new_buf(&bytes).unwrap();
        assert!(!decoder.retain_continuation().unwrap());
        let restored = decoder.decode().unwrap();
        assert_eq!(restored.continuation_max_bytes().unwrap(), 0);
    }

    #[test]
    fn decoded_continuation_can_be_exported_and_resumed() {
        let bytes = unfinished_snapshot();
        let mut decoder = Decoder::new_buf(&bytes).unwrap();
        decoder
            .set_max_continuation_bytes(1024)
            .unwrap()
            .set_retain_continuation(true)
            .unwrap();
        assert!(decoder.retain_continuation().unwrap());

        let mut restored = decoder.decode().unwrap();
        assert_eq!(restored.continuation_max_bytes().unwrap(), 1024);
        assert_eq!(continuation(&restored).as_deref(), Some(&b"\x1b[31"[..]));

        // Tracking isn't needed after the export, so turn it off before
        // writing post-snapshot input. The parser state is still restored.
        restored.set_continuation_max_bytes(0).unwrap();
        restored.vt_write(b"mX");
        assert!(restored.is_vt_ground().unwrap());
        assert_eq!(restored.cursor_x().unwrap(), 1);
    }

    #[test]
    fn ready_retains_continuation_before_history_is_restored() {
        let bytes = unfinished_snapshot();
        let mut decoder = Decoder::new_buf(&bytes).unwrap();
        decoder.set_retain_continuation(true).unwrap();
        // The tracking limit is the decoder's, not the encoder's.
        let limit = decoder.max_continuation_bytes().unwrap();
        let mut incremental = decoder.ready().unwrap();
        assert_eq!(
            incremental.terminal().continuation_max_bytes().unwrap(),
            limit
        );
        assert_eq!(
            continuation(incremental.terminal()).as_deref(),
            Some(&b"\x1b[31"[..])
        );
        while incremental.next().unwrap().is_some() {}
    }

    #[test]
    fn ground_snapshot_keeps_tracking_enabled() {
        let mut terminal = Terminal::new(8, 2).unwrap();
        terminal.vt_write(b"hi");
        let mut bytes = Vec::new();
        terminal.encode_snapshot(&mut bytes).unwrap();

        let mut decoder = Decoder::new_buf(&bytes).unwrap();
        decoder
            .set_max_continuation_bytes(1024)
            .unwrap()
            .set_retain_continuation(true)
            .unwrap();
        let mut restored = decoder.decode().unwrap();

        // There was nothing unfinished to restore, but tracking is still on
        // and exports an empty continuation rather than refusing to.
        assert_eq!(restored.continuation_max_bytes().unwrap(), 1024);
        assert_eq!(continuation(&restored).as_deref(), Some(&b""[..]));

        // Exporting doesn't turn tracking off, so later input is still tracked.
        restored.vt_write(b"\x1b[");
        assert_eq!(continuation(&restored).as_deref(), Some(&b"\x1b["[..]));
    }

    #[test]
    fn zero_limit_leaves_tracking_disabled() {
        // Only snapshots at ground are accepted with a zero limit.
        let mut terminal = Terminal::new(8, 2).unwrap();
        terminal.vt_write(b"hi");
        let mut bytes = Vec::new();
        terminal.encode_snapshot(&mut bytes).unwrap();

        let mut decoder = Decoder::new_buf(&bytes).unwrap();
        decoder
            .set_max_continuation_bytes(0)
            .unwrap()
            .set_retain_continuation(true)
            .unwrap();
        let restored = decoder.decode().unwrap();
        assert_eq!(restored.continuation_max_bytes().unwrap(), 0);
    }

    /// Restoring leaves history uncompressed unless the option asks for it.
    /// This goes through the one-shot [`Decoder::decode`], which upstream runs
    /// as READY followed by every `next`, so the option applies there too.
    #[test]
    fn decode_compresses_history_when_enabled() {
        // Mirrors upstream's own test: at 215 columns a page holds 215 rows, so
        // 3 pages' worth of lines leaves several history pages above the
        // active area. The restored viewport shows the active area, so every
        // one of them is off screen and eligible. This is much cheaper than
        // `snapshot_with_history`, which formats and writes a line at a time.
        let mut terminal = Terminal::new(215, 2).unwrap();
        // Lift the default byte limit, which would otherwise prune all but
        // one of those history pages.
        terminal.set_scrollback_max_bytes(Some(usize::MAX)).unwrap();
        terminal.vt_write(&b"x\r\n".repeat(215 * 3));
        let mut bytes = Vec::new();
        terminal.encode_snapshot(&mut bytes).unwrap();

        // Left at its default, the option compresses nothing.
        let plain = Decoder::new_buf(&bytes).unwrap().decode().unwrap();
        assert_eq!(plain.memory_usage().unwrap().primary_compressed_pages, 0);

        let mut decoder = Decoder::new_buf(&bytes).unwrap();
        assert!(!decoder.is_compress_history_enabled().unwrap());
        decoder.set_compress_history_enabled(true).unwrap();
        assert!(decoder.is_compress_history_enabled().unwrap());
        // Turning it back off must stick too, not just turning it on.
        decoder.set_compress_history_enabled(false).unwrap();
        assert!(!decoder.is_compress_history_enabled().unwrap());
        decoder.set_compress_history_enabled(true).unwrap();
        let compressed = decoder.decode().unwrap().memory_usage().unwrap();
        // Upstream accepts the option where compression is unsupported, but it
        // has no effect there, so only expect compressed pages where it is.
        if compressed.compression_supported {
            assert!(compressed.primary_compressed_pages > 0);
        } else {
            assert_eq!(compressed.primary_compressed_pages, 0);
        }
    }

    /// The decoder starts with a new terminal's storage limit, not some
    /// value of its own, and whatever limit it holds is the one the restored
    /// terminal ends up with.
    #[test]
    #[cfg(feature = "kitty-graphics")]
    fn kitty_image_storage_limit_defaults_to_a_new_terminals_and_is_applied() {
        // Taken from a fresh terminal rather than the decoder, so a decoder
        // that made up its own default would fail here.
        let default = Terminal::new(8, 2)
            .unwrap()
            .kitty_image_storage_limit()
            .unwrap()
            .expect("kitty graphics enabled");
        let bytes = unfinished_snapshot();

        let mut decoder = Decoder::new_buf(&bytes).unwrap();
        assert_eq!(decoder.kitty_image_storage_limit().unwrap(), default);
        // Anything but the default, so a setter that did nothing, or a
        // restore that ignored it, can't pass by accident.
        let limit = default / 2 + 1;
        decoder.set_kitty_image_storage_limit(limit).unwrap();
        assert_eq!(decoder.kitty_image_storage_limit().unwrap(), limit);

        let restored = decoder.decode().unwrap();
        assert_eq!(restored.kitty_image_storage_limit().unwrap(), Some(limit));
    }

    /// Images are restored oldest first, so when they don't all fit within
    /// the decoder's storage limit, the newest evicts the older ones, exactly
    /// as a live transmission would. Mirrors upstream's "decode with a
    /// smaller limit keeps the newest images" test in
    /// `src/terminal/snapshot/kitty.zig`.
    ///
    /// This also checks that the limit is in force *while* images are
    /// restored, not just set on the terminal afterwards: the two orders
    /// evict different images. Upstream evicts unplaced images before placed
    /// ones, and oldest first among equals. The decoder restores every image
    /// before any placement, so while 8 is restored neither image is placed
    /// yet and the older 7 goes. Lowering the limit after the restore, with
    /// 7 placed and 8 not, would evict 8 instead.
    #[test]
    #[cfg(feature = "kitty-graphics")]
    fn kitty_image_storage_limit_keeps_the_newest_images() {
        let mut terminal = Terminal::new(80, 24).unwrap();
        // Two copies of the 2x1 RGBA image from
        // `next_reports_kitty_image_pixels`, 8 bytes each, sent oldest (7)
        // first. 7 is transmitted and placed (`a=T`), 8 only transmitted
        // (`a=t`), so that a limit applied after the restore would keep 7.
        terminal.vt_write(b"\x1b_Ga=T,f=32,s=2,v=1,i=7,q=2;AAAA/wAAAP8=\x1b\\");
        terminal.vt_write(b"\x1b_Ga=t,f=32,s=2,v=1,i=8,q=2;AAAA/wAAAP8=\x1b\\");
        let mut bytes = Vec::new();
        terminal.encode_snapshot(&mut bytes).unwrap();

        // The control: at the default limit both images survive, placed or
        // not, so the absence below is down to the limit and not the
        // snapshot.
        let kept = Decoder::new_buf(&bytes).unwrap().decode().unwrap();
        let graphics = kept
            .kitty_graphics()
            .unwrap()
            .expect("kitty graphics enabled");
        assert!(graphics.image(7).is_some());
        assert!(graphics.image(8).is_some());

        // Room for exactly one image: nonzero, so Kitty graphics stay
        // enabled, and big enough that the newer image fits on its own, so
        // losing the older one can only be eviction.
        let mut decoder = Decoder::new_buf(&bytes).unwrap();
        decoder.set_kitty_image_storage_limit(8).unwrap();
        let restored = decoder.decode().unwrap();
        let graphics = restored
            .kitty_graphics()
            .unwrap()
            .expect("kitty graphics enabled");
        assert!(graphics.image(7).is_none());
        assert!(graphics.image(8).is_some());
    }

    /// The decoder stays reachable through the whole incremental decode, so
    /// READY metadata can be read before the first page and the end of the
    /// snapshot can be located after FINISH.
    #[test]
    fn decoder_is_queryable_before_history_and_after_finish() {
        // Same cheap history-bearing terminal as
        // `decode_compresses_history_when_enabled`: several history pages
        // remain to be decoded after READY.
        let mut terminal = Terminal::new(215, 2).unwrap();
        terminal.set_scrollback_max_bytes(Some(usize::MAX)).unwrap();
        terminal.vt_write(&b"x\r\n".repeat(215 * 3));
        let history = terminal.scrollback_rows().unwrap();
        let mut buf = Vec::new();
        terminal.encode_snapshot(&mut buf).unwrap();
        // Record the length before appending, so the expected offset doesn't
        // come from the decoder itself. The trailing bytes make the end of
        // the snapshot differ from the end of the buffer.
        let snapshot_len = buf.len();
        buf.extend_from_slice(b"trailing");

        let mut incremental = Decoder::new_buf(&buf).unwrap().ready().unwrap();
        // No page has been decoded yet, so there is no `Progress` to reach the
        // decoder through.
        assert_eq!(
            incremental.decoder().history_rows_primary().unwrap(),
            history as u64
        );
        let mut pages = 0;
        while incremental.next().unwrap().is_some() {
            pages += 1;
        }
        assert!(pages > 0);

        let offset = incremental.decoder().source_offset().unwrap();
        assert_eq!(offset, snapshot_len);
        assert_eq!(&buf[offset..], b"trailing");
    }

    /// Length of a record header: u16 tag, u32 payload length, u32 CRC32C.
    const RECORD_HEADER_LEN: usize = 10;

    fn encoded_snapshot() -> Vec<u8> {
        let mut terminal = Terminal::new(20, 5).expect("terminal should initialize");
        terminal.vt_write(b"hello\r\nworld");
        let bytes = terminal
            .encode_snapshot_alloc(None)
            .expect("snapshot should encode")
            .expect("snapshot should not be empty");
        bytes.to_vec()
    }

    #[test]
    fn finish_is_an_empty_marker_record() {
        let bytes = encoded_snapshot();

        // The envelope is "GHOSTSNP" followed by the u16 format version.
        assert_eq!(&bytes[..8], b"GHOSTSNP");
        assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 1);

        // FINISH is the final record and carries no payload, so the stream
        // ends with its bare header. Its declared payload length is zero.
        let finish = &bytes[bytes.len() - RECORD_HEADER_LEN..];
        assert_eq!(&finish[2..6], &0u32.to_le_bytes());

        assert!(Decoder::new_buf(&bytes).unwrap().decode().is_ok());
    }

    #[test]
    fn truncated_snapshot_is_invalid_value() {
        let bytes = encoded_snapshot();

        // Dropping FINISH means end-of-file before the required marker, which
        // the snapshot contract documents as `Error::InvalidValue`.
        let truncated = &bytes[..bytes.len() - RECORD_HEADER_LEN];
        let result = Decoder::new_buf(truncated).unwrap().decode();
        assert!(matches!(result, Err(Error::InvalidValue)));
    }

    #[test]
    fn corrupted_record_fails_to_decode() {
        let mut bytes = encoded_snapshot();

        // Every record, including the empty FINISH marker, is protected by
        // CRC32C. Flipping a bit in FINISH's checksum must be detected.
        let mut finish_crc = bytes.clone();
        let last = finish_crc.len() - 1;
        finish_crc[last] ^= 0x01;
        assert!(Decoder::new_buf(&finish_crc).unwrap().decode().is_err());

        // Likewise for a payload byte of the first (TERMINAL) record, which
        // starts right after the ten-byte envelope and its record header.
        let payload = 10 + RECORD_HEADER_LEN;
        bytes[payload] ^= 0x01;
        assert!(Decoder::new_buf(&bytes).unwrap().decode().is_err());
    }
}
