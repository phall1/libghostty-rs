/// Build libghostty-vt from source via zig. The zig build itself generates
/// shared and static artifacts plus pkg-config files in `share/pkgconfig/`.
fn build_vendored(link_mode: LinkMode, target: &str) {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must be set"));
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
    let cpu = env::var("LIBGHOSTTY_VT_SYS_CPU").unwrap_or_else(|_| "baseline".to_owned());
    assert!(
        !cpu.is_empty(),
        "LIBGHOSTTY_VT_SYS_CPU must not be empty when set"
    );

    // iOS builds go through ghostty's emit-xcframework path instead of a flat
    // `-Dtarget=<ios> --sysroot=<sdk>` invocation. The flat build breaks down
    // for iOS: a generic target baseline can't compile simdutf's always_inline
    // NEON intrinsics, and `--sysroot` applies globally so it leaks into the
    // native codegen tools ghostty runs mid-build. The xcframework path runs
    // host-native and configures each Apple platform itself, so we build that
    // and pull out the library we need afterwards.
    let ios_platform = ios_xcframework_platform(target);
    if ios_platform.is_some() {
        // Ghostty only emits the xcframework when zig itself runs on macOS
        // (it shells out to xcodebuild). Without this check a Linux cross
        // build would silently produce a host-native library and then fail on
        // a confusing missing-xcframework assertion after the full build.
        assert!(
            host.contains("apple-darwin"),
            "building for {target} requires a macOS host with Xcode and the iOS SDK \
             (ghostty's emit-xcframework path runs host-native); host is {host}"
        );
        // The xcframework contains only static archives, and the flat layout
        // below is populated from one of them. Fail up front instead of
        // letting the shared-library search fail with a misleading message.
        assert!(
            matches!(link_mode, LinkMode::Static),
            "building for {target} supports static linking only; \
             disable the link-dynamic feature"
        );
    }
