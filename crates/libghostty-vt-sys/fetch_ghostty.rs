/// Clone ghostty at the pinned commit into OUT_DIR/ghostty-src.
/// Reuses an existing clone if the commit (and applied patches) match.
fn fetch_ghostty(out_dir: &Path) -> PathBuf {
    let src_dir = out_dir.join("ghostty-src");
    let stamp = src_dir.join(".ghostty-commit");
    // Include the wrap-spacer-head dirty patch so a pin bump is not required
    // to rebuild after the patch file changes (phux-5js7).
    let stamp_value = format!("{GHOSTTY_COMMIT}:phux-5js7");

    // Skip fetch if we already have the right commit and patches.
    if stamp.exists()
        && let Ok(existing) = std::fs::read_to_string(&stamp)
        && existing.trim() == stamp_value
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

    apply_wrap_spacer_head_patch(&src_dir);

    std::fs::write(&stamp, stamp_value).unwrap_or_else(|e| panic!("failed to write stamp: {e}"));

    src_dir
}

/// Mark the previous row dirty when a wrapped wide glyph's spacer_head is
/// rewritten (ECH/print). Drop this once the ghostty pin includes the fix.
fn apply_wrap_spacer_head_patch(src_dir: &Path) {
    let patch = Path::new(env!("CARGO_MANIFEST_DIR")).join("patches/spacer-head-dirty.patch");
    assert!(
        patch.exists(),
        "missing wrap spacer-head dirty patch at {}",
        patch.display()
    );
    let mut apply = Command::new("git");
    apply.arg("apply").arg(&patch).current_dir(src_dir);
    run(apply, "git apply spacer-head-dirty patch");
}
