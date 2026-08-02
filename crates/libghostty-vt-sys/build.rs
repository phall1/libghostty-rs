use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned ghostty commit. Update this to pull a newer version.
const GHOSTTY_REPO: &str = "https://github.com/phall1/ghostty.git";
const GHOSTTY_COMMIT: &str = "83fa938a66ddd1190e2591771373652b854260f5";

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
                    file_name == "ghostty-vt-static.lib"
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
    // docs.rs has no Zig toolchain. The checked-in bindings in src/bindings.rs
    // are enough for generating documentation, so skip the entire native
    // build when running under docs.rs.
    if env::var("DOCS_RS").is_ok() {
        return;
    }

    let link_mode = LinkMode::current();

    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SYS_OPTIMIZE");
    println!("cargo:rerun-if-env-changed=GHOSTTY_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=GHOSTTY_ZIG_SYSTEM_DIR");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=HOST");
    println!("cargo:rerun-if-env-changed=DEBUG");
    println!("cargo:rerun-if-env-changed=OPT_LEVEL");
    println!("cargo:rerun-if-changed=crates/libghostty-vt-sys/build.rs");

    // An explicit source override should stay authoritative even when the
    // pkg-config feature is enabled, so local Ghostty checkouts remain easy to
    // test against.
    if env::var_os("GHOSTTY_SOURCE_DIR").is_some() {
        build_vendored(link_mode);
        return;
    }

    // When the pkg-config feature is enabled, prefer an installed library over
    // fetching Ghostty. libghostty is pre-1.0, so this crate intentionally does
    // not promise compatibility with every installed C API revision.
    #[cfg(feature = "pkg-config")]
    if try_pkg_config(link_mode) {
        return;
    }

    build_vendored(link_mode);
}

/// Build libghostty-vt from source via zig. The zig build itself generates
/// shared and static artifacts plus pkg-config files in `share/pkgconfig/`.
fn build_vendored(link_mode: LinkMode) {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must be set"));
    let target = env::var("TARGET").expect("TARGET must be set");
    let host = env::var("HOST").expect("HOST must be set");

    // Locate ghostty source: env override > fetch into OUT_DIR.
    let ghostty_dir = match env::var("GHOSTTY_SOURCE_DIR") {
        Ok(dir) => {
            let p = PathBuf::from(dir);
            assert!(
                p.join("build.zig").exists(),
                "GHOSTTY_SOURCE_DIR does not contain build.zig: {}",
                p.display()
            );
            p
        }
        Err(_) => fetch_ghostty(&out_dir),
    };

    // Build libghostty-vt via zig.
    let install_prefix = out_dir.join("ghostty-install");
    let zig_cache_dir = out_dir.join("zig-cache");
    let zig_global_cache_dir = out_dir.join("zig-global-cache");

    let optimize = zig_optimize_mode();

    // iOS targets must be built through ghostty's emit-xcframework Apple path
    // (`initStaticAppleUniversal`), NOT a flat `-Dtarget=<ios> --sysroot=<sdk>`
    // build. Two reasons a flat per-target build fails (both confirmed on a
    // macOS-15 runner, where the host toolchain itself is healthy):
    //   1. CPU features. A generic `-Dtarget=aarch64-ios-simulator` selects a
    //      baseline CPU, so simdutf's `always_inline` NEON intrinsics
    //      (`vdupq_n_u8`, …) fail to compile ("requires target feature
    //      'altnzcv'"). The xcframework path pins the right per-slice CPU model.
    //   2. Global sysroot. `--sysroot` is global to `zig build`, so it poisons
    //      ghostty's NATIVE mid-build codegen tools (uucode/props unigen) and
    //      doubles library paths. The xcframework path runs host-native and sets
    //      the sysroot per Apple slice instead.
    // So for iOS we emit the xcframework host-native and extract the matching
    // static slice (below). Other targets keep the direct `-Dtarget` build.
    let ios_slice = ios_xcframework_slice(&target);

    let mut build = Command::new("zig");
    build
        .arg("build")
        .arg("-Demit-lib-vt=true")
        .arg(format!("-Doptimize={optimize}"))
        .arg(if ios_slice.is_some() {
            "-Demit-xcframework=true"
        } else {
            "-Demit-xcframework=false"
        })
        .arg("-Dapp-runtime=none")
        .arg("--prefix")
        .arg(&install_prefix)
        .arg("--cache-dir")
        .arg(&zig_cache_dir)
        .current_dir(&ghostty_dir);

    // Package managers can provide Ghostty's Zig package cache ahead of time
    // and ask Zig to resolve packages from that immutable store path instead
    // of fetching during this Cargo build script.
    if let Ok(dir) = env::var("GHOSTTY_ZIG_SYSTEM_DIR") {
        assert!(
            !dir.is_empty(),
            "GHOSTTY_ZIG_SYSTEM_DIR must not be empty when set"
        );
        let zig_system_dir = PathBuf::from(dir);
        assert!(
            zig_system_dir.exists(),
            "GHOSTTY_ZIG_SYSTEM_DIR does not exist: {}",
            zig_system_dir.display()
        );
        build
            .arg("--system")
            .arg(&zig_system_dir)
            .arg("--global-cache-dir")
            .arg(&zig_global_cache_dir);
    }

    // Pass -Dtarget only for non-iOS cross targets. For native builds, let zig
    // auto-detect the host. iOS is the exception: its slices are built
    // host-native inside the xcframework emit, so it must not pass -Dtarget (nor
    // a global --sysroot, which would poison ghostty's native codegen tools).
    if target != host && ios_slice.is_none() {
        let zig_target = zig_target(&target);
        build.arg(format!("-Dtarget={zig_target}"));
    }

    run(build, "zig build");

    // For iOS, overwrite the host-native flat artifacts the emit also installs
    // with the requested slice from the emitted xcframework, so the link
    // emission below finds the right-arch libghostty-vt.a + headers.
    if let Some(slice) = ios_slice {
        extract_xcframework_slice(&install_prefix, slice);
    }

    let lib_dir = install_prefix.join("lib");
    let include_dir = install_prefix.join("include");
    let search_dirs = library_search_dirs(&target, &install_prefix);
    if ios_slice.is_none() {
        warn_unused_xcframework(&lib_dir);
    }

    let has_requested_library = search_dirs.iter().any(|dir| {
        std::fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", dir.display()))
            .any(|entry| {
                let entry = entry.unwrap_or_else(|error| {
                    panic!("failed to read entry from {}: {error}", dir.display())
                });
                let file_name = entry.file_name();
                let Some(file_name) = file_name.to_str() else {
                    return false;
                };

                link_mode.matches_library(&target, file_name)
            })
    });
    assert!(
        has_requested_library,
        "expected libghostty-vt {} in one of {:?}",
        link_mode.artifact_kind(),
        search_dirs
    );
    assert!(
        include_dir.join("ghostty").join("vt.h").exists(),
        "expected header at {}",
        include_dir.join("ghostty").join("vt.h").display()
    );

    for dir in &search_dirs {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }
    match link_mode {
        LinkMode::Dynamic => println!("cargo:rustc-link-lib=dylib=ghostty-vt"),
        LinkMode::Static => println!("cargo:rustc-link-lib=static=ghostty-vt"),
    }
    emit_include_metadata(&[include_dir]);
}

fn warn_unused_xcframework(lib_dir: &Path) {
    let xcframework = lib_dir.join("ghostty-vt.xcframework");
    if xcframework.exists() {
        println!(
            "cargo:warning=unused libghostty-vt XCFramework emitted at {}; Cargo links the dylib or archive directly",
            xcframework.display()
        );
    }
}

#[cfg(feature = "pkg-config")]
fn try_pkg_config(link_mode: LinkMode) -> bool {
    let mut config = pkg_config::Config::new();
    let lib = match link_mode {
        LinkMode::Dynamic => config.probe(link_mode.pkg_config_name()),
        LinkMode::Static => config
            .statik(true)
            .cargo_metadata(false)
            .probe(link_mode.pkg_config_name()),
    };
    let lib = match lib {
        Ok(lib) => lib,
        Err(_) => return false,
    };

    if let LinkMode::Static = link_mode {
        emit_static_pkg_config_metadata(&lib);
    }
    emit_include_metadata(&lib.include_paths);
    true
}

#[cfg(feature = "pkg-config")]
fn emit_static_pkg_config_metadata(lib: &pkg_config::Library) {
    for path in &lib.link_paths {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    for path in &lib.link_files {
        if let Some(parent) = path.parent() {
            println!("cargo:rustc-link-search=native={}", parent.display());
        }
    }
    for path in &lib.framework_paths {
        println!("cargo:rustc-link-search=framework={}", path.display());
    }
    for framework in &lib.frameworks {
        println!("cargo:rustc-link-lib=framework={framework}");
    }

    println!("cargo:rustc-link-lib=static=ghostty-vt");
    for library in &lib.libs {
        if library != "ghostty-vt" {
            println!("cargo:rustc-link-lib={library}");
        }
    }
    for args in &lib.ld_args {
        if !args.is_empty() {
            println!("cargo:rustc-link-arg=-Wl,{}", args.join(","));
        }
    }
}

fn emit_include_metadata(include_paths: &[PathBuf]) {
    if include_paths.is_empty() {
        return;
    }

    let joined = env::join_paths(include_paths)
        .unwrap_or_else(|error| panic!("failed to join include paths for cargo metadata: {error}"));
    println!("cargo:include={}", joined.to_string_lossy());
}

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
    clone
        .arg("clone")
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

/// The `ghostty-vt.xcframework` slice directory for an iOS Rust target, or
/// `None` for non-iOS targets (which build directly via `-Dtarget`). The
/// default xcframework target emits an arm64-only simulator slice, which is the
/// arch the simulator runs on Apple silicon; x86_64-apple-ios (Intel sim) has
/// no slice and is not supported by this path.
fn ios_xcframework_slice(target: &str) -> Option<&'static str> {
    match target {
        "aarch64-apple-ios" => Some("ios-arm64"),
        "aarch64-apple-ios-sim" => Some("ios-arm64-simulator"),
        _ => None,
    }
}

/// Copy the requested xcframework slice's static lib + headers into the flat
/// `<prefix>/lib/libghostty-vt.a` and `<prefix>/include` locations the link
/// emission expects, overwriting the host-native flat artifacts the emit also
/// produced.
fn extract_xcframework_slice(install_prefix: &Path, slice: &str) {
    let slice_dir = install_prefix
        .join("lib")
        .join("ghostty-vt.xcframework")
        .join(slice);
    assert!(
        slice_dir.is_dir(),
        "expected xcframework slice {slice} at {}",
        slice_dir.display()
    );
    // ghostty names the Apple static slices `libghostty-vt-fat.a`.
    let slice_lib = slice_dir.join("libghostty-vt-fat.a");
    assert!(
        slice_lib.exists(),
        "expected slice static lib at {}",
        slice_lib.display()
    );

    let lib_dir = install_prefix.join("lib");
    let dest_lib = lib_dir.join("libghostty-vt.a");
    std::fs::copy(&slice_lib, &dest_lib).unwrap_or_else(|error| {
        panic!(
            "failed to copy {} -> {}: {error}",
            slice_lib.display(),
            dest_lib.display()
        )
    });

    // Headers are arch-independent, but use the slice's own to be exact.
    let slice_headers = slice_dir.join("Headers");
    if slice_headers.is_dir() {
        copy_dir_all(&slice_headers, &install_prefix.join("include"));
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
        other => panic!("unsupported Rust target for vendored build: {other}"),
    };
    value.to_owned()
}
