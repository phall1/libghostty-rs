//! Query compile-time build configuration of libghostty-vt.
//!
//! These values reflect the options the library was built with and are constant for the lifetime of the process.
//!
//! # Example
//! ```rust
//! use libghostty_vt::{Error, build_info::*};
//!
//! fn print_build_info() -> Result<(), Error> {
//!     println!(
//!         "SIMD: {}",
//!         if supports_simd().unwrap_or(false) { "enabled" } else { "disabled" }
//!     );
//!     println!(
//!         "Kitty graphics: {}",
//!         if supports_kitty_graphics().unwrap_or(false) { "enabled" } else { "disabled" }
//!     );
//!     println!(
//!         "Tmux control mode: {}",
//!         if supports_tmux_control_mode().unwrap_or(false) { "enabled" } else { "disabled" }
//!     );
//!     Ok(())
//! }
//! ```

use std::mem::MaybeUninit;

use crate::{
    error::{Error, Result, from_result},
    ffi,
};

/// Whether SIMD-accelerated code paths are enabled.
pub fn supports_simd() -> Result<bool> {
    build_info(ffi::GhosttyBuildInfo_GHOSTTY_BUILD_INFO_SIMD)
}

/// Whether Kitty graphics protocol support is available.
pub fn supports_kitty_graphics() -> Result<bool> {
    build_info(ffi::GhosttyBuildInfo_GHOSTTY_BUILD_INFO_KITTY_GRAPHICS)
}

/// Whether tmux control mode support is available.
pub fn supports_tmux_control_mode() -> Result<bool> {
    build_info(ffi::GhosttyBuildInfo_GHOSTTY_BUILD_INFO_TMUX_CONTROL_MODE)
}

/// The optimization mode the library was built with.
pub fn optimize_mode() -> Result<OptimizeMode> {
    build_info::<ffi::GhosttyOptimizeMode>(ffi::GhosttyBuildInfo_GHOSTTY_BUILD_INFO_OPTIMIZE)
        .and_then(|v| v.try_into().map_err(|_| Error::InvalidValue))
}

/// The full version string (e.g. "1.2.3" or "1.2.3-dev+abcdef").
pub fn version_string() -> Result<&'static str> {
    build_info::<ffi::GhosttyString>(ffi::GhosttyBuildInfo_GHOSTTY_BUILD_INFO_VERSION_STRING)
        // SAFETY: API guarantees
        .map(|s| unsafe { s.to_str() })
}
/// The major version number.
pub fn major_version() -> Result<usize> {
    build_info(ffi::GhosttyBuildInfo_GHOSTTY_BUILD_INFO_VERSION_MAJOR)
}
/// The minor version number.
pub fn minor_version() -> Result<usize> {
    build_info(ffi::GhosttyBuildInfo_GHOSTTY_BUILD_INFO_VERSION_MINOR)
}
/// The patch version number.
pub fn patch_version() -> Result<usize> {
    build_info(ffi::GhosttyBuildInfo_GHOSTTY_BUILD_INFO_VERSION_PATCH)
}
/// The build metadata string (e.g. commit hash).
///
/// Has zero length if no build metadata is present.
pub fn build_version() -> Result<&'static str> {
    build_info::<ffi::GhosttyString>(ffi::GhosttyBuildInfo_GHOSTTY_BUILD_INFO_VERSION_BUILD)
        // SAFETY: API guarantees
        .map(|s| unsafe { s.to_str() })
}

fn build_info<T>(tag: ffi::GhosttyBuildInfo) -> Result<T> {
    let mut value = MaybeUninit::zeroed();
    let result = unsafe { ffi::ghostty_build_info(tag, std::ptr::from_mut(&mut value).cast()) };
    from_result(result)?;
    // SAFETY: Value should be initialized after successful call.
    Ok(unsafe { value.assume_init() })
}

/// The optimization mode libghostty is compiled with.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, int_enum::IntEnum)]
pub enum OptimizeMode {
    /// Debug mode.
    ///
    /// Very slow with all safety checks enabled.
    Debug = ffi::GhosttyOptimizeMode_GHOSTTY_OPTIMIZE_DEBUG,
    /// Release mode optimized for safety.
    ///
    /// Faster than debug due to better code generation,
    /// but still very slow due to active safety checks.
    ReleaseSafe = ffi::GhosttyOptimizeMode_GHOSTTY_OPTIMIZE_RELEASE_SAFE,
    /// Release mode optimized for size.
    ReleaseSmall = ffi::GhosttyOptimizeMode_GHOSTTY_OPTIMIZE_RELEASE_SMALL,
    /// Release mode optimized for speed.
    ReleaseFast = ffi::GhosttyOptimizeMode_GHOSTTY_OPTIMIZE_RELEASE_FAST,
}
