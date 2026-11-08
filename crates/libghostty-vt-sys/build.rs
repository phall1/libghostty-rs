use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned ghostty commit. Update this to pull a newer version.
const GHOSTTY_REPO: &str = "https://github.com/phall1/ghostty.git";
// vt-1.0.0 plus the narrowly retained phux APIs, pinned immutably.
const GHOSTTY_COMMIT: &str = "511a1fabb48ab00715cdb22ce13a7191217e8086";

/// File name of the static archive on Windows. Ghostty installs it under this
/// name for every Windows ABI so it does not collide with `ghostty-vt.lib`,
/// the import library for `ghostty-vt.dll`. Validation and link emission must
/// agree on it, or the build silently links the import library instead.
const WINDOWS_STATIC_LIB_FILE: &str = "ghostty-vt-static.lib";

#[derive(Clone, Copy)]
enum LinkMode {
    Dynamic,
    Static,
}

impl LinkMode {
    fn current() -> Self {
        if cfg!(feature = "link-dynamic") {
            Self::Dynamic
        } else {
            Self::Static
        }
    }

    fn artifact_kind(self) -> &'static str {
        match self {
            Self::Dynamic => "shared library",
            Self::Static => "static library",
        }
    }

    fn matches_library(self, target: &str, file_name: &str) -> bool {
        match self {
            Self::Dynamic => {
                if target.contains("darwin") {
                    file_name.starts_with("libghostty-vt") && file_name.ends_with(".dylib")
                } else if target.contains("windows") {
                    file_name == "ghostty-vt.lib"
                        || file_name == "ghostty-vt.dll"
                        || file_name == "libghostty-vt.dll.lib"
                        || file_name == "libghostty-vt.dll.a"
                } else {
                    file_name == "libghostty-vt.so" || file_name.starts_with("libghostty-vt.so.")
                }
            }
            Self::Static => {
                if target.contains("windows") {
                    file_name == WINDOWS_STATIC_LIB_FILE
                } else {
                    file_name == "libghostty-vt.a"
                }
            }
        }
    }

    #[cfg(feature = "pkg-config")]
    fn pkg_config_name(self) -> &'static str {
        match self {
            Self::Dynamic => "libghostty-vt",
            Self::Static => "libghostty-vt-static",
        }
    }
}

fn main() {
    // Switching from documentation/type-only work back to a native build must
    // restore link metadata instead of reusing the early-return output.
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    // docs.rs has no Zig toolchain. The checked-in bindings in src/bindings.rs
    // are enough for generating documentation, so skip the entire native
    // build when running under docs.rs.
    if env::var("DOCS_RS").is_ok() {
        return;
    }

    // Miri cannot load or call the native Ghostty library. The Miri suite stays
    // within Rust-owned seams, so there is nothing to build or link for it.
    if env::var("CARGO_CFG_MIRI").is_ok() {
        return;
    }

    let link_mode = LinkMode::current();
    // Cargo always sets TARGET for build scripts, so read it once here and
    // hand it to whichever path runs rather than re-reading it per call site.
    let target = env::var("TARGET").expect("TARGET must be set");

    // Wasm consumers link the archive into their own module, not a native
    // shared library. Check before pkg-config as well as the source build.
    assert!(
        !target.starts_with("wasm") || matches!(link_mode, LinkMode::Static),
        "Wasm targets support static linking only; disable the link-dynamic feature"
    );

    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SYS_CPU");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SYS_OPTIMIZE");
    println!("cargo:rerun-if-env-changed=GHOSTTY_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SYS_LIB_DIR");
    println!("cargo:rerun-if-env-changed=GHOSTTY_ZIG_SYSTEM_DIR");
    println!("cargo:rerun-if-env-changed=ZIG");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=HOST");
    println!("cargo:rerun-if-env-changed=DEBUG");
    println!("cargo:rerun-if-env-changed=OPT_LEVEL");
    for path in [
        "build.rs",
        "build_vendored.rs",
        "download.rs",
        "prebuilt.rs",
        "shared_build.rs",
        "link_emit.rs",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }

    // An explicit source override should stay authoritative even when the
    // pkg-config feature is enabled, so local Ghostty checkouts remain easy to
    // test against.
    if env::var_os("GHOSTTY_SOURCE_DIR").is_some() {
        build_vendored(link_mode, &target);
        return;
    }

    if let Some(dir) = env::var_os("LIBGHOSTTY_VT_SYS_LIB_DIR") {
        assert!(
            !dir.is_empty(),
            "LIBGHOSTTY_VT_SYS_LIB_DIR must not be empty"
        );
        link_prebuilt(link_mode, &target, Path::new(&dir));
        return;
    }

    // When the pkg-config feature is enabled, prefer an installed library over
    // fetching Ghostty. This fork's extra C APIs require the matching engine;
    // an arbitrary vanilla 1.0 installation is not equivalent.
    #[cfg(feature = "pkg-config")]
    if try_pkg_config(link_mode, &target) {
        return;
    }

    if try_download_prebuilt(link_mode, &target) {
        return;
    }

    build_vendored(link_mode, &target);
}

include!("build_vendored.rs");
include!("download.rs");
include!("prebuilt.rs");
include!("shared_build.rs");
include!("link_emit.rs");

/// Decide which Zig `OptimizeMode` to pass to `zig build`.
///
/// The `LIBGHOSTTY_VT_SYS_OPTIMIZE` environment variable overrides this unconditionally; accepted
/// values are the four Zig `OptimizeMode` names (`Debug`, `ReleaseSafe`, `ReleaseFast`,
/// `ReleaseSmall`).
///
/// Defaults to `ReleaseFast` for optimized builds. If `DEBUG` is `true` (as cargo sets for the
/// `dev` profile), `Debug` mode is used. Otherwise, if `OPT_LEVEL` is `s` or `z`, `ReleaseSmall`
/// is used.
fn zig_optimize_mode() -> &'static str {
    if let Ok(override_mode) = env::var("LIBGHOSTTY_VT_SYS_OPTIMIZE") {
        return match override_mode.as_str() {
            "Debug" => "Debug",
            "ReleaseSafe" => "ReleaseSafe",
            "ReleaseFast" => "ReleaseFast",
            "ReleaseSmall" => "ReleaseSmall",
            other => panic!(
                "LIBGHOSTTY_VT_SYS_OPTIMIZE must be one of Debug, ReleaseSafe, ReleaseFast, ReleaseSmall (got '{other}')"
            ),
        };
    }

    if env::var("DEBUG").as_deref() == Ok("true") {
        return "Debug";
    }

    match env::var("OPT_LEVEL").as_deref() {
        Ok("s") | Ok("z") => "ReleaseSmall",
        _ => "ReleaseFast",
    }
}

/// Clone ghostty at the pinned commit into OUT_DIR/ghostty-src.
/// Reuses an existing clone if the commit matches.
fn fetch_ghostty(out_dir: &Path) -> PathBuf {
    let src_dir = out_dir.join("ghostty-src");
    let stamp = src_dir.join(".ghostty-commit");

    // Skip fetch if we already have the right commit.
    if stamp.exists()
        && let Ok(existing) = std::fs::read_to_string(&stamp)
        && existing.trim() == GHOSTTY_COMMIT
    {
        return src_dir;
    }

    // Clean and clone fresh.
    if src_dir.exists() {
        std::fs::remove_dir_all(&src_dir)
            .unwrap_or_else(|e| panic!("failed to remove {}: {e}", src_dir.display()));
    }

    eprintln!("Fetching ghostty {GHOSTTY_COMMIT} ...");

    let mut clone = Command::new("git");
    // Cargo's nested OUT_DIR plus Ghostty's fuzz corpus names can exceed
    // Windows MAX_PATH. Scope long-path support to this fetched repository.
    clone
        .arg("clone")
        .arg("--config")
        .arg("core.longpaths=true")
        .arg("--filter=blob:none")
        .arg("--no-checkout")
        .arg(GHOSTTY_REPO)
        .arg(&src_dir);
    run(clone, "git clone ghostty");

    let mut checkout = Command::new("git");
    checkout
        .arg("checkout")
        .arg(GHOSTTY_COMMIT)
        .current_dir(&src_dir);
    run(checkout, "git checkout ghostty commit");

    std::fs::write(&stamp, GHOSTTY_COMMIT).unwrap_or_else(|e| panic!("failed to write stamp: {e}"));

    src_dir
}

fn run(mut command: Command, context: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to execute {context}: {error}"));
    assert!(status.success(), "{context} failed with status {status}");
}

/// Returns directories to search for the built library artifact.
/// On Windows, Zig may place the DLL in `bin/` and the import lib in `lib/`,
/// so both are included.
fn library_search_dirs(target: &str, install_prefix: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![install_prefix.join("lib")];
    if target.contains("windows") {
        dirs.push(install_prefix.join("bin"));
    }
    dirs
}

/// The platform directory inside `ghostty-vt.xcframework` for an iOS Rust
/// target, or `None` for targets that build directly via `-Dtarget`. Ghostty
/// emits an arm64-only simulator library (what the simulator runs on Apple
/// silicon), so x86_64-apple-ios is not supported here.
fn ios_xcframework_platform(target: &str) -> Option<&'static str> {
    match target {
        "aarch64-apple-ios" => Some("ios-arm64"),
        "aarch64-apple-ios-sim" => Some("ios-arm64-simulator"),
        _ => None,
    }
}

/// Copy an xcframework platform's static lib + headers into the flat
/// `<prefix>/lib/libghostty-vt.a` and `<prefix>/include` layout the link
/// emission expects, replacing the host-native artifacts the emit installed.
fn extract_xcframework_lib(install_prefix: &Path, platform: &str) {
    let platform_dir = install_prefix
        .join("lib")
        .join("ghostty-vt.xcframework")
        .join(platform);
    // Ghostty emits iOS xcframework slices only when it detects the iOS SDK,
    // silently skipping them otherwise, so a missing directory here almost
    // always means the SDK is absent rather than a build failure.
    assert!(
        platform_dir.is_dir(),
        "expected xcframework platform dir {platform} at {}; \
         ghostty emits iOS slices only when the iOS SDK is detected, \
         so install it via Xcode (Settings > Components)",
        platform_dir.display()
    );
    // ghostty names the Apple static libraries `libghostty-vt-fat.a`.
    let src_lib = platform_dir.join("libghostty-vt-fat.a");
    assert!(
        src_lib.exists(),
        "expected static lib at {}",
        src_lib.display()
    );

    let lib_dir = install_prefix.join("lib");
    let dest_lib = lib_dir.join("libghostty-vt.a");
    std::fs::copy(&src_lib, &dest_lib).unwrap_or_else(|error| {
        panic!(
            "failed to copy {} -> {}: {error}",
            src_lib.display(),
            dest_lib.display()
        )
    });

    // Headers are arch-independent, but prefer the platform's own copy.
    let headers = platform_dir.join("Headers");
    if headers.is_dir() {
        copy_dir_all(&headers, &install_prefix.join("include"));
    }
}

/// Recursively copy a directory tree (merging into `dst` if it exists).
fn copy_dir_all(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", dst.display()));
    let entries = std::fs::read_dir(src)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", src.display()));
    for entry in entries {
        let entry = entry
            .unwrap_or_else(|error| panic!("failed to read entry in {}: {error}", src.display()));
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let file_type = entry
            .file_type()
            .unwrap_or_else(|error| panic!("failed to stat {}: {error}", from.display()));
        if file_type.is_dir() {
            copy_dir_all(&from, &to);
        } else {
            std::fs::copy(&from, &to)
                .unwrap_or_else(|error| panic!("failed to copy {}: {error}", from.display()));
        }
    }
}

fn zig_target(target: &str) -> String {
    let value = match target {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        "aarch64-apple-darwin" => "aarch64-macos-none",
        "x86_64-apple-darwin" => "x86_64-macos-none",
        "x86_64-pc-windows-gnu" => "x86_64-windows-gnu",
        "aarch64-pc-windows-gnullvm" => "aarch64-windows-gnu",
        "x86_64-pc-windows-msvc" => "x86_64-windows-msvc",
        "aarch64-pc-windows-msvc" => "aarch64-windows-msvc",
        "aarch64-linux-android" => "aarch64-linux-android",
        "x86_64-linux-android" => "x86_64-linux-android",
        "wasm32-unknown-unknown" => "wasm32-freestanding",
        "wasm64-unknown-unknown" => "wasm64-freestanding",
        other => panic!("unsupported Rust target for vendored build: {other}"),
    };
    value.to_owned()
}
