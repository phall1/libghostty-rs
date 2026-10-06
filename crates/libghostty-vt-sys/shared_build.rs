// Vendored builds shared across Cargo target directories.
//
// Building inside OUT_DIR ties the Zig output to one target directory: the
// archive's debug info records the source and Zig cache paths. Every new
// worktree or target directory therefore re-cloned Ghostty, re-ran the full
// Zig build, and produced a library that differed only by those paths, which
// also made every downstream crate miss in content-keyed compiler caches. A
// per-user directory keyed on every build input does each build once per
// machine and yields byte-identical output wherever it is copied.

/// Bump when the shared layout or what the key covers changes.
const SHARED_BUILD_SCHEMA: &str = "1";

/// Marker written after a complete build. Its mtime records the last use.
const SHARED_BUILD_COMPLETE: &str = "complete";

/// Builds unused for this long are removed by later builds.
const SHARED_BUILD_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// The shared build directory, or `None` to build inside OUT_DIR.
///
/// `LIBGHOSTTY_VT_SYS_CACHE_DIR` chooses the directory; set it empty to opt
/// out. Without it, the platform's per-user cache directory is used. Sandboxed
/// builds (a Nix derivation, a read-only HOME) cannot create it and fall back
/// to OUT_DIR.
fn shared_build_root() -> Option<PathBuf> {
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SYS_CACHE_DIR");
    let root = match env::var_os("LIBGHOSTTY_VT_SYS_CACHE_DIR") {
        Some(dir) if dir.is_empty() => return None,
        Some(dir) => PathBuf::from(dir),
        None => user_cache_dir()?.join("libghostty-vt-sys"),
    };
    std::fs::create_dir_all(&root).ok()?;
    Some(root)
}

fn user_cache_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_CACHE_HOME").filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if cfg!(windows) {
        return env::var_os("LOCALAPPDATA").map(PathBuf::from);
    }
    let home = PathBuf::from(env::var_os("HOME")?);
    if cfg!(target_os = "macos") {
        Some(home.join("Library").join("Caches"))
    } else {
        Some(home.join(".cache"))
    }
}

/// Build (or reuse) the shared build for `zig`, then copy its install tree
/// to `install_prefix`.
fn build_shared(root: &Path, zig: &ZigBuild, install_prefix: &Path) {
    let key = shared_build_key(zig);
    let dir = root.join(&key);

    // Concurrent builds in other target directories wait here rather than
    // racing the same clone and Zig build.
    let lock = std::fs::File::create(root.join(format!("{key}.lock"))).unwrap_or_else(|error| {
        panic!("failed to create build lock in {}: {error}", root.display())
    });
    lock.lock()
        .unwrap_or_else(|error| panic!("failed to lock {}: {error}", root.display()));

    let complete = dir.join(SHARED_BUILD_COMPLETE);
    if complete.exists() {
        touch(&complete);
    } else {
        build_shared_dir(&dir, zig);
        std::fs::write(&complete, &key)
            .unwrap_or_else(|error| panic!("failed to write {}: {error}", complete.display()));
    }

    if install_prefix.exists() {
        std::fs::remove_dir_all(install_prefix).unwrap_or_else(|error| {
            panic!("failed to remove {}: {error}", install_prefix.display())
        });
    }
    copy_dir_all(&dir.join("ghostty-install"), install_prefix);
    drop(lock);

    prune_shared_builds(root, &key);
}

fn build_shared_dir(dir: &Path, zig: &ZigBuild) {
    // A directory without the marker is a build that failed or was killed.
    if dir.exists() {
        std::fs::remove_dir_all(dir)
            .unwrap_or_else(|error| panic!("failed to remove {}: {error}", dir.display()));
    }
    std::fs::create_dir_all(dir)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", dir.display()));
    let ghostty_dir = fetch_ghostty(dir);
    zig.run(&ghostty_dir, &dir.join("ghostty-install"), dir);

    // The installed artifacts are all later builds read, so drop what only
    // the build itself needed: the Zig cache, the packages Zig fetched into
    // the source tree, and the clone's history. Together they are about
    // three quarters of the directory. Ghostty's own source stays so its
    // debug info resolves.
    for scratch in [
        dir.join("zig-cache"),
        ghostty_dir.join("zig-pkg"),
        ghostty_dir.join(".git"),
    ] {
        if scratch.exists() {
            std::fs::remove_dir_all(&scratch)
                .unwrap_or_else(|error| panic!("failed to remove {}: {error}", scratch.display()));
        }
    }
}

/// Everything that decides the build's output, hashed into a directory name.
fn shared_build_key(zig: &ZigBuild) -> String {
    // The Apple SDK and compiler environment reach Ghostty's build through
    // xcrun and its C dependencies; Cargo must rerun when they change.
    let environment = ["SDKROOT", "DEVELOPER_DIR", "GHOSTTY_ZIG_SYSTEM_DIR"];
    for name in environment {
        println!("cargo:rerun-if-env-changed={name}");
    }

    let mut key = Fnv1a::default();
    key.write(SHARED_BUILD_SCHEMA);
    key.write(env!("CARGO_PKG_VERSION"));
    key.write(GHOSTTY_REPO);
    key.write(GHOSTTY_COMMIT);
    key.write(zig.target);
    key.write(zig.host);
    key.write(zig.ios_platform.unwrap_or(""));
    key.write(zig.optimize);
    key.write(&zig.cpu);
    key.write(&zig_version());
    // Two Zig builds can report the same version and still emit different
    // objects: nixpkgs' 0.16.0 rebuilt with GCC 16 wrote random undefined
    // section symbols that the release binary does not. Key on the binary
    // itself, so a different Zig never reuses another's archive.
    key.write(&zig_identity());
    for name in environment {
        key.write(&env::var(name).unwrap_or_default());
    }
    format!("{}-{:016x}", &GHOSTTY_COMMIT[..12], key.finish())
}

/// The `zig` on PATH, as its canonical path plus size and modification time.
fn zig_identity() -> String {
    let Some(zig) = zig_executable() else {
        return String::new();
    };
    let (len, modified) = std::fs::metadata(&zig)
        .map(|meta| (meta.len(), meta.modified().ok()))
        .unwrap_or((0, None));
    let modified = modified
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    format!("{} {len} {modified}", zig.display())
}

/// Resolve `zig` the way `Command::new("zig")` will: the first match on PATH.
fn zig_executable() -> Option<PathBuf> {
    let name = if cfg!(windows) { "zig.exe" } else { "zig" };
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .and_then(|zig| std::fs::canonicalize(zig).ok())
}

fn zig_version() -> String {
    let output = Command::new("zig")
        .arg("version")
        .output()
        .unwrap_or_else(|error| panic!("failed to run zig version: {error}"));
    assert!(
        output.status.success(),
        "zig version failed: {}",
        output.status
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn touch(path: &Path) {
    // Best effort: a stale mtime only makes pruning more eager.
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

/// Remove other builds unused for `SHARED_BUILD_MAX_AGE`. Best effort: a
/// build that cannot take its lock is in use and is left alone.
fn prune_shared_builds(root: &Path, current: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == current || name.ends_with(".lock") || !entry.path().is_dir() {
            continue;
        }
        if !shared_build_is_stale(&entry.path()) {
            continue;
        }
        let Ok(lock) = std::fs::File::create(root.join(format!("{name}.lock"))) else {
            continue;
        };
        if lock.try_lock().is_ok() {
            let _ = std::fs::remove_dir_all(entry.path());
            let _ = std::fs::remove_file(root.join(format!("{name}.lock")));
        }
    }
}

fn shared_build_is_stale(dir: &Path) -> bool {
    let marker = dir.join(SHARED_BUILD_COMPLETE);
    let Ok(modified) = std::fs::metadata(&marker).and_then(|meta| meta.modified()) else {
        // Incomplete: only stale once nothing has touched it for the max age.
        return std::fs::metadata(dir)
            .and_then(|meta| meta.modified())
            .map(|modified| modified.elapsed().unwrap_or_default() > SHARED_BUILD_MAX_AGE)
            .unwrap_or(false);
    };
    modified.elapsed().unwrap_or_default() > SHARED_BUILD_MAX_AGE
}

/// FNV-1a: stable across Rust releases, unlike `DefaultHasher`, so a
/// toolchain update does not orphan every shared build.
struct Fnv1a(u64);

impl Default for Fnv1a {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv1a {
    /// Length-prefixed, so ("ab", "c") and ("a", "bc") hash differently.
    fn write(&mut self, part: &str) {
        for byte in (part.len() as u64)
            .to_le_bytes()
            .iter()
            .chain(part.as_bytes())
        {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}
