// Prebuilt archives downloaded from this fork's GitHub Releases.
//
// A build with nothing configured links a published archive of the pinned
// Ghostty commit instead of running Zig, when one exists for the requested
// flavor: the Cargo target, the static link mode, and the optimize mode and
// CPU the archives were built with. Anything else builds from source.
//
// Each archive is verified against the SHA-256 recorded here before it is
// unpacked. A mismatch is a hard error rather than a fallback: it means the
// download is not the archive this crate was released against.

/// Base URL of the release downloads. `LIBGHOSTTY_VT_SYS_PREBUILT_URL`
/// replaces it, for mirrors; the archive is fetched from
/// `<base>/<tag>/<asset>` either way.
const PREBUILT_BASE_URL: &str = "https://github.com/phall1/libghostty-rs/releases/download";

/// The Zig optimize mode and CPU the published archives were built with.
/// `.github/workflows/prebuilt.yml` reads both from this file.
const PREBUILT_OPTIMIZE: &str = "ReleaseFast";
const PREBUILT_CPU: &str = "baseline";

/// `(Rust target, SHA-256 of its archive)` for every archive in the release
/// of `GHOSTTY_COMMIT`. The release workflow prints this table.
const PREBUILT_ARCHIVES: &[(&str, &str)] = &[
    (
        "aarch64-apple-darwin",
        "2914a1693141804cd3cc0bcf2ce19dccc48537fd3d61a45b2f3570043918198a",
    ),
    (
        "aarch64-unknown-linux-gnu",
        "c065e33c71d357666bfea3b6aa867292b6ef325dd4d1e51a72edecec157e4dd3",
    ),
    (
        "x86_64-apple-darwin",
        "774187d851d0023fff52c9dce1ccdceeeb0a628f8bb214bd6c1300265569dbf1",
    ),
    (
        "x86_64-unknown-linux-gnu",
        "2037c32673bcb0c764db7b265763ee2a0f7cc6c554b6df57a996fbefbfd5559b",
    ),
];

/// The release tag: the Ghostty commit and the build flavor.
fn prebuilt_tag() -> String {
    format!(
        "ghostty-vt-{}-{}-{}",
        &GHOSTTY_COMMIT[..12],
        PREBUILT_OPTIMIZE.to_ascii_lowercase(),
        PREBUILT_CPU
    )
}

fn prebuilt_asset(target: &str) -> String {
    format!("libghostty-vt-{target}.tar.gz")
}

/// Link the published archive for this build, if there is one for its
/// flavor. Returns `false` when the build should run Zig instead.
fn try_download_prebuilt(link_mode: LinkMode, target: &str) -> bool {
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SYS_FROM_SOURCE");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SYS_PREBUILT_URL");
    println!("cargo:rerun-if-env-changed=CARGO_NET_OFFLINE");

    if env::var("LIBGHOSTTY_VT_SYS_FROM_SOURCE").is_ok_and(|value| value == "1") {
        return false;
    }
    if !matches!(link_mode, LinkMode::Static) {
        return false;
    }
    let Some(&(_, sha256)) = PREBUILT_ARCHIVES.iter().find(|(name, _)| *name == target) else {
        return false;
    };
    if zig_optimize_mode() != PREBUILT_OPTIMIZE || zig_cpu() != PREBUILT_CPU {
        return false;
    }
    // Cargo does not pass `--offline` to build scripts; CARGO_NET_OFFLINE is
    // the setting they can see.
    if env::var("CARGO_NET_OFFLINE").is_ok_and(|value| value == "true" || value == "1") {
        return false;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must be set"));
    let base = env::var("LIBGHOSTTY_VT_SYS_PREBUILT_URL")
        .ok()
        .filter(|base| !base.is_empty())
        .unwrap_or_else(|| PREBUILT_BASE_URL.to_owned());
    let url = format!(
        "{}/{}/{}",
        base.trim_end_matches('/'),
        prebuilt_tag(),
        prebuilt_asset(target)
    );
    let archive = out_dir.join(prebuilt_asset(target));
    let prefix = out_dir.join("ghostty-prebuilt");

    let mut curl = Command::new("curl");
    curl.args([
        "--fail",
        "--location",
        "--silent",
        "--show-error",
        "--retry",
        "2",
        "--connect-timeout",
        "10",
        "--output",
    ])
    .arg(&archive)
    .arg(&url);
    if let Err(reason) = run_quiet(curl) {
        println!(
            "cargo:warning=libghostty-vt-sys: could not download {url} ({reason}); building from source"
        );
        return false;
    }

    let bytes = std::fs::read(&archive)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", archive.display()));
    let actual = sha256_hex(&bytes);
    assert!(
        actual == sha256,
        "libghostty-vt-sys: {url} has SHA-256 {actual}, expected {sha256}. \
         Set LIBGHOSTTY_VT_SYS_FROM_SOURCE=1 to build from source instead."
    );

    if prefix.exists() {
        std::fs::remove_dir_all(&prefix)
            .unwrap_or_else(|error| panic!("failed to remove {}: {error}", prefix.display()));
    }
    std::fs::create_dir_all(&prefix)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", prefix.display()));
    let mut tar = Command::new("tar");
    tar.arg("-xzf").arg(&archive).arg("-C").arg(&prefix);
    if let Err(reason) = run_quiet(tar) {
        println!(
            "cargo:warning=libghostty-vt-sys: could not unpack {} ({reason}); building from source",
            archive.display()
        );
        return false;
    }
    let _ = std::fs::remove_file(&archive);

    println!("libghostty-vt-sys: linking prebuilt {url} (sha256 {sha256})");
    link_prebuilt(link_mode, target, &prefix.join("lib"));
    true
}

/// Run `command`, returning a one-line reason when it cannot start or fails.
fn run_quiet(mut command: Command) -> Result<(), String> {
    let output = command.output().map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim().lines().last().unwrap_or("").to_owned();
    Err(format!("{}: {detail}", output.status))
}

/// SHA-256 of `data`, as lowercase hex (FIPS 180-4).
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_be_bytes());

    for block in message.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*word);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    state.iter().map(|word| format!("{word:08x}")).collect()
}
