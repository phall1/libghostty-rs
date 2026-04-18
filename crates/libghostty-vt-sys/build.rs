use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned ghostty commit. Update this to pull a newer version.
const GHOSTTY_REPO: &str = "https://github.com/ghostty-org/ghostty.git";
const GHOSTTY_COMMIT: &str = "a1e75daef8b64426dbca551c6e41b1fbc2b7ae24";

fn main() {
    // docs.rs has no Zig toolchain. The checked-in bindings in src/bindings.rs
    // are enough for generating documentation, so skip the entire native
    // build when running under docs.rs.
    if env::var("DOCS_RS").is_ok() {
        return;
    }

    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SYS_NO_VENDOR");
    println!("cargo:rerun-if-env-changed=GHOSTTY_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=HOST");
    println!("cargo:rerun-if-changed=crates/libghostty-vt-sys/build.rs");

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

    // The pinned ghostty's build.zig unconditionally attaches an xcframework
    // step to the install target on macOS→macOS builds, which invokes
    // `xcodebuild -create-xcframework` and therefore requires full Xcode.
    // For `-Demit-lib-vt` we don't want or need the xcframework, so neuter
    // that block in the fetched copy. Harmless on non-darwin hosts.
    patch_disable_xcframework(&ghostty_dir);

    // Build libghostty-vt via zig.
    let install_prefix = out_dir.join("ghostty-install");

    let mut build = Command::new("zig");
    build
        .arg("build")
        .arg("-Demit-lib-vt")
        .arg("--prefix")
        .arg(&install_prefix)
        .current_dir(&ghostty_dir);

    // Only pass -Dtarget when cross-compiling. For native builds, let zig
    // auto-detect the host (matches how ghostty's own CMakeLists.txt works).
    if target != host {
        let zig_target = zig_target(&target);
        build.arg(format!("-Dtarget={zig_target}"));
    }

    run(build, "zig build");

    let lib_dir = install_prefix.join("lib");
    let include_dir = install_prefix.join("include");

    let lib_name = if target.contains("darwin") {
        "libghostty-vt.0.1.0.dylib"
    } else {
        "libghostty-vt.so.0.1.0"
    };

    assert!(
        lib_dir.join(lib_name).exists(),
        "expected shared library at {}",
        lib_dir.join(lib_name).display()
    );
    assert!(
        include_dir.join("ghostty").join("vt.h").exists(),
        "expected header at {}",
        include_dir.join("ghostty").join("vt.h").display()
    );

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=ghostty-vt");
    println!("cargo:include={}", include_dir.display());
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

/// Rewrite the fetched ghostty `build.zig` so the lib-vt-only install step
/// doesn't depend on the xcframework step (which requires full Xcode).
/// Idempotent: if the guard is already present, we leave the file alone.
fn patch_disable_xcframework(ghostty_dir: &Path) {
    let build_zig = ghostty_dir.join("build.zig");
    let Ok(contents) = std::fs::read_to_string(&build_zig) else {
        return;
    };

    let needle =
        "if (builtin.os.tag.isDarwin() and config.target.result.os.tag.isDarwin()) {";
    let patched = "if (false and builtin.os.tag.isDarwin() and config.target.result.os.tag.isDarwin()) {";

    if contents.contains(patched) || !contents.contains(needle) {
        return;
    }

    let new_contents = contents.replacen(needle, patched, 1);
    std::fs::write(&build_zig, new_contents)
        .unwrap_or_else(|e| panic!("failed to patch {}: {e}", build_zig.display()));
}

fn run(mut command: Command, context: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to execute {context}: {error}"));
    assert!(status.success(), "{context} failed with status {status}");
}

fn zig_target(target: &str) -> String {
    let value = match target {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        "aarch64-apple-darwin" => "aarch64-macos-none",
        "x86_64-apple-darwin" => "x86_64-macos-none",
        other => panic!("unsupported Rust target for vendored build: {other}"),
    };
    value.to_owned()
}
