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

    let mut build = Command::new("zig");
    build
        .arg("build")
        .arg("-Demit-lib-vt=true")
        .arg(format!("-Doptimize={optimize}"))
        // Cargo artifacts may run on older CPUs than the build host. Without
        // an explicit CPU model, Zig may emit host-specific instructions that
        // make distributed binaries fail with an illegal instruction. Users
        // building for a known machine can explicitly request `native` or a
        // named Zig CPU model through LIBGHOSTTY_VT_SYS_CPU.
        //
        // For iOS builds this only affects the host-native flat artifacts:
        // ghostty resolves its own per-platform targets for the xcframework
        // slices, so -Dcpu does not leak into them.
        .arg(format!("-Dcpu={cpu}"))
        .arg(if ios_platform.is_some() {
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

    // Pass -Dtarget only for non-iOS cross targets; native builds let zig
    // auto-detect the host. iOS builds run host-native inside the xcframework
    // emit, so they must not pass -Dtarget or a global --sysroot.
    if target != host && ios_platform.is_none() {
        let zig_target = zig_target(target);
        build.arg(format!("-Dtarget={zig_target}"));
    }

    run(build, "zig build");

    // The emit also installs host-native flat artifacts; replace them with the
    // iOS library so the link emission below picks up the right arch.
    if let Some(platform) = ios_platform {
        extract_xcframework_lib(&install_prefix, platform);
    }

    let lib_dir = install_prefix.join("lib");
    let include_dir = install_prefix.join("include");
    let search_dirs = library_search_dirs(target, &install_prefix);
    if ios_platform.is_none() {
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

                link_mode.matches_library(target, file_name)
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
        LinkMode::Static => emit_static_link_lib(target),
    }
    emit_include_metadata(&[include_dir]);
}
