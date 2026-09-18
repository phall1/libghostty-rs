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
