/// Link a libghostty-vt that was built ahead of time instead of running Zig.
///
/// `LIBGHOSTTY_VT_SYS_LIB_DIR` names the directory that holds the library the
/// current link mode needs: `libghostty-vt.a` for static links (the default),
/// the shared library with `link-dynamic`. Headers are optional, since the
/// bindings are checked in; when `<dir>/../include/ghostty/vt.h` exists it is
/// exported as this crate's include metadata, as a vendored build does.
///
/// The directory is trusted to hold the flavor the caller wants. The build
/// script cannot see how a prebuilt archive was compiled, so
/// `LIBGHOSTTY_VT_SYS_OPTIMIZE`, `LIBGHOSTTY_VT_SYS_CPU` and the Ghostty pin
/// in this file do not apply to it.
fn link_prebuilt(link_mode: LinkMode, target: &str, lib_dir: &Path) {
    assert!(
        lib_dir.is_dir(),
        "LIBGHOSTTY_VT_SYS_LIB_DIR is not a directory: {}",
        lib_dir.display()
    );
    let search_dirs = vec![lib_dir.to_path_buf()];
    assert_has_library(link_mode, target, &search_dirs);

    // The variable only names the directory; rerun when the library inside
    // it is replaced as well.
    for entry in std::fs::read_dir(lib_dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", lib_dir.display()))
    {
        let entry = entry.unwrap_or_else(|error| {
            panic!("failed to read entry from {}: {error}", lib_dir.display())
        });
        let file_name = entry.file_name();
        if file_name
            .to_str()
            .is_some_and(|name| link_mode.matches_library(target, name))
        {
            println!("cargo:rerun-if-changed={}", entry.path().display());
        }
    }

    emit_link_directives(link_mode, target, &search_dirs);

    if let Some(include_dir) = lib_dir.parent().map(|prefix| prefix.join("include"))
        && include_dir.join("ghostty").join("vt.h").exists()
    {
        println!(
            "cargo:rustc-env=LIBGHOSTTY_VT_SYS_INCLUDE_DIR={}",
            include_dir.display()
        );
        emit_include_metadata(&[include_dir]);
    }
}
