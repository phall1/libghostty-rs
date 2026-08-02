//! Bounded, incremental terminal snapshots and authenticated history paging.
//!
//! Record and unit bytes are deliberately opaque. The safe API only moves one
//! complete native record into a caller-owned buffer; it never parses or
//! rewrites Ghostty's codec payloads.

use std::{fmt, marker::PhantomData, ptr::NonNull, rc::Rc};

use crate::{
    alloc::{Allocator, Object},
    ffi,
    terminal::Terminal,
};

const ABI_VERSION: u32 = ffi::TERMINAL_SNAPSHOT_ABI_VERSION;
/// Result type for incremental snapshot and history operations.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Exact failures reported by Ghostty's incremental snapshot ABI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// The terminal contains semantic state this codec cannot represent.
    UnsupportedFeature,
    /// The snapshot envelope version is not supported.
    UnknownVersion,
    /// Authentication or structural validation failed.
    Corruption,
    /// End of input arrived before FINISH.
    Truncated,
    /// A caller-supplied codec or history limit was exceeded.
    LimitExceeded,
    /// The engine-owned history lease is no longer live.
    Stale,
    /// Requested history was pruned after the cut was acquired.
    Pruned,
    /// The requested screen or history generation does not match.
    WrongGeneration,
    /// An operation was attempted with a different terminal.
    WrongTerminal,
    /// A checkpoint, capability, or native handle is invalid.
    InvalidHandle,
    /// The destination already has an active history import.
    ImportBusy,
    /// Allocation failed.
    OutOfMemory,
    /// The caller-owned buffer or import budget is too small.
    OutOfSpace {
        /// Exact required byte count, when the operation can report one.
        required_bytes: usize,
        /// Exact required row count, when the operation can report one.
        required_rows: usize,
    },
    /// The operation is not valid in the current state.
    InvalidState,
    /// A canonical parser continuation could not be captured or replayed.
    ContinuationUnavailable,
    /// A terminal reset invalidated the history generation.
    Reset,
    /// A terminal resize invalidated the history generation.
    Resize,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFeature => f.write_str("unsupported snapshot feature"),
            Self::UnknownVersion => f.write_str("unknown snapshot version"),
            Self::Corruption => f.write_str("corrupt snapshot or history unit"),
            Self::Truncated => f.write_str("truncated snapshot"),
            Self::LimitExceeded => f.write_str("snapshot limit exceeded"),
            Self::Stale => f.write_str("stale history lease"),
            Self::Pruned => f.write_str("history was pruned"),
            Self::WrongGeneration => f.write_str("wrong history generation"),
            Self::WrongTerminal => f.write_str("wrong terminal"),
            Self::InvalidHandle => f.write_str("invalid snapshot handle or token"),
            Self::ImportBusy => f.write_str("history import already active"),
            Self::OutOfMemory => f.write_str("out of memory"),
            Self::OutOfSpace {
                required_bytes,
                required_rows,
            } => write!(
                f,
                "out of space ({required_bytes} bytes and {required_rows} rows required)"
            ),
            Self::InvalidState => f.write_str("invalid snapshot state"),
            Self::ContinuationUnavailable => f.write_str("snapshot continuation unavailable"),
            Self::Reset => f.write_str("terminal reset invalidated history"),
            Self::Resize => f.write_str("terminal resize invalidated history"),
        }
    }
}

impl std::error::Error for Error {}

fn from_status(code: ffi::TerminalSnapshotStatus::Type, bytes: usize, rows: usize) -> Result<()> {
    use ffi::TerminalSnapshotStatus as Status;
    match code {
        Status::SUCCESS => Ok(()),
        Status::UNSUPPORTED_FEATURE => Err(Error::UnsupportedFeature),
        Status::UNKNOWN_VERSION => Err(Error::UnknownVersion),
        Status::CORRUPTION => Err(Error::Corruption),
        Status::TRUNCATED => Err(Error::Truncated),
        Status::LIMIT_EXCEEDED => Err(Error::LimitExceeded),
        Status::STALE => Err(Error::Stale),
        Status::PRUNED => Err(Error::Pruned),
        Status::WRONG_GENERATION => Err(Error::WrongGeneration),
        Status::WRONG_TERMINAL => Err(Error::WrongTerminal),
        Status::INVALID_HANDLE => Err(Error::InvalidHandle),
        Status::IMPORT_BUSY => Err(Error::ImportBusy),
        Status::OUT_OF_MEMORY => Err(Error::OutOfMemory),
        Status::OUT_OF_SPACE => Err(Error::OutOfSpace {
            required_bytes: bytes,
            required_rows: rows,
        }),
        Status::INVALID_STATE => Err(Error::InvalidState),
        Status::CONTINUATION_UNAVAILABLE => Err(Error::ContinuationUnavailable),
        Status::RESET => Err(Error::Reset),
        Status::RESIZE => Err(Error::Resize),
        _ => Err(Error::InvalidState),
    }
}

/// A stable screen identity understood by the incremental history ABI.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(transparent)]
pub struct ScreenKey(u16);

impl ScreenKey {
    /// Primary screen history.
    pub const PRIMARY: Self = Self(0);
    /// Alternate screen history.
    pub const ALTERNATE: Self = Self(1);
}

/// Opaque authenticated digest for one READY history cut.
#[allow(
    missing_copy_implementations,
    reason = "checkpoint capabilities are intentionally nonduplicable and single-thread affine"
)]
pub struct CheckpointToken {
    raw: ffi::TerminalHistoryToken,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl CheckpointToken {
    fn new(raw: ffi::TerminalHistoryToken) -> Self {
        Self {
            raw,
            _not_send_or_sync: PhantomData,
        }
    }
}

impl fmt::Debug for CheckpointToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CheckpointToken(<opaque>)")
    }
}

/// Opaque engine capability authorizing one cursor or importer.
#[allow(
    missing_copy_implementations,
    reason = "engine capabilities are intentionally nonduplicable and single-thread affine"
)]
pub struct CapabilityToken {
    // Preserve the exact native capability without exposing serialization.
    _raw: ffi::TerminalHistoryToken,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl CapabilityToken {
    fn new(raw: ffi::TerminalHistoryToken) -> Self {
        Self {
            _raw: raw,
            _not_send_or_sync: PhantomData,
        }
    }
}

impl fmt::Debug for CapabilityToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CapabilityToken(<opaque>)")
    }
}

/// Compatibility, identity, and hard limits of the linked incremental codec.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Capabilities {
    /// Incremental ABI version.
    pub version: u32,
    /// Lowest accepted snapshot envelope version.
    pub min_decode_version: u16,
    /// Highest accepted snapshot envelope version.
    pub max_decode_version: u16,
    /// Envelope version emitted by capture.
    pub default_encode_version: u16,
    /// Incremental capture and decode are implemented.
    pub incremental: bool,
    /// An authenticated READY boundary is available.
    pub ready: bool,
    /// Snapshot history records are available.
    pub history: bool,
    /// History checkpoint and capability tokens are authenticated.
    pub authenticated_tokens: bool,
    /// Capture records have caller-selected strict bounds.
    pub bounded_records: bool,
    /// Snapshot pages have caller-selected strict bounds.
    pub bounded_pages: bool,
    /// Live history units have caller-selected strict bounds.
    pub bounded_units: bool,
    /// Greatest accepted record-byte bound.
    pub max_record_bytes: usize,
    /// Greatest accepted page-count bound.
    pub max_pages: usize,
    /// Greatest accepted history-unit byte bound.
    pub max_unit_bytes: usize,
    /// Greatest accepted history-unit row bound.
    pub max_rows: usize,
    /// Stable codec identity.
    pub codec_identity: &'static str,
    /// Identity of the linked Ghostty build.
    pub build_identity: &'static str,
}

/// Query incremental codec capabilities from the linked Ghostty library.
pub fn capabilities() -> Result<Capabilities> {
    let mut raw = ffi::TerminalSnapshotIncrementalCapabilities {
        size: std::mem::size_of::<ffi::TerminalSnapshotIncrementalCapabilities>(),
        version: ABI_VERSION,
        ..Default::default()
    };
    from_status(
        unsafe { ffi::ghostty_terminal_snapshot_incremental_capabilities(&raw mut raw) },
        0,
        0,
    )?;
    Ok(Capabilities {
        version: raw.version,
        min_decode_version: raw.min_decode_version,
        max_decode_version: raw.max_decode_version,
        default_encode_version: raw.default_encode_version,
        incremental: raw.incremental,
        ready: raw.ready,
        history: raw.history,
        authenticated_tokens: raw.authenticated_tokens,
        bounded_records: raw.bounded_records,
        bounded_pages: raw.bounded_pages,
        bounded_units: raw.bounded_units,
        max_record_bytes: raw.max_record_bytes,
        max_pages: raw.max_pages,
        max_unit_bytes: raw.max_unit_bytes,
        max_rows: raw.max_rows,
        codec_identity: library_str(raw.codec_identity)?,
        build_identity: library_str(raw.build_identity)?,
    })
}

fn library_str(raw: ffi::String) -> Result<&'static str> {
    if raw.ptr.is_null() && raw.len != 0 {
        return Err(Error::InvalidHandle);
    }
    let bytes = if raw.len == 0 {
        &[]
    } else {
        // SAFETY: Capability identity strings are immutable library-owned data
        // whose lifetime is the linked library's process lifetime.
        unsafe { std::slice::from_raw_parts(raw.ptr, raw.len) }
    };
    std::str::from_utf8(bytes).map_err(|_| Error::Corruption)
}

/// Strict limits for incremental capture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaptureOptions {
    /// Inclusive limit for one envelope or framed record.
    pub max_record_bytes: usize,
    /// Inclusive PAGE-record count.
    pub max_pages: usize,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            max_record_bytes: 4 * 1024 * 1024,
            max_pages: 4096,
        }
    }
}

impl CaptureOptions {
    fn raw(self) -> ffi::TerminalSnapshotCaptureOptions {
        ffi::TerminalSnapshotCaptureOptions {
            size: std::mem::size_of::<ffi::TerminalSnapshotCaptureOptions>(),
            version: ABI_VERSION,
            max_record_bytes: self.max_record_bytes,
            max_pages: self.max_pages,
        }
    }
}

/// Metadata for one complete opaque capture record.
#[derive(Debug)]
pub enum CaptureEventKind {
    /// Envelope or non-boundary state record.
    Record,
    /// Authenticated renderable boundary.
    Ready {
        /// Digest authenticating all bytes through READY.
        checkpoint: CheckpointToken,
    },
    /// Start of one screen's snapshot history pages.
    HistoryBegin {
        /// Screen generation key.
        screen: ScreenKey,
        /// Total pages for this screen.
        count: u32,
    },
    /// One snapshot history page.
    HistoryPage {
        /// Screen generation key.
        screen: ScreenKey,
        /// Zero-based page index.
        index: u32,
        /// Total pages for this screen.
        count: u32,
    },
    /// Authenticated end of exactly one snapshot.
    Finish,
}

/// One complete capture record written directly into the caller's buffer.
#[derive(Debug)]
pub struct CaptureEvent<'buffer> {
    /// Typed record metadata.
    pub kind: CaptureEventKind,
    /// Snapshot envelope version emitted by this capture.
    pub codec_version: u16,
    /// Complete opaque record bytes.
    pub record: &'buffer [u8],
}

/// RAII incremental capture borrowing its terminal against mutation.
#[derive(Debug)]
pub struct Capture<'terminal, 'alloc> {
    inner: Object<'alloc, ffi::TerminalSnapshotCaptureImpl>,
    active: bool,
    _terminal: PhantomData<&'terminal mut ()>,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl<'terminal, 'alloc> Capture<'terminal, 'alloc> {
    /// Emit exactly one opaque envelope or record into `buffer`.
    ///
    /// [`Error::OutOfSpace`] reports the exact required size and does not
    /// advance capture, so retrying with a sufficiently large buffer observes
    /// the same event and bytes.
    pub fn next<'buffer>(&mut self, buffer: &'buffer mut [u8]) -> Result<CaptureEvent<'buffer>> {
        let mut raw = ffi::TerminalSnapshotCaptureEvent {
            size: std::mem::size_of::<ffi::TerminalSnapshotCaptureEvent>(),
            version: ABI_VERSION,
            ..Default::default()
        };
        let status = unsafe {
            ffi::ghostty_terminal_snapshot_capture_next(
                self.inner.as_raw(),
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut raw,
            )
        };
        from_status(status, raw.required_bytes, 0)?;
        if raw.written > buffer.len() || raw.required_bytes != raw.written {
            return Err(Error::InvalidState);
        }
        let kind = match raw.kind {
            ffi::TerminalSnapshotCaptureEventKind::RECORD => CaptureEventKind::Record,
            ffi::TerminalSnapshotCaptureEventKind::READY => CaptureEventKind::Ready {
                checkpoint: CheckpointToken::new(raw.checkpoint),
            },
            ffi::TerminalSnapshotCaptureEventKind::HISTORY_BEGIN => {
                CaptureEventKind::HistoryBegin {
                    screen: ScreenKey(raw.screen_key),
                    count: raw.count,
                }
            }
            ffi::TerminalSnapshotCaptureEventKind::HISTORY_PAGE => CaptureEventKind::HistoryPage {
                screen: ScreenKey(raw.screen_key),
                index: raw.index,
                count: raw.count,
            },
            ffi::TerminalSnapshotCaptureEventKind::FINISH => CaptureEventKind::Finish,
            _ => return Err(Error::InvalidState),
        };
        Ok(CaptureEvent {
            kind,
            codec_version: raw.codec_version,
            record: &buffer[..raw.written],
        })
    }

    /// Abort capture and release its terminal borrow.
    pub fn abort(mut self) -> Result<()> {
        self.active = false;
        from_status(
            unsafe { ffi::ghostty_terminal_snapshot_capture_abort(self.inner.as_raw()) },
            0,
            0,
        )
    }
}

impl Drop for Capture<'_, '_> {
    fn drop(&mut self) {
        if self.active {
            let _ = unsafe { ffi::ghostty_terminal_snapshot_capture_abort(self.inner.as_raw()) };
        }
        unsafe { ffi::ghostty_terminal_snapshot_capture_free(self.inner.as_raw()) };
    }
}

impl<'terminal_alloc: 'cb, 'cb> Terminal<'terminal_alloc, 'cb> {
    /// Begin bounded incremental capture using Ghostty's default allocator.
    ///
    /// ```compile_fail
    /// use libghostty_vt::{Terminal, TerminalOptions};
    /// use libghostty_vt::snapshot::incremental::CaptureOptions;
    ///
    /// let mut terminal = Terminal::new(TerminalOptions {
    ///     cols: 80, rows: 24, max_scrollback: 100,
    /// }).unwrap();
    /// let mut capture = terminal.capture(CaptureOptions::default()).unwrap();
    /// terminal.vt_write(b"mutation while capture is live");
    /// let _ = capture.next(&mut []);
    /// ```
    pub fn capture<'terminal>(
        &'terminal mut self,
        options: CaptureOptions,
    ) -> Result<Capture<'terminal, 'static>> {
        unsafe { Capture::new_inner(std::ptr::null(), self.inner.as_raw(), options) }
    }

    /// Begin bounded incremental capture using `allocator` for capture state.
    pub fn capture_with_alloc<'terminal, 'capture_alloc, 'ctx: 'capture_alloc>(
        &'terminal mut self,
        allocator: &'capture_alloc Allocator<'ctx>,
        options: CaptureOptions,
    ) -> Result<Capture<'terminal, 'capture_alloc>> {
        unsafe { Capture::new_inner(allocator.to_raw(), self.inner.as_raw(), options) }
    }

    /// Acquire an authenticated, generation-bound history cut.
    ///
    /// ```compile_fail
    /// use libghostty_vt::{Terminal, TerminalOptions};
    /// use libghostty_vt::snapshot::incremental::ScreenKey;
    ///
    /// let mut terminal = Terminal::new(TerminalOptions {
    ///     cols: 80, rows: 24, max_scrollback: 100,
    /// }).unwrap();
    /// let lease = terminal.history_lease(ScreenKey::PRIMARY).unwrap();
    /// terminal.reset();
    /// let _cursor = lease.into_cursor().unwrap();
    /// ```
    pub fn history_lease<'terminal>(
        &'terminal mut self,
        screen: ScreenKey,
    ) -> Result<HistoryLease<'terminal, 'static>> {
        unsafe { HistoryLease::new_inner(std::ptr::null(), self.inner.as_raw(), screen) }
    }

    /// Acquire a history cut using `allocator` for lease and cursor state.
    pub fn history_lease_with_alloc<'terminal, 'lease_alloc, 'ctx: 'lease_alloc>(
        &'terminal mut self,
        allocator: &'lease_alloc Allocator<'ctx>,
        screen: ScreenKey,
    ) -> Result<HistoryLease<'terminal, 'lease_alloc>> {
        unsafe { HistoryLease::new_inner(allocator.to_raw(), self.inner.as_raw(), screen) }
    }
}

impl<'terminal, 'alloc> Capture<'terminal, 'alloc> {
    unsafe fn new_inner(
        allocator: *const ffi::Allocator,
        terminal: ffi::Terminal,
        options: CaptureOptions,
    ) -> Result<Self> {
        let mut raw = std::ptr::null_mut();
        let raw_options = options.raw();
        let status = unsafe {
            ffi::ghostty_terminal_snapshot_capture_new(
                allocator,
                terminal,
                &raw const raw_options,
                &raw mut raw,
            )
        };
        if let Err(error) = from_status(status, 0, 0) {
            if !raw.is_null() {
                unsafe { ffi::ghostty_terminal_snapshot_capture_free(raw) };
            }
            return Err(error);
        }
        Ok(Self {
            inner: Object::new(raw).map_err(|_| Error::OutOfMemory)?,
            active: true,
            _terminal: PhantomData,
            _not_send_or_sync: PhantomData,
        })
    }
}

/// Strict limits for a fragmented decoder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecoderOptions {
    /// Greatest retained parser-continuation byte count.
    pub max_continuation_bytes: usize,
    /// Inclusive limit for one envelope or framed record.
    pub max_record_bytes: usize,
    /// Inclusive PAGE-record count.
    pub max_pages: usize,
}

impl Default for DecoderOptions {
    fn default() -> Self {
        Self {
            max_continuation_bytes: 1024 * 1024,
            max_record_bytes: 4 * 1024 * 1024,
            max_pages: 4096,
        }
    }
}

impl DecoderOptions {
    fn raw(self) -> ffi::TerminalSnapshotDecoderOptions {
        ffi::TerminalSnapshotDecoderOptions {
            size: std::mem::size_of::<ffi::TerminalSnapshotDecoderOptions>(),
            version: ABI_VERSION,
            max_continuation_bytes: self.max_continuation_bytes,
            max_record_bytes: self.max_record_bytes,
            max_pages: self.max_pages,
        }
    }
}

#[derive(Debug)]
struct DecoderCore<'alloc> {
    inner: Object<'alloc, ffi::TerminalSnapshotDecoderImpl>,
    active: bool,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl Drop for DecoderCore<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = unsafe { ffi::ghostty_terminal_snapshot_decoder_abort(self.inner.as_raw()) };
        }
        unsafe { ffi::ghostty_terminal_snapshot_decoder_free(self.inner.as_raw()) };
    }
}

/// Decoder before the authenticated READY boundary.
#[derive(Debug)]
pub struct Decoder<'alloc> {
    core: DecoderCore<'alloc>,
}

impl Decoder<'static> {
    /// Create a fragmented decoder using Ghostty's default allocator.
    pub fn new(options: DecoderOptions) -> Result<Self> {
        unsafe { Self::new_inner(std::ptr::null(), options) }
    }
}

impl<'alloc> Decoder<'alloc> {
    /// Create a fragmented decoder using a custom allocator.
    pub fn new_with_alloc<'ctx: 'alloc>(
        allocator: &'alloc Allocator<'ctx>,
        options: DecoderOptions,
    ) -> Result<Self> {
        unsafe { Self::new_inner(allocator.to_raw(), options) }
    }

    unsafe fn new_inner(allocator: *const ffi::Allocator, options: DecoderOptions) -> Result<Self> {
        let mut raw = std::ptr::null_mut();
        let raw_options = options.raw();
        let status = unsafe {
            ffi::ghostty_terminal_snapshot_decoder_new(
                allocator,
                &raw const raw_options,
                &raw mut raw,
            )
        };
        if let Err(error) = from_status(status, 0, 0) {
            if !raw.is_null() {
                unsafe { ffi::ghostty_terminal_snapshot_decoder_free(raw) };
            }
            return Err(error);
        }
        Ok(Self {
            core: DecoderCore {
                inner: Object::new(raw).map_err(|_| Error::OutOfMemory)?,
                active: true,
                _not_send_or_sync: PhantomData,
            },
        })
    }

    /// Consume an arbitrary fragment and perform at most one bounded transition.
    pub fn push(self, data: &[u8]) -> std::result::Result<DecodeStep<'alloc>, DecodeFailure> {
        let mut raw = decode_event();
        let status = unsafe {
            ffi::ghostty_terminal_snapshot_decoder_push(
                self.core.inner.as_raw(),
                data.as_ptr(),
                data.len(),
                &raw mut raw,
            )
        };
        if let Err(error) = from_status(status, 0, 0) {
            return Err(DecodeFailure {
                error,
                consumed: raw.consumed,
            });
        }
        if raw.consumed > data.len() {
            return Err(DecodeFailure {
                error: Error::InvalidState,
                consumed: raw.consumed,
            });
        }
        let progress = DecodeProgress::from(raw);
        match raw.kind {
            ffi::TerminalSnapshotDecodeEventKind::NEED_INPUT => Ok(DecodeStep::NeedInput {
                decoder: self,
                progress,
            }),
            ffi::TerminalSnapshotDecodeEventKind::PROGRESS => Ok(DecodeStep::Progress {
                decoder: self,
                progress,
            }),
            ffi::TerminalSnapshotDecodeEventKind::READY => Ok(DecodeStep::Ready {
                decoder: ReadyDecoder {
                    core: self.core,
                    codec_version: raw.codec_version,
                },
                progress,
            }),
            _ => Err(DecodeFailure {
                error: Error::InvalidState,
                consumed: raw.consumed,
            }),
        }
    }

    /// Mark end of input. Before FINISH this returns [`Error::Truncated`].
    pub fn end_input(self) -> std::result::Result<DecodeStep<'alloc>, DecodeFailure> {
        self.push(&[])
    }
}

fn decode_event() -> ffi::TerminalSnapshotDecodeEvent {
    ffi::TerminalSnapshotDecodeEvent {
        size: std::mem::size_of::<ffi::TerminalSnapshotDecodeEvent>(),
        version: ABI_VERSION,
        ..Default::default()
    }
}

/// Common accounting for one decoder transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeProgress {
    /// Bytes consumed from the submitted fragment.
    pub consumed: usize,
    /// Additional bytes currently needed for the buffered record.
    pub needed: usize,
    /// Snapshot envelope version, once known.
    pub codec_version: u16,
}

impl From<ffi::TerminalSnapshotDecodeEvent> for DecodeProgress {
    fn from(raw: ffi::TerminalSnapshotDecodeEvent) -> Self {
        Self {
            consumed: raw.consumed,
            needed: raw.needed,
            codec_version: raw.codec_version,
        }
    }
}

/// A terminal-free decode failure with exact fragment consumption.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeFailure {
    /// Exact native error.
    pub error: Error,
    /// Bytes consumed from the fragment before failure.
    pub consumed: usize,
}

impl fmt::Display for DecodeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} after consuming {} bytes", self.error, self.consumed)
    }
}

impl std::error::Error for DecodeFailure {}

/// Typed transition before READY.
#[derive(Debug)]
pub enum DecodeStep<'alloc> {
    /// More bytes are required to complete the current record.
    NeedInput {
        /// Decoder for the unconsumed suffix or next fragment.
        decoder: Decoder<'alloc>,
        /// Exact transition accounting.
        progress: DecodeProgress,
    },
    /// One bounded non-READY transition completed.
    Progress {
        /// Decoder for the unconsumed suffix or next fragment.
        decoder: Decoder<'alloc>,
        /// Exact transition accounting.
        progress: DecodeProgress,
    },
    /// The authenticated renderable terminal is available.
    Ready {
        /// One-shot READY terminal transfer state.
        decoder: ReadyDecoder<'alloc>,
        /// Exact transition accounting.
        progress: DecodeProgress,
    },
}

/// Decoder positioned exactly at READY, before terminal transfer.
#[derive(Debug)]
pub struct ReadyDecoder<'alloc> {
    core: DecoderCore<'alloc>,
    codec_version: u16,
}

impl<'alloc> ReadyDecoder<'alloc> {
    /// Transfer the READY terminal exactly once.
    pub fn take_terminal<'cb>(self) -> Result<ContinuationDecoder<'alloc, 'cb>>
    where
        'alloc: 'cb,
    {
        let mut raw = ffi::TerminalSnapshotTakeTerminalResult {
            size: std::mem::size_of::<ffi::TerminalSnapshotTakeTerminalResult>(),
            version: ABI_VERSION,
            ..Default::default()
        };
        let status = unsafe {
            ffi::ghostty_terminal_snapshot_decoder_take_terminal(
                self.core.inner.as_raw(),
                &raw mut raw,
            )
        };
        if let Err(error) = from_status(status, 0, 0) {
            if !raw.terminal.is_null() {
                unsafe { ffi::ghostty_terminal_free(raw.terminal) };
            }
            return Err(error);
        }
        if raw.terminal.is_null() || raw.codec_version != self.codec_version {
            if !raw.terminal.is_null() {
                unsafe { ffi::ghostty_terminal_free(raw.terminal) };
            }
            return Err(Error::InvalidState);
        }
        let terminal: Terminal<'alloc, 'cb> = match unsafe { Terminal::from_raw(raw.terminal) } {
            Ok(terminal) => terminal,
            Err(_) => {
                unsafe { ffi::ghostty_terminal_free(raw.terminal) };
                return Err(Error::InvalidHandle);
            }
        };
        Ok(ContinuationDecoder {
            core: self.core,
            terminal,
            codec_version: raw.codec_version,
        })
    }
}

/// READY terminal after one-way transfer and before continuation replay.
#[derive(Debug)]
pub struct ContinuationDecoder<'alloc: 'cb, 'cb> {
    core: DecoderCore<'alloc>,
    terminal: Terminal<'alloc, 'cb>,
    codec_version: u16,
}

impl<'alloc: 'cb, 'cb> ContinuationDecoder<'alloc, 'cb> {
    /// Replay the authenticated parser continuation exactly once.
    pub fn replay(
        self,
    ) -> std::result::Result<DecodedStream<'alloc, 'cb>, ContinuationFailure<'alloc, 'cb>> {
        let status = unsafe {
            ffi::ghostty_terminal_snapshot_decoder_replay_continuation(
                self.core.inner.as_raw(),
                self.terminal.inner.as_raw(),
            )
        };
        if let Err(error) = from_status(status, 0, 0) {
            return Err(ContinuationFailure {
                error,
                decoder: self,
            });
        }
        Ok(DecodedStream {
            core: self.core,
            terminal: self.terminal,
            codec_version: self.codec_version,
        })
    }
}

/// Failed continuation replay retaining the complete retry-safe state.
///
/// The terminal is intentionally not exposed while its authenticated parser
/// continuation is pending. Retry with [`ContinuationDecoder::replay`] through
/// [`ContinuationFailure::decoder`].
#[derive(Debug)]
pub struct ContinuationFailure<'alloc: 'cb, 'cb> {
    /// Exact native error.
    pub error: Error,
    /// Decoder and READY terminal, unchanged when replay is retry-safe.
    pub decoder: ContinuationDecoder<'alloc, 'cb>,
}

impl fmt::Display for ContinuationFailure<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for ContinuationFailure<'_, '_> {}

/// Decoder after READY transfer and continuation replay.
///
/// The owned terminal is live and may accept VT writes between later history
/// transitions through [`DecodedStream::terminal_mut`].
#[derive(Debug)]
pub struct DecodedStream<'alloc: 'cb, 'cb> {
    core: DecoderCore<'alloc>,
    terminal: Terminal<'alloc, 'cb>,
    codec_version: u16,
}

impl<'alloc: 'cb, 'cb> DecodedStream<'alloc, 'cb> {
    /// Borrow the live decoded terminal.
    pub fn terminal(&self) -> &Terminal<'alloc, 'cb> {
        &self.terminal
    }

    /// Mutably borrow the terminal for serialized live VT writes.
    pub fn terminal_mut(&mut self) -> &mut Terminal<'alloc, 'cb> {
        &mut self.terminal
    }

    /// Consume an arbitrary fragment after READY.
    pub fn push(
        self,
        data: &[u8],
    ) -> std::result::Result<AfterReadyStep<'alloc, 'cb>, DecodedFailure<'alloc, 'cb>> {
        let mut raw = decode_event();
        let status = unsafe {
            ffi::ghostty_terminal_snapshot_decoder_push(
                self.core.inner.as_raw(),
                data.as_ptr(),
                data.len(),
                &raw mut raw,
            )
        };
        if let Err(error) = from_status(status, 0, 0) {
            return Err(DecodedFailure::new(error, raw.consumed, self));
        }
        if raw.consumed > data.len() {
            return Err(DecodedFailure::new(Error::InvalidState, raw.consumed, self));
        }
        let progress = DecodeProgress::from(raw);
        match raw.kind {
            ffi::TerminalSnapshotDecodeEventKind::NEED_INPUT => Ok(AfterReadyStep::NeedInput {
                decoder: self,
                progress,
            }),
            ffi::TerminalSnapshotDecodeEventKind::PROGRESS => Ok(AfterReadyStep::Progress {
                decoder: self,
                progress,
            }),
            ffi::TerminalSnapshotDecodeEventKind::HISTORY_BEGIN => {
                Ok(AfterReadyStep::HistoryBegin {
                    decoder: self,
                    progress,
                    screen: ScreenKey(raw.screen_key),
                    count: raw.count,
                })
            }
            ffi::TerminalSnapshotDecodeEventKind::HISTORY_PAGE => Ok(AfterReadyStep::HistoryPage {
                decoder: self,
                progress,
                screen: ScreenKey(raw.screen_key),
                index: raw.index,
                count: raw.count,
                retained: raw.retained,
            }),
            ffi::TerminalSnapshotDecodeEventKind::FINISH => {
                let DecodedStream {
                    mut core,
                    terminal,
                    codec_version,
                } = self;
                core.active = false;
                drop(core);
                Ok(AfterReadyStep::Finish(FinishedSnapshot {
                    terminal,
                    progress,
                    codec_version,
                }))
            }
            _ => Err(DecodedFailure::new(Error::InvalidState, raw.consumed, self)),
        }
    }

    /// Mark end of input. Before FINISH this returns [`Error::Truncated`].
    pub fn end_input(
        self,
    ) -> std::result::Result<AfterReadyStep<'alloc, 'cb>, DecodedFailure<'alloc, 'cb>> {
        self.push(&[])
    }
}

/// A post-READY decode failure retaining the usable terminal and consumption.
#[derive(Debug)]
pub struct DecodedFailure<'alloc: 'cb, 'cb> {
    /// Exact native error.
    pub error: Error,
    /// Bytes consumed from the fragment before failure.
    pub consumed: usize,
    /// READY terminal, with its continuation already replayed.
    pub terminal: Terminal<'alloc, 'cb>,
}

impl<'alloc: 'cb, 'cb> DecodedFailure<'alloc, 'cb> {
    fn new(error: Error, consumed: usize, stream: DecodedStream<'alloc, 'cb>) -> Self {
        let DecodedStream { core, terminal, .. } = stream;
        drop(core);
        Self {
            error,
            consumed,
            terminal,
        }
    }
}

impl fmt::Display for DecodedFailure<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} after consuming {} bytes", self.error, self.consumed)
    }
}

impl std::error::Error for DecodedFailure<'_, '_> {}

/// Typed transition after READY.
#[derive(Debug)]
pub enum AfterReadyStep<'alloc: 'cb, 'cb> {
    /// More bytes are required for the current record.
    NeedInput {
        /// Decoder retaining the live terminal.
        decoder: DecodedStream<'alloc, 'cb>,
        /// Exact transition accounting.
        progress: DecodeProgress,
    },
    /// One bounded non-history transition completed.
    Progress {
        /// Decoder retaining the live terminal.
        decoder: DecodedStream<'alloc, 'cb>,
        /// Exact transition accounting.
        progress: DecodeProgress,
    },
    /// Start of one screen's history pages.
    HistoryBegin {
        /// Decoder retaining the live terminal.
        decoder: DecodedStream<'alloc, 'cb>,
        /// Exact transition accounting.
        progress: DecodeProgress,
        /// Screen generation key.
        screen: ScreenKey,
        /// Total page count.
        count: u32,
    },
    /// One history page was authenticated and considered for retention.
    HistoryPage {
        /// Decoder retaining the live terminal.
        decoder: DecodedStream<'alloc, 'cb>,
        /// Exact transition accounting.
        progress: DecodeProgress,
        /// Screen generation key.
        screen: ScreenKey,
        /// Zero-based page index.
        index: u32,
        /// Total page count.
        count: u32,
        /// Whether the bounded decoder retained the page.
        retained: bool,
    },
    /// Exactly one authenticated snapshot reached FINISH.
    Finish(FinishedSnapshot<'alloc, 'cb>),
}

/// Terminal and accounting returned at authenticated FINISH.
#[derive(Debug)]
pub struct FinishedSnapshot<'alloc: 'cb, 'cb> {
    /// Fully decoded live terminal.
    pub terminal: Terminal<'alloc, 'cb>,
    /// Accounting for the FINISH transition only.
    pub progress: DecodeProgress,
    /// Authenticated snapshot envelope version.
    pub codec_version: u16,
}

/// Strict budgets for one history cursor or import operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryOptions {
    /// Maximum bytes for each unit.
    pub max_unit_bytes: usize,
    /// Maximum rows for each unit.
    pub max_rows: usize,
    /// Maximum imported units; used only at importer construction.
    pub max_units: usize,
}

impl Default for HistoryOptions {
    fn default() -> Self {
        Self {
            max_unit_bytes: 256 * 1024,
            max_rows: 32,
            max_units: 4096,
        }
    }
}

impl HistoryOptions {
    fn raw(self) -> ffi::TerminalHistoryOptions {
        ffi::TerminalHistoryOptions {
            size: std::mem::size_of::<ffi::TerminalHistoryOptions>(),
            version: ABI_VERSION,
            max_unit_bytes: self.max_unit_bytes,
            max_rows: self.max_rows,
            max_units: self.max_units,
        }
    }
}

#[derive(Debug)]
struct LeaseInner<'terminal, 'alloc> {
    handle: Object<'alloc, ffi::TerminalHistoryLeaseImpl>,
    source: NonNull<ffi::TerminalImpl>,
    screen: ScreenKey,
    checkpoint: CheckpointToken,
    _terminal: PhantomData<&'terminal ()>,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl Drop for LeaseInner<'_, '_> {
    fn drop(&mut self) {
        unsafe { ffi::ghostty_terminal_history_lease_free(self.handle.as_raw()) };
    }
}

/// RAII generation-bound history lease borrowing its source against mutation.
#[derive(Debug)]
pub struct HistoryLease<'terminal, 'alloc> {
    inner: Rc<LeaseInner<'terminal, 'alloc>>,
}

impl<'terminal, 'alloc> HistoryLease<'terminal, 'alloc> {
    unsafe fn new_inner(
        allocator: *const ffi::Allocator,
        terminal: ffi::Terminal,
        screen: ScreenKey,
    ) -> Result<Self> {
        let mut raw = ffi::TerminalHistoryLeaseResult {
            size: std::mem::size_of::<ffi::TerminalHistoryLeaseResult>(),
            version: ABI_VERSION,
            ..Default::default()
        };
        let status = unsafe {
            ffi::ghostty_terminal_history_lease_new(allocator, terminal, screen.0, &raw mut raw)
        };
        if let Err(error) = from_status(status, 0, 0) {
            if !raw.lease.is_null() {
                unsafe { ffi::ghostty_terminal_history_lease_free(raw.lease) };
            }
            return Err(error);
        }
        let source = NonNull::new(terminal).ok_or(Error::WrongTerminal)?;
        Ok(Self {
            inner: Rc::new(LeaseInner {
                handle: Object::new(raw.lease).map_err(|_| Error::OutOfMemory)?,
                source,
                screen,
                checkpoint: CheckpointToken::new(raw.checkpoint),
                _terminal: PhantomData,
                _not_send_or_sync: PhantomData,
            }),
        })
    }

    /// Return the opaque authenticated checkpoint for this cut.
    pub fn checkpoint(&self) -> &CheckpointToken {
        &self.inner.checkpoint
    }

    /// Transfer this lease's newest-first cursor exactly once.
    pub fn into_cursor(self) -> Result<HistoryCursor<'terminal, 'alloc>> {
        let mut raw = ffi::TerminalHistoryCursorResult {
            size: std::mem::size_of::<ffi::TerminalHistoryCursorResult>(),
            version: ABI_VERSION,
            ..Default::default()
        };
        let status = unsafe {
            ffi::ghostty_terminal_history_lease_cursor(
                self.inner.handle.as_raw(),
                self.inner.source.as_ptr(),
                &raw mut raw,
            )
        };
        if let Err(error) = from_status(status, 0, 0) {
            if !raw.cursor.is_null() {
                unsafe { ffi::ghostty_terminal_history_cursor_free(raw.cursor) };
            }
            return Err(error);
        }
        Ok(HistoryCursor {
            inner: Object::new(raw.cursor).map_err(|_| Error::OutOfMemory)?,
            lease: Rc::clone(&self.inner),
            capability: CapabilityToken::new(raw.capability),
        })
    }
}

/// One caller-buffer history unit or the end of the newest-first cursor.
#[derive(Debug)]
pub enum HistoryEvent<'buffer> {
    /// One authenticated opaque history unit.
    Unit {
        /// Complete unit bytes in the caller's buffer.
        unit: &'buffer [u8],
        /// Rows represented by this unit.
        rows: usize,
        /// This unit completes its source page.
        page_complete: bool,
    },
    /// No older units remain.
    End,
}

/// Newest-to-oldest cursor for a live history cut.
#[derive(Debug)]
pub struct HistoryCursor<'terminal, 'alloc> {
    inner: Object<'alloc, ffi::TerminalHistoryCursorImpl>,
    lease: Rc<LeaseInner<'terminal, 'alloc>>,
    capability: CapabilityToken,
}

impl<'terminal, 'alloc> HistoryCursor<'terminal, 'alloc> {
    /// Return this cursor's opaque engine capability.
    pub fn capability(&self) -> &CapabilityToken {
        &self.capability
    }

    /// Emit one bounded unit directly into `buffer` without advancing on a
    /// short-buffer error.
    pub fn next<'buffer>(
        &mut self,
        options: HistoryOptions,
        buffer: &'buffer mut [u8],
    ) -> Result<HistoryEvent<'buffer>> {
        let mut raw = ffi::TerminalHistoryEvent {
            size: std::mem::size_of::<ffi::TerminalHistoryEvent>(),
            version: ABI_VERSION,
            ..Default::default()
        };
        let raw_options = options.raw();
        let status = unsafe {
            ffi::ghostty_terminal_history_cursor_next(
                self.inner.as_raw(),
                self.lease.source.as_ptr(),
                &raw const raw_options,
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut raw,
            )
        };
        from_status(status, raw.required_bytes, 0)?;
        match raw.kind {
            ffi::TerminalHistoryEventKind::UNIT => {
                if raw.written > buffer.len() || raw.required_bytes != raw.written {
                    return Err(Error::InvalidState);
                }
                Ok(HistoryEvent::Unit {
                    unit: &buffer[..raw.written],
                    rows: raw.rows,
                    page_complete: raw.page_complete,
                })
            }
            ffi::TerminalHistoryEventKind::END => Ok(HistoryEvent::End),
            _ => Err(Error::InvalidState),
        }
    }

    /// Begin a transactional import into `destination` using the default allocator.
    pub fn importer<'destination, 'destination_alloc: 'destination_cb, 'destination_cb>(
        &self,
        destination: &'destination mut Terminal<'destination_alloc, 'destination_cb>,
        options: HistoryOptions,
    ) -> Result<HistoryImporter<'terminal, 'destination, 'alloc, 'static>> {
        unsafe {
            HistoryImporter::new_inner(
                std::ptr::null(),
                destination.inner.as_raw(),
                Rc::clone(&self.lease),
                options,
            )
        }
    }

    /// Begin a transactional import using `allocator` for importer state.
    pub fn importer_with_alloc<
        'destination,
        'destination_alloc: 'destination_cb,
        'destination_cb,
        'import_alloc,
        'ctx: 'import_alloc,
    >(
        &self,
        allocator: &'import_alloc Allocator<'ctx>,
        destination: &'destination mut Terminal<'destination_alloc, 'destination_cb>,
        options: HistoryOptions,
    ) -> Result<HistoryImporter<'terminal, 'destination, 'alloc, 'import_alloc>> {
        unsafe {
            HistoryImporter::new_inner(
                allocator.to_raw(),
                destination.inner.as_raw(),
                Rc::clone(&self.lease),
                options,
            )
        }
    }
}

impl Drop for HistoryCursor<'_, '_> {
    fn drop(&mut self) {
        unsafe { ffi::ghostty_terminal_history_cursor_free(self.inner.as_raw()) };
    }
}

/// Result of importing one complete opaque history unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryImportEvent {
    /// Exact bytes consumed from the submitted unit.
    pub consumed: usize,
    /// Rows represented by the unit.
    pub rows: usize,
    /// Whether the destination retained this unit.
    pub retained: bool,
}

/// Transactional authenticated history importer.
///
/// The destination is borrowed against outside mutation until commit or abort.
/// [`HistoryImporter::vt_write`] permits serialized live writes between pushes.
#[derive(Debug)]
pub struct HistoryImporter<'source, 'destination, 'lease_alloc, 'import_alloc> {
    inner: Object<'import_alloc, ffi::TerminalHistoryImporterImpl>,
    destination: NonNull<ffi::TerminalImpl>,
    _lease: Rc<LeaseInner<'source, 'lease_alloc>>,
    capability: CapabilityToken,
    finalized: bool,
    _destination: PhantomData<&'destination mut ()>,
}

impl<'source, 'destination, 'lease_alloc, 'import_alloc>
    HistoryImporter<'source, 'destination, 'lease_alloc, 'import_alloc>
{
    unsafe fn new_inner(
        allocator: *const ffi::Allocator,
        destination: ffi::Terminal,
        lease: Rc<LeaseInner<'source, 'lease_alloc>>,
        options: HistoryOptions,
    ) -> Result<Self> {
        let destination = NonNull::new(destination).ok_or(Error::WrongTerminal)?;
        let mut raw = ffi::TerminalHistoryImporterResult {
            size: std::mem::size_of::<ffi::TerminalHistoryImporterResult>(),
            version: ABI_VERSION,
            ..Default::default()
        };
        let raw_options = options.raw();
        let status = unsafe {
            ffi::ghostty_terminal_history_importer_new(
                allocator,
                destination.as_ptr(),
                lease.screen.0,
                lease.source.as_ptr(),
                &raw const lease.checkpoint.raw,
                &raw const raw_options,
                &raw mut raw,
            )
        };
        if let Err(error) = from_status(status, 0, 0) {
            if !raw.importer.is_null() {
                unsafe { ffi::ghostty_terminal_history_importer_free(raw.importer) };
            }
            return Err(error);
        }
        Ok(Self {
            inner: Object::new(raw.importer).map_err(|_| Error::OutOfMemory)?,
            destination,
            _lease: lease,
            capability: CapabilityToken::new(raw.capability),
            finalized: false,
            _destination: PhantomData,
        })
    }

    /// Return this importer's opaque engine capability.
    pub fn capability(&self) -> &CapabilityToken {
        &self.capability
    }

    /// Import one complete authenticated unit without retaining its bytes.
    pub fn push(&mut self, unit: &[u8], options: HistoryOptions) -> Result<HistoryImportEvent> {
        let mut raw = ffi::TerminalHistoryImportEvent {
            size: std::mem::size_of::<ffi::TerminalHistoryImportEvent>(),
            version: ABI_VERSION,
            ..Default::default()
        };
        let raw_options = options.raw();
        let status = unsafe {
            ffi::ghostty_terminal_history_importer_push(
                self.inner.as_raw(),
                self.destination.as_ptr(),
                unit.as_ptr(),
                unit.len(),
                &raw const raw_options,
                &raw mut raw,
            )
        };
        from_status(status, raw.required_bytes, raw.required_rows)?;
        if raw.consumed != unit.len() {
            return Err(Error::InvalidState);
        }
        Ok(HistoryImportEvent {
            consumed: raw.consumed,
            rows: raw.rows,
            retained: raw.retained,
        })
    }

    /// Feed live VT bytes to the borrowed destination between history pushes.
    pub fn vt_write(&mut self, data: &[u8]) {
        unsafe {
            ffi::ghostty_terminal_vt_write(self.destination.as_ptr(), data.as_ptr(), data.len())
        }
    }

    /// Atomically publish all imported history and release the destination borrow.
    pub fn commit(mut self) -> Result<()> {
        let status = unsafe {
            ffi::ghostty_terminal_history_importer_commit(
                self.inner.as_raw(),
                self.destination.as_ptr(),
            )
        };
        from_status(status, 0, 0)?;
        self.finalized = true;
        Ok(())
    }

    /// Discard all imported history and release the destination borrow.
    pub fn abort(mut self) -> Result<()> {
        self.finalized = true;
        from_status(
            unsafe {
                ffi::ghostty_terminal_history_importer_abort(
                    self.inner.as_raw(),
                    self.destination.as_ptr(),
                )
            },
            0,
            0,
        )
    }
}

impl Drop for HistoryImporter<'_, '_, '_, '_> {
    fn drop(&mut self) {
        if !self.finalized {
            let _ = unsafe {
                ffi::ghostty_terminal_history_importer_abort(
                    self.inner.as_raw(),
                    self.destination.as_ptr(),
                )
            };
        }
        unsafe { ffi::ghostty_terminal_history_importer_free(self.inner.as_raw()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, ffi::c_void};

    #[test]
    fn reports_incremental_codec_identity_and_bounds() {
        let capabilities = capabilities().expect("incremental capabilities");
        assert_eq!(capabilities.version, ABI_VERSION);
        assert_eq!(capabilities.default_encode_version, 2);
        assert!(capabilities.incremental);
        assert!(capabilities.ready);
        assert!(capabilities.history);
        assert!(capabilities.bounded_records);
        assert!(capabilities.bounded_pages);
        assert!(!capabilities.codec_identity.is_empty());
        assert!(!capabilities.build_identity.is_empty());
    }
    fn terminal(cols: u16, rows: u16) -> Terminal<'static, 'static> {
        Terminal::new(crate::TerminalOptions {
            cols,
            rows,
            max_scrollback: 1000,
        })
        .expect("terminal construction")
    }

    fn capture_all(source: &mut Terminal<'static, 'static>) -> Vec<u8> {
        let mut capture = source
            .capture(CaptureOptions::default())
            .expect("capture construction");
        let mut all = Vec::new();
        loop {
            let required = match capture.next(&mut []) {
                Err(Error::OutOfSpace {
                    required_bytes,
                    required_rows: 0,
                }) => required_bytes,
                other => panic!("zero-byte capture probe: {other:?}"),
            };
            assert!(required > 0);
            if required > 1 {
                let mut short = vec![0; required - 1];
                assert_eq!(
                    capture.next(&mut short).unwrap_err(),
                    Error::OutOfSpace {
                        required_bytes: required,
                        required_rows: 0,
                    }
                );
            }
            let mut record = vec![0; required];
            let event = capture.next(&mut record).expect("exact record buffer");
            assert_eq!(event.record.len(), required);
            let finished = matches!(event.kind, CaptureEventKind::Finish);
            all.extend_from_slice(event.record);
            if finished {
                break;
            }
        }
        all
    }

    enum DriveState {
        Before(Decoder<'static>),
        After(DecodedStream<'static, 'static>),
        Empty,
    }

    fn decode_with_cut(
        data: &[u8],
        cut: usize,
        live_during_history: bool,
    ) -> (Terminal<'static, 'static>, usize) {
        let mut state = DriveState::Before(
            Decoder::new(DecoderOptions::default()).expect("decoder construction"),
        );
        let mut offset = 0;
        let mut boundary = cut.min(data.len());
        let mut wrote_live = false;
        loop {
            if offset == boundary && boundary != data.len() {
                boundary = data.len();
            }
            assert!(offset < boundary, "decoder did not reach FINISH");
            let fragment = &data[offset..boundary];
            state = match std::mem::replace(&mut state, DriveState::Empty) {
                DriveState::Before(decoder) => match decoder.push(fragment) {
                    Ok(DecodeStep::NeedInput { decoder, progress })
                    | Ok(DecodeStep::Progress { decoder, progress }) => {
                        assert!(progress.consumed > 0);
                        offset += progress.consumed;
                        DriveState::Before(decoder)
                    }
                    Ok(DecodeStep::Ready { decoder, progress }) => {
                        assert!(progress.consumed > 0);
                        offset += progress.consumed;
                        let continuation = decoder
                            .take_terminal::<'static>()
                            .expect("READY terminal transfer");
                        DriveState::After(
                            continuation.replay().expect("one-shot continuation replay"),
                        )
                    }
                    Err(error) => panic!("decode before READY failed: {error}"),
                },
                DriveState::After(decoder) => match decoder.push(fragment) {
                    Ok(AfterReadyStep::NeedInput { decoder, progress })
                    | Ok(AfterReadyStep::Progress { decoder, progress })
                    | Ok(AfterReadyStep::HistoryBegin {
                        decoder, progress, ..
                    }) => {
                        assert!(progress.consumed > 0);
                        offset += progress.consumed;
                        DriveState::After(decoder)
                    }
                    Ok(AfterReadyStep::HistoryPage {
                        mut decoder,
                        progress,
                        ..
                    }) => {
                        assert!(progress.consumed > 0);
                        offset += progress.consumed;
                        if live_during_history && !wrote_live {
                            decoder
                                .terminal_mut()
                                .vt_write(b"live-between-decoded-history-pages\r\n");
                            wrote_live = true;
                        }
                        DriveState::After(decoder)
                    }
                    Ok(AfterReadyStep::Finish(finished)) => {
                        offset += finished.progress.consumed;
                        return (finished.terminal, offset);
                    }
                    Err(error) => panic!("decode after READY failed: {error}"),
                },
                DriveState::Empty => unreachable!(),
            };
        }
    }

    #[test]
    fn every_fragment_cut_reaches_ready_replays_continuation_and_finishes() {
        let mut source = terminal(20, 4);
        source.vt_write(b"first\r\nsecond\r\n\x1b[31");
        let bytes = capture_all(&mut source);
        source.vt_write(b"mred");
        let expected = source.encode_snapshot().expect("expected snapshot");

        for cut in 1..bytes.len() {
            let (mut decoded, consumed) = decode_with_cut(&bytes, cut, false);
            assert_eq!(consumed, bytes.len(), "cut {cut}");
            decoded.vt_write(b"mred");
            let actual = decoded.encode_snapshot().expect("decoded snapshot");
            assert_eq!(actual.as_ref(), expected.as_ref(), "cut {cut}");
        }
    }

    #[test]
    fn finish_leaves_transport_suffix_for_the_live_terminal() {
        let mut source = terminal(20, 4);
        source.vt_write(b"snapshot");
        let bytes = capture_all(&mut source);
        let tail = b"PTY-after-snapshot";
        let mut transport = bytes.clone();
        transport.extend_from_slice(tail);

        let (mut decoded, consumed) = decode_with_cut(&transport, 1, true);
        assert_eq!(consumed, bytes.len());
        assert_eq!(&transport[consumed..], tail);
        decoded.vt_write(&transport[consumed..]);
    }

    #[test]
    fn history_units_retry_short_buffers_and_allow_live_writes_while_importing() {
        if !capabilities().expect("capabilities").authenticated_tokens {
            return;
        }
        let mut source = terminal(20, 4);
        for row in 0..200 {
            source.vt_write(format!("row-{row:03}\r\n").as_bytes());
        }
        let mut destination = terminal(20, 4);
        let lease = source
            .history_lease(ScreenKey::PRIMARY)
            .expect("history lease");
        let mut cursor = lease.into_cursor().expect("one-way cursor transfer");
        let mut importer = cursor
            .importer(&mut destination, HistoryOptions::default())
            .expect("history importer");
        let mut wrote_live = false;
        let mut corrupted = false;

        loop {
            let required = match cursor.next(HistoryOptions::default(), &mut []) {
                Ok(HistoryEvent::End) => break,
                Err(Error::OutOfSpace {
                    required_bytes,
                    required_rows: 0,
                }) => required_bytes,
                other => panic!("history probe: {other:?}"),
            };
            assert!(required > 0);
            if required > 1 {
                let mut short = vec![0; required - 1];
                assert_eq!(
                    cursor
                        .next(HistoryOptions::default(), &mut short)
                        .unwrap_err(),
                    Error::OutOfSpace {
                        required_bytes: required,
                        required_rows: 0,
                    }
                );
            }
            let mut unit = vec![0; required];
            let unit_len = match cursor
                .next(HistoryOptions::default(), &mut unit)
                .expect("history unit")
            {
                HistoryEvent::Unit { unit, .. } => unit.len(),
                HistoryEvent::End => panic!("probe promised a unit"),
            };

            if !corrupted {
                unit[unit_len - 1] ^= 0x40;
                assert_eq!(
                    importer
                        .push(&unit[..unit_len], HistoryOptions::default())
                        .unwrap_err(),
                    Error::Corruption
                );
                unit[unit_len - 1] ^= 0x40;
                corrupted = true;
            }

            let options = HistoryOptions::default();
            let raw_options = options.raw();
            let mut raw_event = ffi::TerminalHistoryImportEvent {
                size: std::mem::size_of::<ffi::TerminalHistoryImportEvent>(),
                version: ABI_VERSION,
                ..Default::default()
            };
            let wrong = unsafe {
                ffi::ghostty_terminal_history_importer_push(
                    importer.inner.as_raw(),
                    cursor.lease.source.as_ptr(),
                    unit.as_ptr(),
                    unit_len,
                    &raw const raw_options,
                    &raw mut raw_event,
                )
            };
            assert_eq!(
                from_status(wrong, raw_event.required_bytes, raw_event.required_rows),
                Err(Error::WrongTerminal)
            );

            let imported = importer
                .push(&unit[..unit_len], HistoryOptions::default())
                .expect("history import");
            assert_eq!(imported.consumed, unit_len);
            if !wrote_live {
                importer.vt_write(b"live while older history imports\r\n");
                wrote_live = true;
            }
        }
        importer.commit().expect("transactional commit");
        drop(cursor);
        destination.vt_write(b"after commit");
    }

    fn raw_history_cursor(
        terminal: ffi::Terminal,
    ) -> (ffi::TerminalHistoryLease, ffi::TerminalHistoryCursor) {
        let mut lease = ffi::TerminalHistoryLeaseResult {
            size: std::mem::size_of::<ffi::TerminalHistoryLeaseResult>(),
            version: ABI_VERSION,
            ..Default::default()
        };
        from_status(
            unsafe {
                ffi::ghostty_terminal_history_lease_new(
                    std::ptr::null(),
                    terminal,
                    ScreenKey::PRIMARY.0,
                    &raw mut lease,
                )
            },
            0,
            0,
        )
        .expect("raw lease");
        let mut cursor = ffi::TerminalHistoryCursorResult {
            size: std::mem::size_of::<ffi::TerminalHistoryCursorResult>(),
            version: ABI_VERSION,
            ..Default::default()
        };
        from_status(
            unsafe {
                ffi::ghostty_terminal_history_lease_cursor(lease.lease, terminal, &raw mut cursor)
            },
            0,
            0,
        )
        .expect("raw cursor");
        (lease.lease, cursor.cursor)
    }

    fn raw_cursor_next(cursor: ffi::TerminalHistoryCursor, terminal: ffi::Terminal) -> Result<()> {
        let options = HistoryOptions::default().raw();
        let mut event = ffi::TerminalHistoryEvent {
            size: std::mem::size_of::<ffi::TerminalHistoryEvent>(),
            version: ABI_VERSION,
            ..Default::default()
        };
        let mut buffer = [0u8; 4096];
        let status = unsafe {
            ffi::ghostty_terminal_history_cursor_next(
                cursor,
                terminal,
                &raw const options,
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut event,
            )
        };
        from_status(status, event.required_bytes, 0)?;
        Ok(())
    }

    #[test]
    fn native_stale_reset_and_resize_statuses_remain_distinct() {
        if !capabilities().expect("capabilities").authenticated_tokens {
            return;
        }
        let mut source = terminal(20, 4);
        for row in 0..20 {
            source.vt_write(format!("row-{row}\r\n").as_bytes());
        }
        let raw_terminal = source.inner.as_raw();

        let mut wrong_generation = ffi::TerminalHistoryLeaseResult {
            size: std::mem::size_of::<ffi::TerminalHistoryLeaseResult>(),
            version: ABI_VERSION,
            ..Default::default()
        };
        let status = unsafe {
            ffi::ghostty_terminal_history_lease_new(
                std::ptr::null(),
                raw_terminal,
                u16::MAX,
                &raw mut wrong_generation,
            )
        };
        assert_eq!(from_status(status, 0, 0), Err(Error::WrongGeneration));
        assert!(wrong_generation.lease.is_null());

        let (lease, cursor) = raw_history_cursor(raw_terminal);
        unsafe { ffi::ghostty_terminal_history_lease_free(lease) };
        assert!(matches!(
            raw_cursor_next(cursor, raw_terminal),
            Err(Error::Stale)
        ));
        unsafe { ffi::ghostty_terminal_history_cursor_free(cursor) };

        let (lease, cursor) = raw_history_cursor(raw_terminal);
        unsafe { ffi::ghostty_terminal_reset(raw_terminal) };
        assert!(matches!(
            raw_cursor_next(cursor, raw_terminal),
            Err(Error::Reset)
        ));
        unsafe {
            ffi::ghostty_terminal_history_cursor_free(cursor);
            ffi::ghostty_terminal_history_lease_free(lease);
        }

        let (lease, cursor) = raw_history_cursor(raw_terminal);
        crate::error::from_result(unsafe {
            ffi::ghostty_terminal_resize(raw_terminal, 21, 5, 0, 0)
        })
        .expect("raw resize");
        assert!(matches!(
            raw_cursor_next(cursor, raw_terminal),
            Err(Error::Resize)
        ));
        unsafe {
            ffi::ghostty_terminal_history_cursor_free(cursor);
            ffi::ghostty_terminal_history_lease_free(lease);
        }
    }

    #[derive(Default)]
    struct FailState {
        calls: Cell<usize>,
        fail_after: Cell<usize>,
        active: Cell<usize>,
    }

    unsafe extern "C" fn fail_alloc(
        context: *mut c_void,
        len: usize,
        alignment: u8,
        _return_address: usize,
    ) -> *mut c_void {
        let state = unsafe { &*context.cast::<FailState>() };
        let call = state.calls.get();
        state.calls.set(call + 1);
        if call >= state.fail_after.get() {
            return std::ptr::null_mut();
        }
        let Ok(layout) = std::alloc::Layout::from_size_align(len, 1usize << alignment) else {
            return std::ptr::null_mut();
        };
        let memory = unsafe { std::alloc::alloc(layout) };
        if !memory.is_null() {
            state.active.set(state.active.get() + 1);
        }
        memory.cast()
    }

    unsafe extern "C" fn fail_free(
        context: *mut c_void,
        memory: *mut c_void,
        len: usize,
        alignment: u8,
        _return_address: usize,
    ) {
        let layout = std::alloc::Layout::from_size_align(len, 1usize << alignment)
            .expect("native allocation layout");
        unsafe { std::alloc::dealloc(memory.cast(), layout) };
        let state = unsafe { &*context.cast::<FailState>() };
        state.active.set(state.active.get() - 1);
    }

    unsafe extern "C" fn no_resize(
        _context: *mut c_void,
        _memory: *mut c_void,
        _memory_len: usize,
        _alignment: u8,
        _new_len: usize,
        _return_address: usize,
    ) -> bool {
        false
    }

    unsafe extern "C" fn fail_remap(
        context: *mut c_void,
        memory: *mut c_void,
        memory_len: usize,
        alignment: u8,
        new_len: usize,
        _return_address: usize,
    ) -> *mut c_void {
        let state = unsafe { &*context.cast::<FailState>() };
        let call = state.calls.get();
        state.calls.set(call + 1);
        if call >= state.fail_after.get() {
            return std::ptr::null_mut();
        }
        let Ok(layout) = std::alloc::Layout::from_size_align(memory_len, 1usize << alignment)
        else {
            return std::ptr::null_mut();
        };
        unsafe { std::alloc::realloc(memory.cast(), layout, new_len).cast() }
    }

    static FAIL_VTABLE: ffi::AllocatorVtable = ffi::AllocatorVtable {
        alloc: Some(fail_alloc),
        resize: Some(no_resize),
        remap: Some(fail_remap),
        free: Some(fail_free),
    };

    fn failing_allocator(state: &FailState) -> Allocator<'_> {
        let raw = ffi::Allocator {
            ctx: std::ptr::from_ref(state).cast_mut().cast(),
            vtable: &FAIL_VTABLE,
        };
        unsafe { Allocator::from_raw(&raw) }
    }

    #[test]
    fn custom_allocator_oom_and_early_drop_cleanup_are_exact() {
        let state = FailState::default();
        state.fail_after.set(1);
        let allocator = failing_allocator(&state);
        let mut source = terminal(20, 4);
        assert_eq!(
            source
                .capture_with_alloc(&allocator, CaptureOptions::default())
                .unwrap_err(),
            Error::OutOfMemory
        );
        assert_eq!(state.active.get(), 0);

        state.calls.set(0);
        state.fail_after.set(usize::MAX);
        drop(
            source
                .capture_with_alloc(&allocator, CaptureOptions::default())
                .expect("capture"),
        );
        assert_eq!(state.active.get(), 0);

        state.calls.set(0);
        state.fail_after.set(1);
        assert_eq!(
            Decoder::new_with_alloc(&allocator, DecoderOptions::default()).unwrap_err(),
            Error::OutOfMemory
        );
        assert_eq!(state.active.get(), 0);

        state.calls.set(0);
        state.fail_after.set(0);
        assert_eq!(
            source
                .history_lease_with_alloc(&allocator, ScreenKey::PRIMARY)
                .unwrap_err(),
            Error::OutOfMemory
        );
        assert_eq!(state.active.get(), 0);

        if capabilities().expect("capabilities").authenticated_tokens {
            state.calls.set(0);
            state.fail_after.set(usize::MAX);
            let lease = source
                .history_lease_with_alloc(&allocator, ScreenKey::PRIMARY)
                .expect("allocator-owned lease");
            let cursor = lease.into_cursor().expect("allocator-owned cursor");
            assert_eq!(state.active.get(), 2);
            let mut destination = terminal(20, 4);
            let importer = cursor
                .importer_with_alloc(&allocator, &mut destination, HistoryOptions::default())
                .expect("allocator-owned importer");
            assert_eq!(state.active.get(), 3);
            drop(importer);
            assert_eq!(state.active.get(), 2);
            drop(cursor);
            assert_eq!(state.active.get(), 0);
        }
    }

    #[test]
    fn continuation_replay_oom_preserves_state_for_retry() {
        let mut source = terminal(20, 4);
        source.vt_write(b"before\r\n\x1b[31");
        let bytes = capture_all(&mut source);

        let state = FailState::default();
        state.fail_after.set(usize::MAX);
        let allocator = failing_allocator(&state);
        let mut decoder = Decoder::new_with_alloc(&allocator, DecoderOptions::default())
            .expect("decoder construction");
        let mut offset = 0;
        let ready = loop {
            match decoder.push(&bytes[offset..]) {
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
                Ok(DecodeStep::Ready {
                    decoder: ready,
                    progress,
                }) => {
                    assert!(progress.consumed > 0);
                    offset += progress.consumed;
                    break ready;
                }
                Err(error) => panic!("decode before READY failed: {error}"),
            }
        };
        let continuation = ready.take_terminal().expect("READY terminal transfer");

        state.fail_after.set(state.calls.get());
        let failure = continuation
            .replay()
            .expect_err("verification allocation should fail once");
        assert_eq!(failure.error, Error::OutOfMemory);

        state.fail_after.set(usize::MAX);
        let mut stream = failure
            .decoder
            .replay()
            .expect("retry should replay the untouched continuation");
        let mut decoded = loop {
            match stream.push(&bytes[offset..]) {
                Ok(AfterReadyStep::NeedInput {
                    decoder: next,
                    progress,
                })
                | Ok(AfterReadyStep::Progress {
                    decoder: next,
                    progress,
                })
                | Ok(AfterReadyStep::HistoryBegin {
                    decoder: next,
                    progress,
                    ..
                })
                | Ok(AfterReadyStep::HistoryPage {
                    decoder: next,
                    progress,
                    ..
                }) => {
                    assert!(progress.consumed > 0);
                    offset += progress.consumed;
                    stream = next;
                }
                Ok(AfterReadyStep::Finish(finished)) => {
                    offset += finished.progress.consumed;
                    break finished.terminal;
                }
                Err(error) => panic!("decode after replay retry failed: {error}"),
            }
        };
        assert_eq!(offset, bytes.len());

        source.vt_write(b"mred");
        decoded.vt_write(b"mred");
        let expected = source.encode_snapshot().expect("source snapshot");
        let actual = decoded.encode_snapshot().expect("retried decoder snapshot");
        assert_eq!(actual.as_ref(), expected.as_ref());
        drop(actual);
        drop(expected);
        drop(decoded);
        assert_eq!(state.active.get(), 0);
    }
}
