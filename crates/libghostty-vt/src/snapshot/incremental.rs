//! Bounded, incremental terminal snapshots and authenticated history paging.
//!
//! Record and unit bytes are deliberately opaque. The safe API only moves one
//! complete native record into a caller-owned buffer; it never parses or
//! rewrites Ghostty's codec payloads.

use std::{fmt, marker::PhantomData, ptr::NonNull, rc::Rc};

use crate::{
    alloc::{Allocator, Object},
    ffi,
    terminal::{ScrollViewport, Terminal},
};

const ABI_VERSION: u32 = ffi::TERMINAL_SNAPSHOT_ABI_VERSION;

/// Byte length of an opaque authenticated history token.
pub const TOKEN_LEN: usize = ffi::TERMINAL_HISTORY_TOKEN_BYTES as usize;
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

    /// Borrow the authenticated bytes for transport without interpreting them.
    ///
    /// These bytes are opaque. They are only valid when echoed back within the
    /// same engine-owned terminal generation; this crate intentionally exposes
    /// no constructor from arbitrary bytes.
    pub fn as_bytes(&self) -> &[u8; TOKEN_LEN] {
        &self.raw.bytes
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

    /// Borrow the authenticated bytes for transport without interpreting them.
    ///
    /// These bytes are opaque. They are only valid when echoed back within the
    /// same engine-owned terminal generation; this crate intentionally exposes
    /// no constructor from arbitrary bytes.
    pub fn as_bytes(&self) -> &[u8; TOKEN_LEN] {
        &self._raw.bytes
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

/// Strict limits for converting a READY capture into owned records.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DetachOptions {
    /// Inclusive number of post-READY PAGE records after row splitting.
    pub max_pages: usize,
    /// Inclusive retained bytes for records and native record metadata.
    pub max_total_bytes: usize,
    /// Inclusive number of rows represented by each owned PAGE record.
    pub max_rows: usize,
}

impl Default for DetachOptions {
    fn default() -> Self {
        Self {
            max_pages: 4096,
            max_total_bytes: 64 * 1024 * 1024,
            max_rows: 256,
        }
    }
}

impl DetachOptions {
    fn raw(self) -> ffi::TerminalSnapshotDetachOptions {
        ffi::TerminalSnapshotDetachOptions {
            size: std::mem::size_of::<ffi::TerminalSnapshotDetachOptions>(),
            version: ABI_VERSION,
            max_pages: self.max_pages,
            max_total_bytes: self.max_total_bytes,
            max_rows: self.max_rows,
        }
    }
}

/// Row budget applied to one owned continuation delivery attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContinuationOptions {
    /// Inclusive number of HISTORY_PAGE rows accepted by this request.
    pub max_rows: usize,
}

impl Default for ContinuationOptions {
    fn default() -> Self {
        Self { max_rows: 256 }
    }
}

impl ContinuationOptions {
    fn raw(self) -> ffi::TerminalSnapshotContinuationOptions {
        ffi::TerminalSnapshotContinuationOptions {
            size: std::mem::size_of::<ffi::TerminalSnapshotContinuationOptions>(),
            version: ABI_VERSION,
            max_rows: self.max_rows,
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
    /// Rows represented by this HISTORY_PAGE, zero for every other event.
    pub rows: usize,
    /// Complete opaque record bytes.
    pub record: &'buffer [u8],
}

fn empty_capture_event() -> ffi::TerminalSnapshotCaptureEvent {
    ffi::TerminalSnapshotCaptureEvent {
        size: std::mem::size_of::<ffi::TerminalSnapshotCaptureEvent>(),
        version: ABI_VERSION,
        ..Default::default()
    }
}

fn capture_event<'buffer>(
    status: ffi::TerminalSnapshotStatus::Type,
    raw: ffi::TerminalSnapshotCaptureEvent,
    buffer: &'buffer mut [u8],
) -> Result<CaptureEvent<'buffer>> {
    from_status(status, raw.required_bytes, raw.required_rows)?;
    if raw.written > buffer.len() || raw.required_bytes != raw.written {
        return Err(Error::InvalidState);
    }
    let kind = match raw.kind {
        ffi::TerminalSnapshotCaptureEventKind::RECORD => CaptureEventKind::Record,
        ffi::TerminalSnapshotCaptureEventKind::READY => CaptureEventKind::Ready {
            checkpoint: CheckpointToken::new(raw.checkpoint),
        },
        ffi::TerminalSnapshotCaptureEventKind::HISTORY_BEGIN => CaptureEventKind::HistoryBegin {
            screen: ScreenKey(raw.screen_key),
            count: raw.count,
        },
        ffi::TerminalSnapshotCaptureEventKind::HISTORY_PAGE => CaptureEventKind::HistoryPage {
            screen: ScreenKey(raw.screen_key),
            index: raw.index,
            count: raw.count,
        },
        ffi::TerminalSnapshotCaptureEventKind::FINISH => CaptureEventKind::Finish,
        _ => return Err(Error::InvalidState),
    };
    if !matches!(kind, CaptureEventKind::HistoryPage { .. }) && raw.rows != 0 {
        return Err(Error::InvalidState);
    }
    Ok(CaptureEvent {
        kind,
        codec_version: raw.codec_version,
        rows: raw.rows,
        record: &buffer[..raw.written],
    })
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
        let mut raw = empty_capture_event();
        let status = unsafe {
            ffi::ghostty_terminal_snapshot_capture_next(
                self.inner.as_raw(),
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut raw,
            )
        };
        capture_event(status, raw, buffer)
    }

    /// Eagerly own all records after an already-delivered READY event.
    ///
    /// This transition is transactional. Allocation or limit failure returns
    /// the original capture so the same READY cut can be retried or aborted.
    /// On success, the returned continuation carries no terminal lifetime: the
    /// source terminal can be mutated or dropped before [`CaptureContinuation::next`] resumes.
    pub fn detach_ready(
        mut self,
        options: DetachOptions,
    ) -> Result<CaptureContinuation<'alloc>, CaptureDetachFailure<Self>> {
        let mut capture = self.inner.as_raw();
        let mut continuation = std::ptr::null_mut();
        let raw_options = options.raw();
        let status = unsafe {
            ffi::ghostty_terminal_snapshot_capture_detach_ready(
                &mut capture,
                &raw_options,
                &mut continuation,
            )
        };
        if let Err(error) = from_status(status, 0, 0) {
            return Err(CaptureDetachFailure {
                error,
                capture: self,
            });
        }
        self.active = false;
        let inner = Object::new(continuation)
            .expect("Ghostty returned success with a null snapshot continuation");
        std::mem::forget(self);
        Ok(CaptureContinuation {
            inner,
            active: true,
            _not_send_or_sync: PhantomData,
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

/// Transactional detach failure carrying the still-usable original capture.
#[derive(Debug)]
pub struct CaptureDetachFailure<C> {
    /// Exact native error.
    pub error: Error,
    /// Capture at the unchanged READY boundary.
    pub capture: C,
}

/// Terminal-independent owner of all snapshot records after READY.
#[derive(Debug)]
pub struct CaptureContinuation<'alloc> {
    inner: Object<'alloc, ffi::TerminalSnapshotContinuationImpl>,
    active: bool,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl<'alloc> CaptureContinuation<'alloc> {
    /// Emit one complete owned record into `buffer`.
    ///
    /// Byte or row shortage is nonadvancing. [`Error::OutOfSpace`] contains
    /// exact requirements, while [`CaptureEvent::rows`] declares the accepted
    /// row charge for every HISTORY_PAGE.
    pub fn next<'buffer>(
        &mut self,
        options: ContinuationOptions,
        buffer: &'buffer mut [u8],
    ) -> Result<CaptureEvent<'buffer>> {
        let raw_options = options.raw();
        let mut raw = empty_capture_event();
        let status = unsafe {
            ffi::ghostty_terminal_snapshot_continuation_next(
                self.inner.as_raw(),
                &raw_options,
                buffer.as_mut_ptr(),
                buffer.len(),
                &raw mut raw,
            )
        };
        capture_event(status, raw, buffer)
    }

    /// Abort without emitting remaining history or FINISH.
    pub fn abort(mut self) -> Result<()> {
        self.active = false;
        from_status(
            unsafe { ffi::ghostty_terminal_snapshot_continuation_abort(self.inner.as_raw()) },
            0,
            0,
        )
    }
}

impl Drop for CaptureContinuation<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ =
                unsafe { ffi::ghostty_terminal_snapshot_continuation_abort(self.inner.as_raw()) };
        }
        unsafe { ffi::ghostty_terminal_snapshot_continuation_free(self.inner.as_raw()) };
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

    /// Consume this terminal into an owned live history cursor.
    ///
    /// Unlike [`Terminal::history_lease`], this shape permits controlled
    /// mutation through [`LiveHistoryCursor::vt_write`],
    /// [`LiveHistoryCursor::resize`], and [`LiveHistoryCursor::reset`] while
    /// engine copy-on-write keeps the older history cut stable.
    pub fn into_live_history_cursor(
        self,
        screen: ScreenKey,
    ) -> Result<LiveHistoryCursor<'terminal_alloc, 'cb, 'static>> {
        unsafe { LiveHistoryCursor::new_inner(self, std::ptr::null(), screen) }
    }

    /// Consume this terminal into a live cursor using `allocator` for lease
    /// and cursor state.
    pub fn into_live_history_cursor_with_alloc<'lease_alloc, 'ctx: 'lease_alloc>(
        self,
        allocator: &'lease_alloc Allocator<'ctx>,
        screen: ScreenKey,
    ) -> Result<LiveHistoryCursor<'terminal_alloc, 'cb, 'lease_alloc>> {
        unsafe { LiveHistoryCursor::new_inner(self, allocator.to_raw(), screen) }
    }

    /// Consume this terminal into a bounded multi-client live history manager.
    ///
    /// Construction failure returns the unchanged terminal in
    /// [`LiveHistorySetFailure`].
    pub fn into_live_history_set(
        self,
        capacity: usize,
    ) -> Result<
        LiveHistorySet<'terminal_alloc, 'cb, 'static>,
        LiveHistorySetFailure<'terminal_alloc, 'cb>,
    > {
        LiveHistorySet::new_inner(self, std::ptr::null(), capacity)
    }

    /// Consume this terminal into a multi-client manager using `allocator` for
    /// every native lease and cursor owned by the set. Construction failure
    /// returns the unchanged terminal.
    pub fn into_live_history_set_with_alloc<'lease_alloc, 'ctx: 'lease_alloc>(
        self,
        allocator: &'lease_alloc Allocator<'ctx>,
        capacity: usize,
    ) -> Result<
        LiveHistorySet<'terminal_alloc, 'cb, 'lease_alloc>,
        LiveHistorySetFailure<'terminal_alloc, 'cb>,
    > {
        LiveHistorySet::new_inner(self, allocator.to_raw(), capacity)
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
/// The owned terminal is live and may accept controlled VT writes between
/// later history transitions through [`DecodedStream::vt_write`].
///
/// ```compile_fail
/// use libghostty_vt::{Terminal, TerminalOptions};
/// use libghostty_vt::snapshot::incremental::DecodedStream;
///
/// fn cannot_replace(stream: &mut DecodedStream<'static, 'static>) {
///     let replacement = Terminal::new(TerminalOptions {
///         cols: 80, rows: 24, max_scrollback: 100,
///     }).unwrap();
///     // Only a shared terminal reference is exposed while the native
///     // decoder retains its handle through HISTORY and FINISH.
///     let _old = std::mem::replace(stream.terminal(), replacement);
/// }
/// ```
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

    /// Process live VT bytes while preserving decoder ownership.
    pub fn vt_write(&mut self, data: &[u8]) {
        self.terminal.vt_write(data);
    }

    /// Register live PTY write-back while retaining decoded terminal ownership.
    pub fn on_pty_write(
        &mut self,
        callback: impl crate::terminal::PtyWriteFn<'alloc, 'cb>,
    ) -> crate::error::Result<&mut Self> {
        self.terminal.on_pty_write(callback)?;
        Ok(self)
    }

    /// Scroll the live decoded viewport without exposing terminal ownership.
    pub fn scroll_viewport(&mut self, scroll: ScrollViewport) {
        self.terminal.scroll_viewport(scroll);
    }

    /// Resize the decoded terminal. Any later snapshot history pages are
    /// reported with `retained == false`; authenticated decoding still reaches
    /// FINISH without replacing the resized active state.
    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> crate::error::Result<()> {
        self.terminal
            .resize(cols, rows, cell_width_px, cell_height_px)
    }

    /// Reset the decoded terminal. Any later snapshot history pages are
    /// reported with `retained == false`; authenticated decoding still reaches
    /// FINISH without replacing the reset active state.
    pub fn reset(&mut self) {
        self.terminal.reset();
    }

    /// Set the decoded terminal's scrollback byte limit without moving it.
    pub fn set_scrollback_max_bytes(&mut self, max: Option<usize>) -> crate::error::Result<()> {
        self.terminal.set_scrollback_max_bytes(max)?;
        Ok(())
    }

    /// Set or clear the decoded terminal's physical scrollback line limit.
    pub fn set_scrollback_max_lines(&mut self, max: Option<usize>) -> crate::error::Result<()> {
        self.terminal.set_scrollback_max_lines(max)?;
        Ok(())
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

fn history_cursor_next<'buffer>(
    cursor: ffi::TerminalHistoryCursor,
    terminal: ffi::Terminal,
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
            cursor,
            terminal,
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
        history_cursor_next(
            self.inner.as_raw(),
            self.lease.source.as_ptr(),
            options,
            buffer,
        )
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

/// Owned terminal plus an engine copy-on-write history cut and cursor.
///
/// This shape is for actors that must keep processing live PTY input while
/// paging older history. It consumes the terminal, so all mutation remains
/// serialized through this owner. Drop and [`LiveHistoryCursor::into_terminal`]
/// always release the cursor and lease before the terminal.
///
/// ```compile_fail
/// use libghostty_vt::{Terminal, TerminalOptions};
/// use libghostty_vt::snapshot::incremental::ScreenKey;
///
/// let terminal = Terminal::new(TerminalOptions {
///     cols: 80, rows: 24, max_scrollback: 100,
/// }).unwrap();
/// let live = terminal.into_live_history_cursor(ScreenKey::PRIMARY).unwrap();
/// let replacement = Terminal::new(TerminalOptions {
///     cols: 80, rows: 24, max_scrollback: 100,
/// }).unwrap();
/// // The terminal is only available by shared reference, so it cannot be
/// // replaced or dropped while native cursor and lease handles refer to it.
/// let _old = std::mem::replace(live.terminal(), replacement);
/// ```
#[derive(Debug)]
pub struct LiveHistoryCursor<'terminal_alloc: 'cb, 'cb, 'lease_alloc> {
    cursor: Option<Object<'lease_alloc, ffi::TerminalHistoryCursorImpl>>,
    lease: Option<Object<'lease_alloc, ffi::TerminalHistoryLeaseImpl>>,
    terminal: Option<Terminal<'terminal_alloc, 'cb>>,
    checkpoint: CheckpointToken,
    capability: CapabilityToken,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl<'terminal_alloc: 'cb, 'cb, 'lease_alloc>
    LiveHistoryCursor<'terminal_alloc, 'cb, 'lease_alloc>
{
    unsafe fn new_inner(
        terminal: Terminal<'terminal_alloc, 'cb>,
        allocator: *const ffi::Allocator,
        screen: ScreenKey,
    ) -> Result<Self> {
        let terminal_raw = terminal.inner.as_raw();
        let mut lease_raw = ffi::TerminalHistoryLeaseResult {
            size: std::mem::size_of::<ffi::TerminalHistoryLeaseResult>(),
            version: ABI_VERSION,
            ..Default::default()
        };
        let status = unsafe {
            ffi::ghostty_terminal_history_lease_new(
                allocator,
                terminal_raw,
                screen.0,
                &raw mut lease_raw,
            )
        };
        if let Err(error) = from_status(status, 0, 0) {
            if !lease_raw.lease.is_null() {
                unsafe { ffi::ghostty_terminal_history_lease_free(lease_raw.lease) };
            }
            return Err(error);
        }
        let lease = match Object::new(lease_raw.lease) {
            Ok(lease) => lease,
            Err(_) => return Err(Error::OutOfMemory),
        };

        let mut cursor_raw = ffi::TerminalHistoryCursorResult {
            size: std::mem::size_of::<ffi::TerminalHistoryCursorResult>(),
            version: ABI_VERSION,
            ..Default::default()
        };
        let status = unsafe {
            ffi::ghostty_terminal_history_lease_cursor(
                lease.as_raw(),
                terminal_raw,
                &raw mut cursor_raw,
            )
        };
        if let Err(error) = from_status(status, 0, 0) {
            if !cursor_raw.cursor.is_null() {
                unsafe { ffi::ghostty_terminal_history_cursor_free(cursor_raw.cursor) };
            }
            unsafe { ffi::ghostty_terminal_history_lease_free(lease.as_raw()) };
            return Err(error);
        }
        let cursor = match Object::new(cursor_raw.cursor) {
            Ok(cursor) => cursor,
            Err(_) => {
                unsafe { ffi::ghostty_terminal_history_lease_free(lease.as_raw()) };
                return Err(Error::OutOfMemory);
            }
        };

        Ok(Self {
            cursor: Some(cursor),
            lease: Some(lease),
            terminal: Some(terminal),
            checkpoint: CheckpointToken::new(lease_raw.checkpoint),
            capability: CapabilityToken::new(cursor_raw.capability),
            _not_send_or_sync: PhantomData,
        })
    }

    /// Borrow the live terminal for read-only queries.
    pub fn terminal(&self) -> &Terminal<'terminal_alloc, 'cb> {
        self.terminal
            .as_ref()
            .expect("live history cursor always owns its terminal")
    }

    /// Process live VT bytes while preserving the copy-on-write history cut.
    pub fn vt_write(&mut self, data: &[u8]) {
        self.terminal
            .as_mut()
            .expect("live history cursor always owns its terminal")
            .vt_write(data);
    }

    /// Set the owned terminal's scrollback byte limit without moving it.
    pub fn set_scrollback_max_bytes(&mut self, max: Option<usize>) -> crate::error::Result<()> {
        self.terminal
            .as_mut()
            .expect("live history cursor always owns its terminal")
            .set_scrollback_max_bytes(max)?;
        Ok(())
    }

    /// Set or clear the owned terminal's physical scrollback line limit.
    pub fn set_scrollback_max_lines(&mut self, max: Option<usize>) -> crate::error::Result<()> {
        self.terminal
            .as_mut()
            .expect("live history cursor always owns its terminal")
            .set_scrollback_max_lines(max)?;
        Ok(())
    }

    /// Resize the owned terminal, explicitly invalidating this history cut.
    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> crate::error::Result<()> {
        self.terminal
            .as_mut()
            .expect("live history cursor always owns its terminal")
            .resize(cols, rows, cell_width_px, cell_height_px)
    }

    /// Reset the owned terminal, explicitly invalidating this history cut.
    pub fn reset(&mut self) {
        self.terminal
            .as_mut()
            .expect("live history cursor always owns its terminal")
            .reset();
    }

    /// Return the opaque authenticated checkpoint for this cut.
    pub fn checkpoint(&self) -> &CheckpointToken {
        &self.checkpoint
    }

    /// Return this cursor's opaque engine capability.
    pub fn capability(&self) -> &CapabilityToken {
        &self.capability
    }

    /// Emit one newest-first bounded unit without blocking controlled live
    /// terminal mutation between calls.
    pub fn next<'buffer>(
        &mut self,
        options: HistoryOptions,
        buffer: &'buffer mut [u8],
    ) -> Result<HistoryEvent<'buffer>> {
        let cursor = self
            .cursor
            .as_ref()
            .expect("live history cursor handle remains present");
        let terminal = self
            .terminal
            .as_ref()
            .expect("live history cursor always owns its terminal");
        history_cursor_next(cursor.as_raw(), terminal.inner.as_raw(), options, buffer)
    }

    /// Release cursor and lease state, returning the still-live terminal.
    pub fn into_terminal(mut self) -> Terminal<'terminal_alloc, 'cb> {
        self.release_handles();
        self.terminal
            .take()
            .expect("live history cursor always owns its terminal")
    }

    fn release_handles(&mut self) {
        if let Some(cursor) = self.cursor.take() {
            unsafe { ffi::ghostty_terminal_history_cursor_free(cursor.as_raw()) };
        }
        if let Some(lease) = self.lease.take() {
            unsafe { ffi::ghostty_terminal_history_lease_free(lease.as_raw()) };
        }
    }
}

impl Drop for LiveHistoryCursor<'_, '_, '_> {
    fn drop(&mut self) {
        self.release_handles();
    }
}

/// Opaque transport credentials for one manager-owned history cut.
///
/// The bytes may be echoed back to [`LiveHistorySet::next`] or
/// [`LiveHistorySet::release`], but cannot construct native tokens or handles.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct LiveHistoryCut {
    screen: ScreenKey,
    checkpoint: [u8; TOKEN_LEN],
    capability: [u8; TOKEN_LEN],
}

impl LiveHistoryCut {
    /// Screen whose history is pinned by this cut.
    pub fn screen(&self) -> ScreenKey {
        self.screen
    }

    /// Opaque authenticated history checkpoint bytes.
    pub fn checkpoint(&self) -> &[u8; TOKEN_LEN] {
        &self.checkpoint
    }

    /// Opaque capability bytes used to address this manager-owned cursor.
    pub fn capability(&self) -> &[u8; TOKEN_LEN] {
        &self.capability
    }
}

impl fmt::Debug for LiveHistoryCut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LiveHistoryCut")
            .field("screen", &self.screen)
            .field("checkpoint", &"<opaque>")
            .field("capability", &"<opaque>")
            .finish()
    }
}

#[derive(Debug)]
struct LiveHistorySetEntry<'alloc> {
    cursor: Option<Object<'alloc, ffi::TerminalHistoryCursorImpl>>,
    lease: Option<Object<'alloc, ffi::TerminalHistoryLeaseImpl>>,
    screen: ScreenKey,
    checkpoint: CheckpointToken,
    capability: CapabilityToken,
}

impl<'alloc> LiveHistorySetEntry<'alloc> {
    unsafe fn new(
        allocator: *const ffi::Allocator,
        terminal: ffi::Terminal,
        screen: ScreenKey,
    ) -> Result<Self> {
        let mut lease_raw = ffi::TerminalHistoryLeaseResult {
            size: std::mem::size_of::<ffi::TerminalHistoryLeaseResult>(),
            version: ABI_VERSION,
            ..Default::default()
        };
        let status = unsafe {
            ffi::ghostty_terminal_history_lease_new(
                allocator,
                terminal,
                screen.0,
                &raw mut lease_raw,
            )
        };
        if let Err(error) = from_status(status, 0, 0) {
            if !lease_raw.lease.is_null() {
                unsafe { ffi::ghostty_terminal_history_lease_free(lease_raw.lease) };
            }
            return Err(error);
        }
        let lease = Object::new(lease_raw.lease).map_err(|_| Error::OutOfMemory)?;

        let mut cursor_raw = ffi::TerminalHistoryCursorResult {
            size: std::mem::size_of::<ffi::TerminalHistoryCursorResult>(),
            version: ABI_VERSION,
            ..Default::default()
        };
        let status = unsafe {
            ffi::ghostty_terminal_history_lease_cursor(
                lease.as_raw(),
                terminal,
                &raw mut cursor_raw,
            )
        };
        if let Err(error) = from_status(status, 0, 0) {
            if !cursor_raw.cursor.is_null() {
                unsafe { ffi::ghostty_terminal_history_cursor_free(cursor_raw.cursor) };
            }
            unsafe { ffi::ghostty_terminal_history_lease_free(lease.as_raw()) };
            return Err(error);
        }
        let cursor = match Object::new(cursor_raw.cursor) {
            Ok(cursor) => cursor,
            Err(_) => {
                unsafe { ffi::ghostty_terminal_history_lease_free(lease.as_raw()) };
                return Err(Error::OutOfMemory);
            }
        };

        Ok(Self {
            cursor: Some(cursor),
            lease: Some(lease),
            screen,
            checkpoint: CheckpointToken::new(lease_raw.checkpoint),
            capability: CapabilityToken::new(cursor_raw.capability),
        })
    }

    fn cut(&self) -> LiveHistoryCut {
        LiveHistoryCut {
            screen: self.screen,
            checkpoint: *self.checkpoint.as_bytes(),
            capability: *self.capability.as_bytes(),
        }
    }

    fn matches(&self, capability: &[u8; TOKEN_LEN]) -> bool {
        self.capability
            .as_bytes()
            .iter()
            .zip(capability)
            .fold(0_u8, |difference, (actual, echoed)| {
                difference | (actual ^ echoed)
            })
            == 0
    }

    fn release(&mut self) {
        if let Some(cursor) = self.cursor.take() {
            unsafe { ffi::ghostty_terminal_history_cursor_free(cursor.as_raw()) };
        }
        if let Some(lease) = self.lease.take() {
            unsafe { ffi::ghostty_terminal_history_lease_free(lease.as_raw()) };
        }
    }
}

impl Drop for LiveHistorySetEntry<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

/// Lossless failure to construct a [`LiveHistorySet`].
#[derive(Debug)]
pub struct LiveHistorySetFailure<'terminal_alloc: 'cb, 'cb> {
    /// Exact construction error.
    pub error: Error,
    /// Original terminal, unchanged and still live.
    pub terminal: Terminal<'terminal_alloc, 'cb>,
}

impl fmt::Display for LiveHistorySetFailure<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for LiveHistorySetFailure<'_, '_> {}

/// Single-thread-affine owner for one live terminal and bounded concurrent cuts.
///
/// No mutable terminal reference escapes this manager. Each cut is addressed
/// only by capability bytes originally returned from [`LiveHistorySet::acquire`]
/// or [`LiveSetCapture::ready_cut`].
///
/// ```compile_fail
/// use libghostty_vt::{Terminal, TerminalOptions};
/// use libghostty_vt::snapshot::incremental::LiveHistorySet;
///
/// fn cannot_replace(set: &mut LiveHistorySet<'static, 'static, 'static>) {
///     let replacement = Terminal::new(TerminalOptions {
///         cols: 80, rows: 24, max_scrollback: 100,
///     }).unwrap();
///     let _old = std::mem::replace(set.terminal(), replacement);
/// }
/// ```
#[derive(Debug)]
pub struct LiveHistorySet<'terminal_alloc: 'cb, 'cb, 'lease_alloc> {
    entries: Vec<LiveHistorySetEntry<'lease_alloc>>,
    terminal: Option<Terminal<'terminal_alloc, 'cb>>,
    allocator: *const ffi::Allocator,
    capacity: usize,
    _not_send_or_sync: PhantomData<Rc<()>>,
}

impl<'terminal_alloc: 'cb, 'cb, 'lease_alloc> LiveHistorySet<'terminal_alloc, 'cb, 'lease_alloc> {
    fn new_inner(
        terminal: Terminal<'terminal_alloc, 'cb>,
        allocator: *const ffi::Allocator,
        capacity: usize,
    ) -> Result<Self, LiveHistorySetFailure<'terminal_alloc, 'cb>> {
        if capacity == 0 {
            return Err(LiveHistorySetFailure {
                error: Error::InvalidState,
                terminal,
            });
        }
        let mut entries = Vec::new();
        if entries.try_reserve_exact(capacity).is_err() {
            return Err(LiveHistorySetFailure {
                error: Error::OutOfMemory,
                terminal,
            });
        }
        Ok(Self {
            entries,
            terminal: Some(terminal),
            allocator,
            capacity,
            _not_send_or_sync: PhantomData,
        })
    }

    /// Read-only access for mode, title, geometry, and render queries.
    pub fn terminal(&self) -> &Terminal<'terminal_alloc, 'cb> {
        self.terminal
            .as_ref()
            .expect("live history set always owns its terminal")
    }

    /// Number of active manager-owned cuts.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no cuts are active.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Configured deterministic cut limit.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Cuts that may be acquired before deterministic manager expiry is needed.
    pub fn available(&self) -> usize {
        self.capacity - self.entries.len()
    }

    /// Process live VT bytes while all existing cuts remain copy-on-write.
    pub fn vt_write(&mut self, data: &[u8]) {
        self.terminal
            .as_mut()
            .expect("live history set always owns its terminal")
            .vt_write(data);
    }

    /// Set or clear the live terminal's physical scrollback line limit.
    pub fn set_scrollback_max_lines(&mut self, max: Option<usize>) -> crate::error::Result<()> {
        self.terminal
            .as_mut()
            .expect("live history set always owns its terminal")
            .set_scrollback_max_lines(max)?;
        Ok(())
    }

    /// Scroll the live terminal viewport without exposing terminal ownership.
    pub fn scroll_viewport(&mut self, scroll: ScrollViewport) {
        self.terminal
            .as_mut()
            .expect("live history set always owns its terminal")
            .scroll_viewport(scroll);
    }

    /// Resize the terminal, causing existing cuts to report native Resize.
    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> crate::error::Result<()> {
        self.terminal
            .as_mut()
            .expect("live history set always owns its terminal")
            .resize(cols, rows, cell_width_px, cell_height_px)
    }

    /// Reset the terminal, causing existing cuts to report native Reset.
    pub fn reset(&mut self) {
        self.terminal
            .as_mut()
            .expect("live history set always owns its terminal")
            .reset();
    }

    /// Acquire one authenticated cut without exposing its owning handles.
    pub fn acquire(&mut self, screen: ScreenKey) -> Result<LiveHistoryCut> {
        if self.entries.len() == self.capacity {
            return Err(Error::LimitExceeded);
        }
        let terminal = self.terminal().inner.as_raw();
        let entry = unsafe { LiveHistorySetEntry::new(self.allocator, terminal, screen) }?;
        if self
            .entries
            .iter()
            .any(|existing| existing.matches(entry.capability.as_bytes()))
        {
            return Err(Error::InvalidState);
        }
        let cut = entry.cut();
        self.entries.push(entry);
        Ok(cut)
    }

    /// Emit one unit for the cursor named by echoed opaque capability bytes.
    pub fn next<'buffer>(
        &mut self,
        capability: &[u8; TOKEN_LEN],
        options: HistoryOptions,
        buffer: &'buffer mut [u8],
    ) -> Result<HistoryEvent<'buffer>> {
        let terminal = self.terminal().inner.as_raw();
        let entry = self
            .entries
            .iter_mut()
            .find(|entry| entry.matches(capability))
            .ok_or(Error::InvalidHandle)?;
        let cursor = entry
            .cursor
            .as_ref()
            .expect("manager entry always owns its cursor");
        history_cursor_next(cursor.as_raw(), terminal, options, buffer)
    }

    /// Release a known cut. Unknown or already released bytes are invalid.
    pub fn release(&mut self, capability: &[u8; TOKEN_LEN]) -> Result<()> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.matches(capability))
            .ok_or(Error::InvalidHandle)?;
        drop(self.entries.swap_remove(index));
        Ok(())
    }

    /// Atomically acquire a cut and begin capture from the same terminal state.
    pub fn capture(
        &mut self,
        screen: ScreenKey,
        options: CaptureOptions,
    ) -> Result<LiveSetCapture<'_, 'terminal_alloc, 'cb, 'lease_alloc, 'static>> {
        unsafe { self.capture_inner(std::ptr::null(), screen, options) }
    }

    /// Atomically acquire a cut and begin capture using `allocator` for capture
    /// state. Manager lease/cursor state keeps its constructor allocator.
    pub fn capture_with_alloc<'capture_alloc, 'ctx: 'capture_alloc>(
        &mut self,
        allocator: &'capture_alloc Allocator<'ctx>,
        screen: ScreenKey,
        options: CaptureOptions,
    ) -> Result<LiveSetCapture<'_, 'terminal_alloc, 'cb, 'lease_alloc, 'capture_alloc>> {
        unsafe { self.capture_inner(allocator.to_raw(), screen, options) }
    }

    unsafe fn capture_inner<'set, 'capture_alloc>(
        &'set mut self,
        allocator: *const ffi::Allocator,
        screen: ScreenKey,
        options: CaptureOptions,
    ) -> Result<LiveSetCapture<'set, 'terminal_alloc, 'cb, 'lease_alloc, 'capture_alloc>> {
        let cut = self.acquire(screen)?;
        let terminal = self.terminal().inner.as_raw();
        let capture: Capture<'set, 'capture_alloc> =
            match unsafe { Capture::new_inner(allocator, terminal, options) } {
                Ok(capture) => capture,
                Err(error) => {
                    self.release(cut.capability())
                        .expect("newly acquired cut remains present");
                    return Err(error);
                }
            };
        Ok(LiveSetCapture {
            set: self,
            capture: Some(capture),
            cut,
            preserve_cut: false,
        })
    }

    /// Release every cut and return the still-live terminal.
    pub fn into_terminal(mut self) -> Terminal<'terminal_alloc, 'cb> {
        self.entries.clear();
        self.terminal
            .take()
            .expect("live history set always owns its terminal")
    }
}

impl Drop for LiveHistorySet<'_, '_, '_> {
    fn drop(&mut self) {
        self.entries.clear();
    }
}

/// Capture tied to an atomically retained manager history cut.
///
/// Dropping or aborting before READY releases the reserved cut. Once a READY
/// event has been returned, dropping the capture preserves the cut for lookup
/// through its echoed capability bytes.
#[derive(Debug)]
pub struct LiveSetCapture<'set, 'terminal_alloc: 'cb, 'cb, 'lease_alloc, 'capture_alloc> {
    set: &'set mut LiveHistorySet<'terminal_alloc, 'cb, 'lease_alloc>,
    capture: Option<Capture<'set, 'capture_alloc>>,
    cut: LiveHistoryCut,
    preserve_cut: bool,
}

impl<'set, 'terminal_alloc: 'cb, 'cb, 'lease_alloc, 'capture_alloc>
    LiveSetCapture<'set, 'terminal_alloc, 'cb, 'lease_alloc, 'capture_alloc>
{
    /// Emit one complete opaque capture record with retry-safe short buffers.
    pub fn next<'buffer>(&mut self, buffer: &'buffer mut [u8]) -> Result<CaptureEvent<'buffer>> {
        let event = self
            .capture
            .as_mut()
            .expect("live set capture remains active")
            .next(buffer)?;
        if matches!(&event.kind, CaptureEventKind::Ready { .. }) {
            self.preserve_cut = true;
        }
        Ok(event)
    }

    /// Return the atomic history cut only after READY was emitted successfully.
    pub fn ready_cut(&self) -> Option<&LiveHistoryCut> {
        self.preserve_cut.then_some(&self.cut)
    }

    /// Abort capture. A cut that has not reached READY is rolled back.
    pub fn abort(mut self) -> Result<()> {
        let result = self
            .capture
            .take()
            .expect("live set capture remains active")
            .abort();
        self.rollback_unready_cut();
        result
    }

    fn rollback_unready_cut(&mut self) {
        if !self.preserve_cut {
            self.set
                .release(self.cut.capability())
                .expect("unready capture cut remains present");
            self.preserve_cut = true;
        }
    }
}

impl Drop for LiveSetCapture<'_, '_, '_, '_, '_> {
    fn drop(&mut self) {
        drop(self.capture.take());
        self.rollback_unready_cut();
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
    use crate::fmt::{Format, Formatter, FormatterOptions};
    use std::{
        cell::{Cell, RefCell},
        ffi::c_void,
        rc::Rc,
    };

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

    fn semantic_terminal_bytes(terminal: &Terminal<'_, '_>) -> Vec<u8> {
        let options = FormatterOptions::new()
            .with_format(Format::Vt)
            .with_unwrap(false)
            .with_trim(false)
            .with_palette(true)
            .with_modes(true)
            .with_scrolling_region(true)
            .with_tabstops(true)
            .with_pwd(true)
            .with_keyboard(true)
            .with_cursor(true)
            .with_style(true)
            .with_hyperlink(true)
            .with_protection(true)
            .with_kitty_keyboard(true)
            .with_charsets(true);
        Formatter::new(terminal, options)
            .expect("semantic formatter")
            .format_alloc(None)
            .expect("semantic terminal bytes")
            .to_vec()
    }

    fn assert_semantically_equal(a: &Terminal<'_, '_>, b: &Terminal<'_, '_>) {
        macro_rules! compare {
            ($getter:ident) => {
                assert_eq!(
                    a.$getter().expect(stringify!($getter)),
                    b.$getter().expect(stringify!($getter)),
                    stringify!($getter)
                );
            };
        }

        compare!(cols);
        compare!(rows);
        compare!(width_px);
        compare!(height_px);
        compare!(cursor_x);
        compare!(cursor_y);
        compare!(is_cursor_pending_wrap);
        compare!(is_cursor_visible);
        compare!(cursor_style);
        compare!(kitty_keyboard_flags);
        compare!(active_screen);
        compare!(viewport_active);
        compare!(is_mouse_tracking);
        compare!(vt_processing_error);
        compare!(title);
        compare!(pwd);
        compare!(total_rows);
        compare!(scrollback_rows);

        let a_scrollbar = a.scrollbar().expect("scrollbar");
        let b_scrollbar = b.scrollbar().expect("scrollbar");
        assert_eq!(
            (a_scrollbar.total, a_scrollbar.offset, a_scrollbar.len),
            (b_scrollbar.total, b_scrollbar.offset, b_scrollbar.len),
            "scrollbar"
        );
        assert_eq!(
            semantic_terminal_bytes(a),
            semantic_terminal_bytes(b),
            "formatted cells, styles, row metadata, modes, and terminal extras"
        );
    }

    fn multipage_terminal() -> Terminal<'static, 'static> {
        let mut source = terminal(512, 4);
        source
            .set_scrollback_max_bytes(None)
            .expect("remove source scrollback byte cap");
        source
            .set_scrollback_max_lines(Some(4000))
            .expect("raise source scrollback line cap");
        let full_width_content = [b'x'; 500];
        for _ in 0..2000 {
            source.vt_write(&full_width_content);
            source.vt_write(b"\r\n");
        }
        source
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

    #[test]
    fn detached_capture_is_terminal_independent_and_row_bounded() {
        let mut source = terminal(80, 4);
        source
            .set_scrollback_max_bytes(None)
            .expect("unbounded source scrollback");
        for _ in 0..1000 {
            source.vt_write(b"\r\n");
        }
        let control_bytes = source.encode_snapshot().expect("control snapshot");
        let control = Terminal::decode_snapshot(&control_bytes)
            .expect("control decode")
            .terminal;

        let capture_options = CaptureOptions {
            max_pages: 64,
            ..CaptureOptions::default()
        };
        let mut capture = source
            .capture(capture_options)
            .expect("capture construction");
        let mut snapshot_bytes = Vec::new();
        loop {
            let required = match capture.next(&mut []) {
                Err(Error::OutOfSpace {
                    required_bytes,
                    required_rows: 0,
                }) => required_bytes,
                other => panic!("capture byte probe: {other:?}"),
            };
            let mut record = vec![0; required];
            let event = capture.next(&mut record).expect("capture record");
            let ready = matches!(event.kind, CaptureEventKind::Ready { .. });
            assert_eq!(event.rows, 0);
            snapshot_bytes.extend_from_slice(event.record);
            if ready {
                break;
            }
        }

        let CaptureDetachFailure {
            error,
            capture,
        } = capture
            .detach_ready(DetachOptions {
                max_total_bytes: 1,
                ..DetachOptions::default()
            })
            .expect_err("retained-byte limit must be transactional");
        assert_eq!(error, Error::LimitExceeded);
        let CaptureDetachFailure {
            error,
            capture,
        } = capture
            .detach_ready(DetachOptions {
                max_pages: 4096,
                max_total_bytes: 64 * 1024 * 1024,
                max_rows: 7,
            })
            .expect_err("row splitting must honor the original page cap");
        assert_eq!(error, Error::LimitExceeded);
        let mut continuation = capture
            .detach_ready(DetachOptions {
                max_pages: 4096,
                max_total_bytes: 64 * 1024 * 1024,
                max_rows: 64,
            })
            .expect("READY detachment");

        source.vt_write(b"live-after-ready");
        source.resize(81, 5, 0, 0).expect("source resize after detach");
        drop(source);

        let mut saw_row_gate = false;
        let mut history_rows = 0;
        loop {
            let mut record = vec![0; capture_options.max_record_bytes];
            let event = match continuation.next(
                ContinuationOptions { max_rows: 1 },
                &mut record,
            ) {
                Ok(event) => event,
                Err(Error::OutOfSpace {
                    required_bytes,
                    required_rows,
                }) if required_rows > 1 => {
                    saw_row_gate = true;
                    assert!(required_bytes <= record.len());
                    assert_eq!(
                        continuation
                            .next(ContinuationOptions { max_rows: 1 }, &mut record)
                            .unwrap_err(),
                        Error::OutOfSpace {
                            required_bytes,
                            required_rows,
                        },
                        "row shortage must not advance"
                    );
                    let event = continuation
                        .next(ContinuationOptions { max_rows: 64 }, &mut record)
                        .expect("row-budget retry");
                    assert_eq!(event.rows, required_rows);
                    event
                }
                Err(other) => panic!("owned continuation delivery: {other:?}"),
            };
            assert!(event.rows <= 64);
            if matches!(event.kind, CaptureEventKind::HistoryPage { .. }) {
                assert!(event.rows > 0);
                history_rows += event.rows;
            } else {
                assert_eq!(event.rows, 0);
            }
            let finished = matches!(event.kind, CaptureEventKind::Finish);
            snapshot_bytes.extend_from_slice(event.record);
            if finished {
                break;
            }
        }
        assert!(saw_row_gate);
        assert!(history_rows > 500, "many small-byte blank rows were charged");

        let decoded = Terminal::decode_snapshot(&snapshot_bytes)
            .expect("detached snapshot decode")
            .terminal;
        assert_semantically_equal(&control, &decoded);
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
        unbounded_scrollback_bytes: bool,
    ) -> (Terminal<'static, 'static>, usize, usize) {
        let mut state = DriveState::Before(
            Decoder::new(DecoderOptions::default()).expect("decoder construction"),
        );
        let mut offset = 0;
        let mut boundary = cut.min(data.len());
        let mut wrote_live = false;
        let mut history_pages = 0;
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
                        let mut stream =
                            continuation.replay().expect("one-shot continuation replay");
                        if unbounded_scrollback_bytes {
                            stream
                                .set_scrollback_max_bytes(None)
                                .expect("remove decoded scrollback byte cap");
                            stream
                                .set_scrollback_max_lines(Some(4000))
                                .expect("forward decoded scrollback line cap");
                        }
                        DriveState::After(stream)
                    }
                    Err(error) => panic!("decode before READY failed: {error}"),
                },
                DriveState::After(decoder) => match decoder.push(fragment) {
                    Ok(AfterReadyStep::NeedInput { decoder, progress })
                    | Ok(AfterReadyStep::Progress { decoder, progress }) => {
                        assert!(progress.consumed > 0);
                        offset += progress.consumed;
                        DriveState::After(decoder)
                    }
                    Ok(AfterReadyStep::HistoryBegin {
                        decoder,
                        progress,
                        count,
                        ..
                    }) => {
                        assert!(progress.consumed > 0);
                        if unbounded_scrollback_bytes {
                            assert!(count > 1, "fixture must declare multiple history pages");
                        }
                        offset += progress.consumed;
                        DriveState::After(decoder)
                    }
                    Ok(AfterReadyStep::HistoryPage {
                        mut decoder,
                        progress,
                        retained,
                        ..
                    }) => {
                        assert!(retained);
                        assert!(progress.consumed > 0);
                        offset += progress.consumed;
                        history_pages += 1;
                        if live_during_history && !wrote_live {
                            decoder.vt_write(b"live-between-decoded-history-pages\r\n");
                            wrote_live = true;
                        }
                        DriveState::After(decoder)
                    }
                    Ok(AfterReadyStep::Finish(finished)) => {
                        offset += finished.progress.consumed;
                        return (finished.terminal, offset, history_pages);
                    }
                    Err(error) => panic!("decode after READY failed: {error}"),
                },
                DriveState::Empty => unreachable!(),
            };
        }
    }

    fn stream_after_history_page(data: &[u8]) -> (DecodedStream<'static, 'static>, usize) {
        let mut decoder = Decoder::new(DecoderOptions::default()).expect("decoder construction");
        let mut offset = 0;
        let mut stream = loop {
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
                Ok(DecodeStep::Ready {
                    decoder: ready,
                    progress,
                }) => {
                    assert!(progress.consumed > 0);
                    offset += progress.consumed;
                    let continuation = ready.take_terminal::<'static>().expect("READY terminal");
                    let mut decoded = continuation.replay().expect("continuation replay");
                    decoded
                        .set_scrollback_max_bytes(None)
                        .expect("remove decoded scrollback byte cap");
                    decoded
                        .set_scrollback_max_lines(Some(4000))
                        .expect("forward decoded scrollback line cap");
                    break decoded;
                }
                Err(error) => panic!("decode before READY failed: {error}"),
            }
        };

        loop {
            match stream.push(&data[offset..]) {
                Ok(AfterReadyStep::NeedInput {
                    decoder: next,
                    progress,
                })
                | Ok(AfterReadyStep::Progress {
                    decoder: next,
                    progress,
                }) => {
                    assert!(progress.consumed > 0);
                    offset += progress.consumed;
                    stream = next;
                }
                Ok(AfterReadyStep::HistoryBegin {
                    decoder: next,
                    progress,
                    count,
                    ..
                }) => {
                    assert!(progress.consumed > 0);
                    assert!(count > 1, "fixture must declare multiple history pages");
                    offset += progress.consumed;
                    stream = next;
                }
                Ok(AfterReadyStep::HistoryPage {
                    decoder: next,
                    progress,
                    retained,
                    ..
                }) => {
                    assert!(retained);
                    assert!(progress.consumed > 0);
                    offset += progress.consumed;
                    return (next, offset);
                }
                Ok(AfterReadyStep::Finish(_)) => {
                    panic!("snapshot needs multiple history pages for invalidation test")
                }
                Err(error) => panic!("decode before history page failed: {error}"),
            }
        }
    }

    fn finish_discarded_history(
        mut stream: DecodedStream<'static, 'static>,
        data: &[u8],
        mut offset: usize,
    ) -> Terminal<'static, 'static> {
        let mut discarded = 0;
        loop {
            match stream.push(&data[offset..]) {
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
                }) => {
                    assert!(progress.consumed > 0);
                    offset += progress.consumed;
                    stream = next;
                }
                Ok(AfterReadyStep::HistoryPage {
                    decoder: next,
                    progress,
                    retained,
                    ..
                }) => {
                    assert!(progress.consumed > 0);
                    assert!(!retained);
                    discarded += 1;
                    offset += progress.consumed;
                    stream = next;
                }
                Ok(AfterReadyStep::Finish(finished)) => {
                    assert!(discarded > 0);
                    return finished.terminal;
                }
                Err(error) => panic!("invalidated decoder failed before FINISH: {error}"),
            }
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
            let (mut decoded, consumed, _) = decode_with_cut(&bytes, cut, false, false);
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

        let (mut decoded, consumed, _) = decode_with_cut(&transport, 1, true, false);
        assert_eq!(consumed, bytes.len());
        assert_eq!(&transport[consumed..], tail);
        decoded.vt_write(&transport[consumed..]);
    }

    #[test]
    fn decoded_stream_accepts_live_writes_between_history_pages() {
        let mut source = multipage_terminal();
        let bytes = capture_all(&mut source);
        let live = b"live-between-decoded-history-pages\r\n";
        source.vt_write(live);
        let (decoded, consumed, history_pages) = decode_with_cut(&bytes, 1, true, true);
        assert_eq!(consumed, bytes.len());
        assert!(history_pages > 1);
        assert_semantically_equal(&source, &decoded);
    }

    #[test]
    fn decoded_stream_reset_and_resize_discard_pending_history() {
        let mut source = multipage_terminal();
        let bytes = capture_all(&mut source);

        let (mut reset, offset) = stream_after_history_page(&bytes);
        let replies = Rc::new(RefCell::new(Vec::new()));
        let callback_replies = Rc::clone(&replies);
        reset
            .on_pty_write(move |_terminal, data| {
                callback_replies.borrow_mut().extend_from_slice(data);
            })
            .expect("decoded PTY callback");
        reset.vt_write(b"\x1b[5n");
        assert_eq!(&*replies.borrow(), b"\x1b[0n");
        reset.reset();
        reset.vt_write(b"active-after-reset");
        let reset_active = semantic_terminal_bytes(reset.terminal());
        let reset_cursor = (
            reset.terminal().cursor_x().expect("reset cursor x"),
            reset.terminal().cursor_y().expect("reset cursor y"),
        );
        let reset = finish_discarded_history(reset, &bytes, offset);
        assert_eq!(semantic_terminal_bytes(&reset), reset_active);
        assert_eq!(
            (
                reset.cursor_x().expect("finished reset cursor x"),
                reset.cursor_y().expect("finished reset cursor y"),
            ),
            reset_cursor
        );

        let (mut resized, offset) = stream_after_history_page(&bytes);
        resized.resize(21, 5, 0, 0).expect("decoded resize");
        resized.vt_write(b"active-after-resize");
        let resized_active = semantic_terminal_bytes(resized.terminal());
        let resized_geometry = (
            resized.terminal().cols().expect("resized cols"),
            resized.terminal().rows().expect("resized rows"),
        );
        let resized = finish_discarded_history(resized, &bytes, offset);
        assert_eq!(semantic_terminal_bytes(&resized), resized_active);
        assert_eq!(
            (
                resized.cols().expect("finished resized cols"),
                resized.rows().expect("finished resized rows"),
            ),
            resized_geometry
        );
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
        let checkpoint_bytes = *lease.checkpoint().as_bytes();
        assert_eq!(checkpoint_bytes.len(), TOKEN_LEN);
        assert_eq!(lease.checkpoint().as_bytes(), &checkpoint_bytes);
        let mut cursor = lease.into_cursor().expect("one-way cursor transfer");
        let cursor_capability = *cursor.capability().as_bytes();
        assert_eq!(cursor_capability.len(), TOKEN_LEN);
        assert_eq!(cursor.capability().as_bytes(), &cursor_capability);
        let mut importer = cursor
            .importer(&mut destination, HistoryOptions::default())
            .expect("history importer");
        let importer_capability = *importer.capability().as_bytes();
        assert_eq!(importer_capability.len(), TOKEN_LEN);
        assert_eq!(importer.capability().as_bytes(), &importer_capability);
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

    #[test]
    fn owned_live_cursor_pages_while_source_accepts_vt_writes() {
        if !capabilities().expect("capabilities").authenticated_tokens {
            return;
        }
        let mut source = terminal(20, 4);
        for row in 0..200 {
            source.vt_write(format!("row-{row:03}\r\n").as_bytes());
        }
        let encoded = source.encode_snapshot().expect("control snapshot");
        let mut control = Terminal::decode_snapshot(&encoded)
            .expect("control terminal")
            .terminal;

        let mut live = source
            .into_live_history_cursor(ScreenKey::PRIMARY)
            .expect("owned live cursor");
        live.set_scrollback_max_lines(Some(1000))
            .expect("forward owned scrollback line cap");
        let checkpoint = *live.checkpoint().as_bytes();
        let capability = *live.capability().as_bytes();
        assert_eq!(live.checkpoint().as_bytes(), &checkpoint);
        assert_eq!(live.capability().as_bytes(), &capability);

        let mut units = 0;
        let mut wrote_live = false;
        loop {
            let required = match live.next(HistoryOptions::default(), &mut []) {
                Ok(HistoryEvent::End) => break,
                Err(Error::OutOfSpace {
                    required_bytes,
                    required_rows: 0,
                }) => required_bytes,
                other => panic!("owned history probe: {other:?}"),
            };
            let mut unit = vec![0; required];
            assert!(matches!(
                live.next(HistoryOptions::default(), &mut unit)
                    .expect("owned history unit"),
                HistoryEvent::Unit { .. }
            ));
            units += 1;
            if !wrote_live {
                let input = b"live while source history pages\r\n";
                live.vt_write(input);
                control.vt_write(input);
                wrote_live = true;
            }
        }
        assert!(units > 1);
        assert!(wrote_live);

        let source = live.into_terminal();
        assert_semantically_equal(&source, &control);

        let mut invalidated = source
            .into_live_history_cursor(ScreenKey::PRIMARY)
            .expect("second owned live cursor");
        invalidated.reset();
        let mut unit = [0; 4096];
        assert!(matches!(
            invalidated.next(HistoryOptions::default(), &mut unit),
            Err(Error::Reset)
        ));
        let source = invalidated.into_terminal();
        let mut resized = source
            .into_live_history_cursor(ScreenKey::PRIMARY)
            .expect("resize invalidation cursor");
        resized.resize(21, 5, 0, 0).expect("owned resize");
        assert!(matches!(
            resized.next(HistoryOptions::default(), &mut unit),
            Err(Error::Resize)
        ));
        drop(resized.into_terminal());
    }

    #[test]
    fn live_history_set_serves_concurrent_cuts_and_atomic_ready_capture() {
        if !capabilities().expect("capabilities").authenticated_tokens {
            return;
        }
        let mut source = terminal(20, 4);
        let mut control = terminal(20, 4);
        for row in 0..200 {
            let input = format!("row-{row:03}\r\n");
            source.vt_write(input.as_bytes());
            control.vt_write(input.as_bytes());
        }

        let mut set = source
            .into_live_history_set(4)
            .expect("bounded live history set");
        assert_eq!((set.len(), set.capacity(), set.available()), (0, 4, 4));
        let first = set.acquire(ScreenKey::PRIMARY).expect("first cut");
        let between = b"live-between-cuts\r\n";
        set.vt_write(between);
        control.vt_write(between);
        let second = set.acquire(ScreenKey::PRIMARY).expect("second cut");
        assert_ne!(first.capability(), second.capability());

        let mut capture = set
            .capture(ScreenKey::PRIMARY, CaptureOptions::default())
            .expect("atomic cut and capture");
        assert!(capture.ready_cut().is_none());
        let ready = loop {
            let required = match capture.next(&mut []) {
                Err(Error::OutOfSpace {
                    required_bytes,
                    required_rows: 0,
                }) => required_bytes,
                other => panic!("manager capture probe: {other:?}"),
            };
            let mut record = vec![0; required];
            let event = capture
                .next(&mut record)
                .expect("complete manager capture record");
            if matches!(event.kind, CaptureEventKind::Ready { .. }) {
                break *capture
                    .ready_cut()
                    .expect("atomic cut exposed exactly at READY");
            }
        };
        drop(capture);
        assert_eq!((set.len(), set.available()), (3, 1));

        let fourth = set.acquire(ScreenKey::PRIMARY).expect("fourth cut");
        assert_eq!(set.acquire(ScreenKey::PRIMARY), Err(Error::LimitExceeded));
        set.release(fourth.capability())
            .expect("release fourth cut");

        let after_ready = b"live-after-atomic-ready\r\n";
        set.vt_write(after_ready);
        control.vt_write(after_ready);
        for capability in [
            *first.capability(),
            *second.capability(),
            *ready.capability(),
        ] {
            let required = match set.next(&capability, HistoryOptions::default(), &mut []) {
                Err(Error::OutOfSpace {
                    required_bytes,
                    required_rows: 0,
                }) => required_bytes,
                other => panic!("manager history probe: {other:?}"),
            };
            let mut unit = vec![0; required];
            assert!(matches!(
                set.next(&capability, HistoryOptions::default(), &mut unit)
                    .expect("manager history unit"),
                HistoryEvent::Unit { .. }
            ));
        }

        let unknown = [0_u8; TOKEN_LEN];
        assert!(matches!(
            set.next(&unknown, HistoryOptions::default(), &mut []),
            Err(Error::InvalidHandle)
        ));
        set.release(first.capability()).expect("release first cut");
        assert!(matches!(
            set.next(first.capability(), HistoryOptions::default(), &mut []),
            Err(Error::InvalidHandle)
        ));
        set.release(second.capability())
            .expect("release second cut");
        set.release(ready.capability()).expect("release READY cut");
        assert!(set.is_empty());

        let active_before_abort = set.len();
        set.capture(ScreenKey::PRIMARY, CaptureOptions::default())
            .expect("pre-READY capture")
            .abort()
            .expect("pre-READY abort");
        assert_eq!(set.len(), active_before_abort);

        let source = set.into_terminal();
        assert_semantically_equal(&source, &control);
    }

    #[test]
    fn live_history_set_construction_failure_returns_unchanged_terminal() {
        let mut source = terminal(20, 4);
        let mut control = terminal(20, 4);
        source.vt_write(b"canonical-before-manager");
        control.vt_write(b"canonical-before-manager");

        let failure = source
            .into_live_history_set(usize::MAX)
            .expect_err("impossible reservation");
        assert_eq!(failure.error, Error::OutOfMemory);
        let mut recovered = failure.terminal;
        recovered.vt_write(b"-still-live");
        control.vt_write(b"-still-live");
        assert_semantically_equal(&recovered, &control);

        let failure = recovered
            .into_live_history_set(0)
            .expect_err("zero capacity");
        assert_eq!(failure.error, Error::InvalidState);
        assert_semantically_equal(&failure.terminal, &control);
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

            state.calls.set(0);
            state.fail_after.set(1);
            assert_eq!(
                terminal(20, 4)
                    .into_live_history_cursor_with_alloc(&allocator, ScreenKey::PRIMARY,)
                    .unwrap_err(),
                Error::OutOfMemory
            );
            assert_eq!(state.active.get(), 0);

            state.calls.set(0);
            state.fail_after.set(usize::MAX);
            let live = terminal(20, 4)
                .into_live_history_cursor_with_alloc(&allocator, ScreenKey::PRIMARY)
                .expect("allocator-owned live cursor");
            assert_eq!(state.active.get(), 2);
            let recovered_terminal = live.into_terminal();
            assert_eq!(state.active.get(), 0);
            drop(recovered_terminal);

            let mut set = terminal(20, 4)
                .into_live_history_set_with_alloc(&allocator, 4)
                .expect("allocator-owned live history set");
            state.calls.set(0);
            state.fail_after.set(1);
            assert_eq!(
                set.acquire(ScreenKey::PRIMARY).unwrap_err(),
                Error::OutOfMemory
            );
            assert_eq!((set.len(), state.active.get()), (0, 0));

            state.calls.set(0);
            state.fail_after.set(2);
            assert_eq!(
                set.capture_with_alloc(&allocator, ScreenKey::PRIMARY, CaptureOptions::default(),)
                    .unwrap_err(),
                Error::OutOfMemory
            );
            assert_eq!((set.len(), state.active.get()), (0, 0));

            state.calls.set(0);
            state.fail_after.set(usize::MAX);
            drop(
                set.capture_with_alloc(&allocator, ScreenKey::PRIMARY, CaptureOptions::default())
                    .expect("allocator-owned atomic capture"),
            );
            assert_eq!((set.len(), state.active.get()), (0, 0));

            let cut = set.acquire(ScreenKey::PRIMARY).expect("manager cut");
            assert_eq!((set.len(), state.active.get()), (1, 2));
            assert_eq!(set.release(cut.capability()), Ok(()));
            assert_eq!((set.len(), state.active.get()), (0, 0));
            drop(set);
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
