//! Encode and restore the complete state of a terminal via a binary format.
//!
//! A snapshot is an ordered, authenticated record stream. Its READY checkpoint
//! contains enough state to render and resume the terminal, including any
//! unfinished VT parser input. Older scrollback pages follow READY and the
//! FINISH checkpoint authenticates the complete snapshot.
//!
//! End-of-file before an operation's required READY or FINISH checkpoint is
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
//! +------------- CONTINUATION ---------------+
//! | unfinished VT/UTF-8 input, or ground     |
//! +------------------ READY -----------------+
//! | BLAKE3-256 of every preceding byte       |  ready() returns here
//! +----------------- HISTORY ----------------+  repeated per screen
//! | scrollback manifest                      |
//! +------------------ PAGE ------------------+  next() consumes one page
//! | older screen rows                        |
//! +------------------ FINISH ----------------+
//! | BLAKE3-256 of every preceding byte       |  next() returns NO_VALUE
//! +------------------------------------------+
//! | trailing transport bytes (not consumed) |
//! +------------------------------------------+
//! ```
//!
//! READY authenticates the renderable prefix through CONTINUATION. FINISH
//! authenticates READY and every history record as well as the earlier prefix.
//! Thus record CRC32C detects local corruption while the BLAKE3 checkpoints
//! also bind the ordering and completeness of the record stream.
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
    ptr::NonNull,
};

const RECORD_HEADER_BYTES: usize = 10;

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
    ///produced that state was written; otherwise this returns
    /// [`Error::InvalidValue`].
    ///
    /// Encoding begins at the writer's current position. If an error occurs,
    /// the writer may contain a partial snapshot without a valid FINISH
    /// checkpoint. Calls to the writer are synchronous; this function does not
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
        let alloc = alloc.map_or(std::ptr::null(), |v| v.to_raw());

        let result = unsafe {
            ffi::ghostty_snapshot_encode_alloc(
                self.inner.as_raw(),
                alloc,
                &raw mut out,
                &raw mut out_len,
            )
        };

        let out = from_optional_result(result, out)?;
        Ok(out
            .and_then(NonNull::new)
            .map(|ptr| unsafe { Bytes::from_raw_parts(ptr, out_len, alloc) }))
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

impl<'alloc: 'cb, 'cb> Terminal<'alloc, 'cb> {
    /// Begin a bounded, record-at-a-time snapshot capture.
    ///
    /// The returned type-state wrappers retain exclusive access to the source:
    /// prefix capture freezes it through READY, while detached history capture
    /// exposes mutable access for live writes between bounded steps.
    pub fn capture_snapshot<'terminal>(
        &'terminal mut self,
        options: CaptureOptions,
    ) -> Result<Capture<'alloc, 'cb, 'terminal>> {
        Capture::new(self, options)
    }

    /// Begin a cooperative prefix capture that temporarily owns this terminal.
    ///
    /// Unlike [`Self::capture_snapshot`], this form can be stored across actor
    /// turns without a self-reference. At READY, [`OwnedCapture::detach`]
    /// returns the terminal and a source-borrow-free history capture.
    pub fn into_snapshot_capture(
        self,
        options: CaptureOptions,
    ) -> Result<OwnedCapture<'alloc, 'cb>> {
        OwnedCapture::new(self, options)
    }
}

/// Hard limits fixed for the lifetime of a progressive snapshot capture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureOptions {
    /// Maximum encoded size of any individual record.
    pub max_record_bytes: usize,
    /// Maximum number of history pages the capture may emit.
    pub max_pages: usize,
}

/// Boundary represented by a progressive capture event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureEventKind {
    /// One cold page was inspected; no bytes were emitted.
    Scan,
    /// A format envelope or ordinary record.
    Record,
    /// The authenticated renderable checkpoint.
    Ready,
    /// A history PAGE record.
    HistoryPage,
    /// The authenticated final checkpoint.
    Finish,
    /// The point-in-time history cut is no longer available.
    Invalidated,
}

/// Typed reason a detached history cut can no longer be encoded safely.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureInvalidation {
    /// A different source terminal was supplied.
    WrongTerminal,
    /// The source screen was reset.
    Reset,
    /// The source width changed and history was reflowed.
    Resize,
    /// A retained cold page changed identity or contents.
    Mutation,
    /// A retained cold page was removed by scrollback pruning.
    Evicted,
}

/// Metadata for one complete envelope or record emitted by [`Capture::next`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureEvent {
    /// One cold page descriptor was inspected without emitting bytes.
    Scan,
    /// A format envelope or non-checkpoint record was emitted.
    Record {
        /// Bytes emitted for this envelope or record.
        written: usize,
    },
    /// The authenticated renderable prefix ends at this record.
    Ready {
        /// Bytes emitted for the READY record.
        written: usize,
    },
    /// One complete history page was emitted.
    HistoryPage {
        /// Screen whose history contains this page.
        screen: Screen,
        /// Rows encoded in this page.
        rows: usize,
        /// Page records remaining in this screen's HISTORY sequence.
        remaining: u32,
        /// Bytes emitted for this record.
        written: usize,
    },
    /// The final authenticated checkpoint was emitted.
    Finish {
        /// Bytes emitted for the FINISH record.
        written: usize,
    },
    /// The detached cut was invalidated; FINISH cannot be emitted.
    Invalidated(CaptureInvalidation),
}

impl CaptureEvent {
    /// The boundary represented by this event.
    #[must_use]
    pub const fn kind(self) -> CaptureEventKind {
        match self {
            Self::Scan => CaptureEventKind::Scan,
            Self::Record { .. } => CaptureEventKind::Record,
            Self::Ready { .. } => CaptureEventKind::Ready,
            Self::HistoryPage { .. } => CaptureEventKind::HistoryPage,
            Self::Finish { .. } => CaptureEventKind::Finish,
            Self::Invalidated(_) => CaptureEventKind::Invalidated,
        }
    }

    /// Number of bytes written into the buffer supplied to [`Capture::next`].
    #[must_use]
    pub const fn written(self) -> usize {
        match self {
            Self::Scan | Self::Invalidated(_) => 0,
            Self::Record { written }
            | Self::Ready { written }
            | Self::HistoryPage { written, .. }
            | Self::Finish { written } => written,
        }
    }
}

#[derive(Debug)]
struct CaptureWriter {
    ptr: *mut u8,
    capacity: usize,
    written: usize,
}

impl CaptureWriter {
    const fn idle() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            capacity: 0,
            written: 0,
        }
    }

    fn begin(&mut self, buf: &mut [u8]) {
        self.ptr = buf.as_mut_ptr();
        self.capacity = buf.len();
        self.written = 0;
    }

    fn end(&mut self) {
        self.ptr = std::ptr::null_mut();
        self.capacity = 0;
    }
}

unsafe extern "C" fn capture_write(
    userdata: *mut std::ffi::c_void,
    data: *const u8,
    len: usize,
) -> bool {
    // SAFETY: Capture owns this boxed context, and native calls the callback
    // only synchronously from capture_next while begin() has installed a valid
    // caller buffer. The context is cleared before that borrow can end.
    let writer = unsafe { &mut *userdata.cast::<CaptureWriter>() };
    let Some(end) = writer.written.checked_add(len) else {
        return false;
    };
    if writer.ptr.is_null() || end > writer.capacity {
        return false;
    }
    // SAFETY: Native guarantees `data` covers `len` readable bytes, and the
    // capacity check above proves the destination subrange is writable.
    unsafe {
        std::ptr::copy_nonoverlapping(data, writer.ptr.add(writer.written), len);
    }
    writer.written = end;
    true
}

#[derive(Debug)]
struct CaptureCore<'alloc> {
    inner: Object<'alloc, ffi::SnapshotCaptureImpl>,
    // Native stores a pointer to this callback context. Boxing keeps its address
    // stable if either type-state wrapper moves.
    writer: Box<CaptureWriter>,
    max_record_bytes: usize,
}

impl<'alloc> CaptureCore<'alloc> {
    fn new(terminal: &mut Terminal<'alloc, '_>, options: CaptureOptions) -> Result<Self> {
        if options.max_record_bytes < RECORD_HEADER_BYTES || options.max_pages == 0 {
            return Err(Error::InvalidValue);
        }
        let mut writer = Box::new(CaptureWriter::idle());
        let raw_writer = ffi::Writer {
            userdata: std::ptr::from_mut(&mut *writer).cast(),
            write: Some(capture_write),
        };
        let raw_options = ffi::SnapshotCaptureOptions {
            size: std::mem::size_of::<ffi::SnapshotCaptureOptions>(),
            max_record_bytes: options.max_record_bytes,
            max_pages: options.max_pages,
        };
        let mut raw: ffi::SnapshotCapture = std::ptr::null_mut();
        let result = unsafe {
            ffi::ghostty_snapshot_capture_new(
                std::ptr::null(),
                terminal.inner.as_raw(),
                raw_writer,
                &raw const raw_options,
                &raw mut raw,
            )
        };
        from_result(result)?;
        Ok(Self {
            inner: Object::new(raw)?,
            writer,
            max_record_bytes: options.max_record_bytes,
        })
    }

    fn call(
        &mut self,
        buf: &mut [u8],
        operation: impl FnOnce(
            ffi::SnapshotCapture,
            *mut ffi::SnapshotCaptureEvent,
        ) -> ffi::Result::Type,
    ) -> Result<CaptureEvent> {
        if buf.len() < self.max_record_bytes {
            return Err(Error::OutOfSpace {
                required: self.max_record_bytes,
            });
        }
        self.writer.begin(buf);
        let mut raw = ffi::SnapshotCaptureEvent {
            size: std::mem::size_of::<ffi::SnapshotCaptureEvent>(),
            ..Default::default()
        };
        let result = operation(self.inner.as_raw(), &raw mut raw);
        self.writer.end();
        from_result(result)?;
        if raw.written != self.writer.written || raw.written > buf.len() {
            return Err(Error::InvalidValue);
        }
        capture_event(raw)
    }
}

impl Drop for CaptureCore<'_> {
    fn drop(&mut self) {
        // Native releases its callback pointer before the boxed writer drops.
        unsafe { ffi::ghostty_snapshot_capture_free(self.inner.as_raw()) };
    }
}

fn capture_invalidation(
    raw: ffi::SnapshotCaptureInvalidation::Type,
) -> Result<CaptureInvalidation> {
    match raw {
        ffi::SnapshotCaptureInvalidation::WRONG_TERMINAL => Ok(CaptureInvalidation::WrongTerminal),
        ffi::SnapshotCaptureInvalidation::RESET => Ok(CaptureInvalidation::Reset),
        ffi::SnapshotCaptureInvalidation::RESIZE => Ok(CaptureInvalidation::Resize),
        ffi::SnapshotCaptureInvalidation::MUTATION => Ok(CaptureInvalidation::Mutation),
        ffi::SnapshotCaptureInvalidation::EVICTED => Ok(CaptureInvalidation::Evicted),
        _ => Err(Error::InvalidValue),
    }
}

fn capture_event(raw: ffi::SnapshotCaptureEvent) -> Result<CaptureEvent> {
    match raw.kind {
        ffi::SnapshotCaptureEventKind::SCAN => Ok(CaptureEvent::Scan),
        ffi::SnapshotCaptureEventKind::RECORD => Ok(CaptureEvent::Record {
            written: raw.written,
        }),
        ffi::SnapshotCaptureEventKind::READY => Ok(CaptureEvent::Ready {
            written: raw.written,
        }),
        ffi::SnapshotCaptureEventKind::HISTORY_PAGE => Ok(CaptureEvent::HistoryPage {
            screen: raw.screen.try_into().map_err(|_| Error::InvalidValue)?,
            rows: raw.rows,
            remaining: raw.remaining,
            written: raw.written,
        }),
        ffi::SnapshotCaptureEventKind::FINISH => Ok(CaptureEvent::Finish {
            written: raw.written,
        }),
        ffi::SnapshotCaptureEventKind::INVALIDATED => Ok(CaptureEvent::Invalidated(
            capture_invalidation(raw.invalidation)?,
        )),
        _ => Err(Error::InvalidValue),
    }
}

/// A bounded snapshot prefix capture borrowing one terminal through READY.
#[derive(Debug)]
pub struct Capture<'alloc, 'cb, 'terminal> {
    core: CaptureCore<'alloc>,
    _terminal: &'terminal mut Terminal<'alloc, 'cb>,
}

impl<'alloc: 'cb, 'cb, 'terminal> Capture<'alloc, 'cb, 'terminal> {
    fn new(
        terminal: &'terminal mut Terminal<'alloc, 'cb>,
        options: CaptureOptions,
    ) -> Result<Self> {
        Ok(Self {
            core: CaptureCore::new(terminal, options)?,
            _terminal: terminal,
        })
    }

    /// Emit one complete format envelope or record into `buf`.
    ///
    /// `buf` must be at least the configured `max_record_bytes`. Prevalidating
    /// the capacity ensures native output cannot fail after partially updating
    /// the running snapshot digest.
    pub fn next(&mut self, buf: &mut [u8]) -> Result<CaptureEvent> {
        self.core.call(buf, |capture, event| unsafe {
            ffi::ghostty_snapshot_capture_next(capture, event)
        })
    }

    /// Detach the O(1)-storage history cut and release the source borrow.
    ///
    /// This is valid only immediately after [`CaptureEvent::Ready`].
    pub fn detach(self) -> Result<HistoryCapture<'alloc>> {
        let result = unsafe { ffi::ghostty_snapshot_capture_detach(self.core.inner.as_raw()) };
        from_result(result)?;
        Ok(HistoryCapture { core: self.core })
    }
}

/// A cooperative prefix capture that temporarily owns its source terminal.
///
/// Native points to the terminal's heap allocation, not this movable Rust
/// wrapper. Field order ensures capture state drops before the terminal.
#[derive(Debug)]
pub struct OwnedCapture<'alloc: 'cb, 'cb> {
    core: CaptureCore<'alloc>,
    terminal: Terminal<'alloc, 'cb>,
}

impl<'alloc: 'cb, 'cb> OwnedCapture<'alloc, 'cb> {
    fn new(mut terminal: Terminal<'alloc, 'cb>, options: CaptureOptions) -> Result<Self> {
        let core = CaptureCore::new(&mut terminal, options)?;
        Ok(Self { core, terminal })
    }

    /// Emit one complete prefix envelope or record into `buf`.
    pub fn next(&mut self, buf: &mut [u8]) -> Result<CaptureEvent> {
        self.core.call(buf, |capture, event| unsafe {
            ffi::ghostty_snapshot_capture_next(capture, event)
        })
    }

    /// At READY, release the terminal and its detached history cut.
    pub fn detach(self) -> Result<(Terminal<'alloc, 'cb>, HistoryCapture<'alloc>)> {
        let result = unsafe { ffi::ghostty_snapshot_capture_detach(self.core.inner.as_raw()) };
        from_result(result)?;
        Ok((self.terminal, HistoryCapture { core: self.core }))
    }

    /// Abort prefix capture and recover the unchanged source terminal.
    #[must_use]
    pub fn into_terminal(self) -> Terminal<'alloc, 'cb> {
        self.terminal
    }
}

/// A detached point-in-time history cut that retains no source borrow.
#[derive(Debug)]
pub struct HistoryCapture<'alloc> {
    core: CaptureCore<'alloc>,
}

impl<'alloc> HistoryCapture<'alloc> {
    /// Perform one bounded history scan or record step.
    ///
    /// `source` must be the original terminal used to create the capture.
    /// Supplying a replacement returns [`CaptureInvalidation::WrongTerminal`],
    /// even if its native allocation reused the original address.
    /// [`CaptureEvent::Scan`] inspects one page and writes no bytes. An
    /// invalidation event is a typed tombstone and means this capture can never
    /// produce FINISH. Dropping a detached capture never accesses its source.
    pub fn next(
        &mut self,
        source: &mut Terminal<'alloc, '_>,
        buf: &mut [u8],
    ) -> Result<CaptureEvent> {
        let source = source.inner.as_raw();
        self.core.call(buf, |capture, event| unsafe {
            ffi::ghostty_snapshot_capture_next_history(capture, source, event)
        })
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

fn staged_record(bytes: &[u8], offset: usize) -> Result<(u16, usize)> {
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
    Ok((tag, end))
}

/// Validate the complete framing boundary before native can mutate a terminal.
fn validate_staged_history_unit(bytes: &[u8]) -> Result<()> {
    let mut offset = 0;
    let mut histories = 0;
    loop {
        let (tag, end) = staged_record(bytes, offset)?;
        match tag {
            RECORD_TAG_HISTORY if histories < 2 => {
                histories += 1;
                offset = end;
            }
            RECORD_TAG_PAGE | RECORD_TAG_FINISH if end == bytes.len() => return Ok(()),
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
    // Native retains the callback userdata pointer. The Box keeps that address
    // stable, and Drop frees native state before the source allocation.
    source: Box<FeedReader>,
    failed: bool,
}

impl FeedCore<'_> {
    unsafe fn new_inner(alloc: *const ffi::Allocator, max_staged_bytes: usize) -> Result<Self> {
        let mut source = Box::new(FeedReader::new(max_staged_bytes)?);
        let reader = crate::io::to_reader(&mut *source);
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
        // This is the only unsafe self-reference: native holds a callback
        // pointer into `source`. Freeing it here ends that borrow before Rust
        // drops the boxed source field.
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
        Ok(FeedIncrementalDecoder {
            core: self.core,
            terminal: unsafe { Terminal::from_raw(raw)? },
            finished: false,
        })
    }
}

/// Progress from one complete staged history unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedProgress {
    /// Screen whose history received the page.
    pub screen: Screen,
    /// Rows prepended to the live terminal, or zero if it could not be applied.
    pub rows: usize,
    /// Pages remaining in this screen's HISTORY sequence.
    pub remaining: u32,
}

/// A READY terminal plus an owned staged source for history records.
#[derive(Debug)]
pub struct FeedIncrementalDecoder<'alloc, 'cb> {
    // Drop order is explicit in FeedCore and into_terminal.
    core: FeedCore<'alloc>,
    terminal: Terminal<'alloc, 'cb>,
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

    /// Decode one complete staged page unit, or authenticate staged FINISH.
    ///
    /// A unit may include a HISTORY manifest immediately preceding its PAGE.
    /// `Ok(None)` is returned only after native validation of FINISH.
    #[expect(
        clippy::should_implement_trait,
        reason = "decoding is fallible and mutates the separately accessible live terminal"
    )]
    pub fn next(&mut self) -> Result<Option<FeedProgress>> {
        if self.finished || self.core.failed {
            return Err(Error::InvalidValue);
        }
        if validate_staged_history_unit(&self.core.source.bytes).is_err() {
            self.core.source.finish_operation();
            self.core.failed = true;
            return Err(Error::InvalidValue);
        }
        let result = unsafe { ffi::ghostty_snapshot_decoder_next(self.core.inner.as_raw()) };
        if result == ffi::Result::NO_VALUE {
            let consumed_all = self.core.source.finish_operation();
            if !consumed_all {
                self.core.failed = true;
                return Err(Error::InvalidValue);
            }
            self.finished = true;
            return Ok(None);
        }
        self.core.complete_operation(result)?;
        Ok(Some(FeedProgress {
            screen: self
                .core
                .get::<ffi::TerminalScreen::Type>(Data::PROGRESS_SCREEN)?
                .try_into()
                .map_err(|_| Error::InvalidValue)?,
            rows: self.core.get(Data::PROGRESS_ROWS)?,
            remaining: self.core.get(Data::PROGRESS_REMAINING)?,
        }))
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

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_RECORD_BYTES: usize = 64 * 1024;

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
            .set_scrollback_max_bytes(None)
            .expect("unlimited scrollback bytes");
        for row in 0..3000 {
            terminal.vt_write(format!("row {row:03}\r\n").as_bytes());
        }
        assert!(terminal.scrollback_rows().expect("scrollback rows") > 1000);
        terminal
    }

    fn detached_capture(terminal: &mut Terminal<'static, 'static>) -> HistoryCapture<'static> {
        let mut capture = terminal
            .capture_snapshot(CaptureOptions {
                max_record_bytes: MAX_RECORD_BYTES,
                max_pages: 1000,
            })
            .expect("capture");
        let mut buf = vec![0; MAX_RECORD_BYTES];
        loop {
            if matches!(capture.next(&mut buf), Ok(CaptureEvent::Ready { .. })) {
                return capture.detach().expect("detach");
            }
        }
    }

    fn tombstone(
        capture: &mut HistoryCapture<'static>,
        terminal: &mut Terminal<'static, 'static>,
    ) -> CaptureInvalidation {
        let mut buf = vec![0; MAX_RECORD_BYTES];
        for _ in 0..10_000 {
            match capture.next(terminal, &mut buf).expect("capture step") {
                CaptureEvent::Invalidated(reason) => return reason,
                CaptureEvent::Scan => {}
                event => panic!("expected invalidation, got {event:?}"),
            }
        }
        panic!("capture never reached an invalidation")
    }

    fn capture(
        terminal: &mut Terminal<'_, '_>,
        max_pages: usize,
        write_after_ready: bool,
    ) -> Result<Captured> {
        let mut capture = terminal.capture_snapshot(CaptureOptions {
            max_record_bytes: MAX_RECORD_BYTES,
            max_pages,
        })?;
        let mut buf = vec![0; MAX_RECORD_BYTES];
        let mut bytes = Vec::new();
        let mut ready = Vec::new();
        let mut pending = Vec::new();
        let mut history = Vec::new();
        let finish;

        loop {
            let event = capture.next(&mut buf)?;
            let record = &buf[..event.written()];
            bytes.extend_from_slice(record);
            ready.extend_from_slice(record);
            if matches!(event, CaptureEvent::Ready { .. }) {
                break;
            }
        }

        let mut capture = capture.detach()?;
        if write_after_ready {
            // This is the canonical source terminal, not the decoded replica.
            // Unique OSC 8 values force active-page string/set capacity growth;
            // replacing that active node must not invalidate the cold cut.
            for id in 0..32 {
                terminal
                    .vt_write(format!("\x1b]8;;https://example.invalid/{id}\x1b\\x").as_bytes());
            }
            // Crossing native page boundaries also exercises ordinary append.
            for _ in 0..500 {
                terminal.vt_write(b"\rSOURCE-LIVE\r\n");
            }
        }
        loop {
            let event = capture.next(terminal, &mut buf)?;
            let record = &buf[..event.written()];
            bytes.extend_from_slice(record);
            pending.extend_from_slice(record);
            match event {
                CaptureEvent::HistoryPage {
                    screen,
                    rows,
                    remaining,
                    ..
                } => history.push((std::mem::take(&mut pending), screen, rows, remaining)),
                CaptureEvent::Finish { .. } => {
                    finish = std::mem::take(&mut pending);
                    break;
                }
                CaptureEvent::Scan | CaptureEvent::Record { .. } => {}
                CaptureEvent::Invalidated(reason) => {
                    panic!("history cut unexpectedly invalidated: {reason:?}")
                }
                CaptureEvent::Ready { .. } => unreachable!(),
            }
        }
        Ok(Captured {
            bytes,
            ready,
            history,
            finish,
        })
    }

    #[test]
    fn bounded_capture_matches_one_shot_and_owned_feed_restores_history() {
        let mut terminal = source_terminal();
        let mut eager = Vec::new();
        terminal
            .encode_snapshot(&mut eager)
            .expect("one-shot capture");
        let captured = capture(&mut terminal, 1000, true).expect("progressive capture");
        assert!(
            captured.history.len() > 1,
            "capture should contain multiple history pages"
        );

        assert_eq!(captured.bytes, eager);

        let mut decoder = FeedDecoder::new(captured.bytes.len()).expect("feed decoder");
        for fragment in captured.ready.chunks(37) {
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
            for fragment in unit.chunks(29) {
                decoder.feed(fragment).expect("history fragment");
            }
            let progress = decoder.next().expect("valid history").expect("page");
            assert_eq!(progress.screen, *screen);
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
        let captured = capture(&mut terminal, 1000, false).expect("capture");
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
        let captured = capture(&mut terminal, 1000, false).expect("capture");
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
        let captured = capture(&mut terminal, 1000, false).expect("capture");
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
    fn zero_history_still_authenticates_finish() {
        let mut terminal = Terminal::new(80, 24).expect("terminal");
        let captured = capture(&mut terminal, 1000, false).expect("capture");
        assert!(captured.history.is_empty());

        let mut decoder = FeedDecoder::new(captured.bytes.len()).expect("decoder");
        decoder.feed(&captured.ready).expect("READY");
        let mut decoder = decoder.ready().expect("ready");
        decoder
            .feed(&captured.finish)
            .expect("history manifests and FINISH");
        assert!(decoder.next().expect("authenticated FINISH").is_none());
    }

    #[test]
    fn capture_page_limit_fails_before_emitting_an_excess_page() {
        let mut terminal = source_terminal();
        let mut capture = terminal
            .capture_snapshot(CaptureOptions {
                max_record_bytes: MAX_RECORD_BYTES,
                max_pages: 1,
            })
            .expect("capture");
        let mut buf = vec![0; MAX_RECORD_BYTES];
        loop {
            if matches!(capture.next(&mut buf), Ok(CaptureEvent::Ready { .. })) {
                break;
            }
        }
        let mut capture = capture.detach().expect("detach");
        let mut pages = 0;
        loop {
            match capture.next(&mut terminal, &mut buf) {
                Ok(CaptureEvent::HistoryPage { .. }) => pages += 1,
                Err(Error::LimitExceeded) => break,
                Ok(CaptureEvent::Finish { .. }) => panic!("history unexpectedly fit one page"),
                Ok(_) => {}
                Err(error) => panic!("unexpected capture error: {error}"),
            }
        }
        assert_eq!(pages, 0, "limit is found during the bounded scan");
    }

    #[test]
    fn capture_record_limit_reports_limit_exceeded() {
        let mut terminal = source_terminal();
        let mut capture = terminal
            .capture_snapshot(CaptureOptions {
                max_record_bytes: RECORD_HEADER_BYTES,
                max_pages: 1000,
            })
            .expect("capture");
        let mut buf = vec![0; RECORD_HEADER_BYTES];
        assert!(matches!(
            capture.next(&mut buf),
            Ok(CaptureEvent::Record { .. })
        ));
        assert!(matches!(capture.next(&mut buf), Err(Error::LimitExceeded)));
    }

    #[test]
    fn detached_capture_reports_resize_reset_and_eviction_tombstones() {
        let mut resized = source_terminal();
        let mut resized_capture = detached_capture(&mut resized);
        resized.resize(201, 3, 0, 0).expect("resize");
        assert_eq!(
            tombstone(&mut resized_capture, &mut resized),
            CaptureInvalidation::Resize
        );

        let mut reset = source_terminal();
        let mut reset_capture = detached_capture(&mut reset);
        reset.reset();
        assert_eq!(
            tombstone(&mut reset_capture, &mut reset),
            CaptureInvalidation::Reset
        );

        let mut evicted = source_terminal();
        let mut evicted_capture = detached_capture(&mut evicted);
        evicted
            .set_scrollback_max_lines(Some(1))
            .expect("prune scrollback");
        assert_eq!(
            tombstone(&mut evicted_capture, &mut evicted),
            CaptureInvalidation::Evicted
        );
    }

    #[test]
    fn detached_captures_coexist_while_the_source_stays_live() {
        let mut terminal = source_terminal();
        let mut first = detached_capture(&mut terminal);
        let mut second = detached_capture(&mut terminal);
        let mut first_finished = false;
        let mut second_finished = false;
        let mut buf = vec![0; MAX_RECORD_BYTES];

        for _ in 0..10_000 {
            terminal.vt_write(b"\rCONCURRENT-LIVE\r\n");
            if !first_finished {
                match first.next(&mut terminal, &mut buf).expect("first step") {
                    CaptureEvent::Finish { .. } => first_finished = true,
                    CaptureEvent::Invalidated(reason) => {
                        panic!("first cut unexpectedly invalidated: {reason:?}")
                    }
                    _ => {}
                }
            }
            if !second_finished {
                match second.next(&mut terminal, &mut buf).expect("second step") {
                    CaptureEvent::Finish { .. } => second_finished = true,
                    CaptureEvent::Invalidated(reason) => {
                        panic!("second cut unexpectedly invalidated: {reason:?}")
                    }
                    _ => {}
                }
            }
            if first_finished && second_finished {
                return;
            }
        }
        panic!("simultaneous captures did not finish")
    }

    #[test]
    fn detached_capture_rejects_replacement_and_drops_after_source() {
        let mut original = source_terminal();
        let mut capture = detached_capture(&mut original);
        drop(original);

        let mut replacement = source_terminal();
        let mut buf = vec![0; MAX_RECORD_BYTES];
        assert_eq!(
            capture
                .next(&mut replacement, &mut buf)
                .expect("identity check"),
            CaptureEvent::Invalidated(CaptureInvalidation::WrongTerminal)
        );
        drop(replacement);
        drop(capture);

        let mut source = source_terminal();
        let capture = detached_capture(&mut source);
        drop(source);
        drop(capture);
    }

    #[test]
    fn owned_prefix_capture_returns_terminal_at_ready() {
        let terminal = source_terminal();
        let mut capture = terminal
            .into_snapshot_capture(CaptureOptions {
                max_record_bytes: MAX_RECORD_BYTES,
                max_pages: 1000,
            })
            .expect("owned capture");
        let mut buf = vec![0; MAX_RECORD_BYTES];
        loop {
            if matches!(capture.next(&mut buf), Ok(CaptureEvent::Ready { .. })) {
                break;
            }
        }
        let (mut terminal, mut history) = capture.detach().expect("detach");
        terminal.vt_write(b"\rOWNED-LIVE");
        loop {
            match history.next(&mut terminal, &mut buf).expect("history step") {
                CaptureEvent::Finish { .. } => break,
                CaptureEvent::Invalidated(reason) => {
                    panic!("owned cut unexpectedly invalidated: {reason:?}")
                }
                _ => {}
            }
        }

        let terminal = Terminal::new(80, 24).expect("terminal");
        let capture = terminal
            .into_snapshot_capture(CaptureOptions {
                max_record_bytes: MAX_RECORD_BYTES,
                max_pages: 1,
            })
            .expect("owned capture");
        let mut terminal = capture.into_terminal();
        terminal.vt_write(b"recovered");
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
    /// in their callback. Reading zero bytes before a required checkpoint
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
    /// decoder that owns it. Reading zero bytes before a required checkpoint
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
    /// query [`Decoder::source_offset`] to locate them.
    pub fn new_buf(buf: &'r [u8]) -> Result<Self> {
        // SAFETY: A NULL allocator is always valid
        unsafe { Self::new_buf_inner(std::ptr::null(), buf) }
    }

    /// Create a new snapshot decoder over a borrowed byte buffer
    /// with a custom allocator.
    ///
    /// The bytes are not copied. Bytes after FINISH are not consumed;
    /// query [`Decoder::source_offset`] to locate them.
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

    /// Decode and authenticate one complete snapshot.
    ///
    /// This is the one-shot form of READY followed by all history pages
    /// through FINISH. It may only be called before decoding starts. Bytes
    /// following FINISH are left unread. On success this returns a
    /// caller-owned terminal with its persistent VT stream restored.
    /// Continuation tracking on the returned terminal is disabled and
    /// [`Terminal::continuation_max_bytes`] returns zero.
    ///
    /// A decoding, I/O, or allocation error after input consumption begins
    /// poisons the decoder, after which it must be dropped. An invalid
    /// argument or lifecycle error detected before the operation consumes
    /// input does not poison it.    
    pub fn decode<'cb>(self) -> Result<Terminal<'alloc, 'cb>> {
        let mut raw: ffi::Terminal = std::ptr::null_mut();
        let result = unsafe { ffi::ghostty_snapshot_decoder_decode(self.inner.as_raw(), &mut raw) };
        from_result(result)?;
        unsafe { Terminal::from_raw(raw) }
    }

    /// Decode and authenticate the renderable snapshot prefix through READY.
    ///
    /// On success, terminal receives a caller-owned terminal with its
    /// persistent VT stream already restored from the snapshot continuation.
    /// The terminal is immediately usable for rendering and live input.
    /// Older scrollback remains to be restored with [`IncrementalDecoder::next`].
    ///
    /// The restored parser state may be unfinished, but terminal continuation
    /// tracking is disabled; [`Terminal::continuation_max_bytes`]
    /// returns zero. The decoder's continuation option is an input limit,
    /// not terminal runtime policy.
    ///
    /// A decoding, I/O, or allocation error after input consumption begins
    /// poisons the decoder, after which it must be dropped. An invalid
    /// argument or lifecycle error detected before the operation consumes
    /// input does not poison it.    
    pub fn ready<'cb>(self) -> Result<IncrementalDecoder<'alloc, 'r, 'cb>> {
        let mut raw: ffi::Terminal = std::ptr::null_mut();
        let result = unsafe { ffi::ghostty_snapshot_decoder_ready(self.inner.as_raw(), &mut raw) };
        from_result(result)?;
        Ok(IncrementalDecoder {
            decoder: self,
            terminal: unsafe { Terminal::from_raw(raw)? },
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
    /// This is an input validation limit only. It does not configure continuation
    /// tracking on a terminal returned by the decoder.
    pub fn set_max_continuation_bytes(&mut self, v: usize) -> Result<&mut Self> {
        self.set(Opt::MAX_CONTINUATION_BYTES, &v)?;
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

/// A [`Decoder`] that incrementally decodes history and appends it to the
/// terminal, obtained by calling [`Decoder::ready`].
///
/// Call [`IncrementalDecoder::next`] repeatedly until `Ok(None)` is returned
/// to keep decoding history from the snapshot.
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
}

impl<'alloc, 'r, 'cb> IncrementalDecoder<'alloc, 'r, 'cb> {
    /// Decode one history page into the terminal returned by READY.
    ///
    /// Each `Ok(Some(progress))` result consumes and authenticates one PAGE
    /// record. Query the values on the returned `progress` before
    /// calling [`IncrementalDecoder::next`] again.
    ///
    /// `Ok(None)` means FINISH was validated; repeated calls after FINISH
    /// also return `Ok(None)`.
    ///
    /// The terminal may be rendered, resized, and fed live PTY input between
    /// calls. If a history page can no longer be applied safely, it is still
    /// consumed and authenticated and progress reports zero rows. The decoder
    /// applies history to the terminal produced by its READY operation.
    ///
    /// A decoding error invalidates the decoder's source position. The terminal
    /// remains usable with its already-restored history, but the decoder can
    /// only be dropped.
    pub fn next<'d>(&'d mut self) -> Result<Option<Progress<'alloc, 'r, 'd>>> {
        let result = unsafe { ffi::ghostty_snapshot_decoder_next(self.decoder.inner.as_raw()) };
        from_optional_result(
            result,
            Progress {
                decoder: &self.decoder,
            },
        )
    }

    /// Return a shared reference to the terminal being decoded.
    pub fn terminal(&self) -> &Terminal<'alloc, 'cb> {
        &self.terminal
    }
    /// Return an exclusive reference to the terminal being decoded.
    pub fn terminal_mut(&mut self) -> &mut Terminal<'alloc, 'cb> {
        &mut self.terminal
    }
    /// Stop decoding and obtain the final, fully decoded terminal.
    pub fn into_terminal(self) -> Terminal<'alloc, 'cb> {
        self.terminal
    }
}

/// The current progress of the decode process.
#[derive(Debug, Clone, Copy)]
pub struct Progress<'alloc, 'r, 'd> {
    decoder: &'d Decoder<'alloc, 'r>,
}

impl<'alloc, 'r, 'd> Progress<'alloc, 'r, 'd> {
    /// Screen associated with the most recently decoded history page.
    pub fn screen(&self) -> Result<Screen> {
        self.decoder
            .get::<ffi::TerminalScreen::Type>(Data::PROGRESS_SCREEN)
            .and_then(|v| v.try_into().map_err(|_| Error::InvalidValue))
    }
    /// Rows prepended by the most recently decoded history page.
    ///
    /// Zero means the page was consumed and authenticated but could not be
    /// applied to the live terminal.
    pub fn rows(&self) -> Result<usize> {
        self.decoder.get(Data::PROGRESS_ROWS)
    }
    /// Page records remaining in the same screen's HISTORY sequence.
    ///
    /// This is not a count of all pages remaining in the snapshot.
    pub fn remaining(&self) -> Result<u32> {
        self.decoder.get(Data::PROGRESS_REMAINING)
    }

    /// Get a reference to the underlying decoder.
    pub fn as_decoder(self) -> &'d Decoder<'alloc, 'r> {
        self.decoder
    }
}
