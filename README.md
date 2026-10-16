# libghostty-rs

Rust bindings and safe API for [libghostty-vt](https://ghostty.org), the virtual terminal emulator library extracted from [Ghostty](https://ghostty.org).

## Workspace Layout

- `crates/libghostty-vt-sys` — raw FFI bindings generated from `ghostty/vt.h`
- `crates/libghostty-vt` — safe Rust wrappers (Terminal, RenderState, KeyEncoder, MouseEncoder, etc.)
- `example/grid_ref_tracked_rs` — focused example of tracked grid references following cells through scrollback and reset
- `example/ghostling_rs` — Rust port of [ghostling](https://github.com/ghostty-org/ghostling), a minimal terminal emulator using [macroquad](https://macroquad.rs)

## Quick Start

```rust
use libghostty_vt::{Terminal, RenderState};
use libghostty_vt::render::{RowIterator, CellIterator};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create a terminal with 80 columns and 24 rows.
    let mut terminal = Terminal::new(80, 24)?;

    // Register an effect handler for PTY write-back (e.g. query responses).
    terminal.on_pty_write(|_term, data| {
        println!("PTY response: {} bytes", data.len());
    })?;

    // Feed VT-encoded data into the terminal.
    terminal.vt_write(b"Hello, \x1b[1;32mworld\x1b[0m!\r\n");
    terminal.vt_write(b"\x1b[38;2;255;128;0morange text\x1b[0m\r\n");

    // Capture a render snapshot and iterate rows/cells.
    let mut render_state = RenderState::new()?;
    let mut rows = RowIterator::new()?;
    let mut cells = CellIterator::new()?;

    let snapshot = render_state.update(&terminal)?;
    let mut row_iter = rows.update(&snapshot)?;

    while let Some(row) = row_iter.next() {
        let mut cell_iter = cells.update(row)?;
        while let Some(cell) = cell_iter.next() {
            let graphemes: Vec<char> = cell.graphemes()?;
            print!("{graphemes:?}");
        }
        println!();
    }

    Ok(())
}
```

## Building

### Prebuilt archives

A build with none of the variables below set first tries to download a prebuilt
`libghostty-vt.a` of the pinned Ghostty commit from this fork's GitHub
Releases, so it needs neither Zig nor a Ghostty checkout. The build script uses
it only when one is published for the requested flavor:

- the Cargo target is `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
  `aarch64-apple-darwin`, or `x86_64-apple-darwin`;
- the link is static (no `link-dynamic`);
- the Zig optimize mode resolves to `ReleaseFast` and the CPU to `baseline`
  (see below). Release builds get this by default; dev builds use `Debug`, so a
  project that wants the download in dev builds sets
  `LIBGHOSTTY_VT_SYS_OPTIMIZE = "ReleaseFast"` in its `.cargo/config.toml`
  `[env]`.

The download shells out to `curl` and `tar`, and the archive is checked against
the SHA-256 recorded in `crates/libghostty-vt-sys/download.rs` before it is
unpacked into `OUT_DIR`. A checksum mismatch fails the build. Any other
failure (no network, no `curl`) prints a warning and builds from source, as
does `CARGO_NET_OFFLINE=true`. Cargo does not tell build scripts about
`--offline`, so set the variable for an offline build. Set
`LIBGHOSTTY_VT_SYS_FROM_SOURCE=1` to skip the download, or
`LIBGHOSTTY_VT_SYS_PREBUILT_URL` to fetch from a mirror of the release
downloads (`<url>/<tag>/libghostty-vt-<target>.tar.gz`). The archives are
built by `.github/workflows/prebuilt.yml`.

### Building from source

Building from source requires [Zig](https://ziglang.org/) 0.16.x on PATH. The
ghostty source is fetched automatically at build time from the pinned commit in
`build.rs`. Set `GHOSTTY_SOURCE_DIR` to make the build use a local Ghostty
checkout instead. Package managers that need network-free builds can also set
`GHOSTTY_ZIG_SYSTEM_DIR` to a pre-fetched Zig package directory; this is passed
to `zig build --system` so Zig does not download package dependencies during
the Cargo build script.

Set `LIBGHOSTTY_VT_SYS_LIB_DIR` to a directory holding an already built
`libghostty-vt.a` (or the shared library with `link-dynamic`) to skip Zig and
the Ghostty fetch entirely; Zig is then not needed on PATH. This is meant for
package managers and CI that build the C library once, for example as a cached
Nix derivation, and reuse it across Cargo builds. The build script links the
directory as given, so it must hold the Ghostty commit pinned in `build.rs`
built for the Cargo target, and the optimize and CPU settings below do not
apply to it. Headers are optional because the bindings are checked in; when
`<dir>/../include/ghostty/vt.h` exists it is exported as the crate's include
metadata. `GHOSTTY_SOURCE_DIR` takes precedence over it, and it takes
precedence over the `pkg-config` path.

Vendored builds derive Zig's optimize mode from Cargo's profile: dev builds use
`Debug`, size-optimized builds use `ReleaseSmall`, and other release builds use
`ReleaseFast`. Set `LIBGHOSTTY_VT_SYS_OPTIMIZE` to `Debug`, `ReleaseSafe`,
`ReleaseFast`, or `ReleaseSmall` to override that choice explicitly.

Vendored builds target Zig's portable `baseline` CPU so published binaries can
run on older processors. For a binary that will run on the build machine, set
`LIBGHOSTTY_VT_SYS_CPU=native`; a named Zig CPU model such as `x86_64_v3` can be
used when all deployment machines support that target.

The `pkg-config` path is opt-in. If you enable `libghostty-vt-sys/pkg-config`,
the build will prefer an installed `libghostty-vt` discovered through
`pkg-config` when `GHOSTTY_SOURCE_DIR` is unset. libghostty-vt is pre-1.0, so
the checked-in bindings are expected to move with the pinned Ghostty source and
do not guarantee compatibility with arbitrary installed C API revisions. An
explicit `GHOSTTY_SOURCE_DIR` always wins.

Nix builds in this repository prefetch the pinned Ghostty source and Ghostty's
Zig package dependencies up front, then set `GHOSTTY_SOURCE_DIR` and
`GHOSTTY_ZIG_SYSTEM_DIR` for the Cargo build. Downstream Nix packaging should
use the same contract rather than adding `git` or allowing network access in
the sandbox.

By default, `libghostty-vt` and `libghostty-vt-sys` link `libghostty-vt.a`.
This statically links the Ghostty VT archive, but the final binary may still
depend on platform runtime libraries. To link the shared library instead,
enable `libghostty-vt/link-dynamic`.

```sh
nix develop
cargo check
cargo test -p libghostty-vt-sys
cargo build --manifest-path example/Cargo.toml --workspace
```

### Regenerating bindings

`crates/libghostty-vt-sys/src/bindings.rs` is checked in and must match the
Ghostty commit pinned in both `crates/libghostty-vt-sys/build.rs` and
`flake.nix` (run `nix flake update ghostty` after changing the latter); the
`bindings-fresh` Nix check fails otherwise. After bumping the pin, regenerate
the bindings from the dev shell:

```sh
cargo run -p libghostty-vt-sys --features bindgen-tool --bin gen-bindings && cargo fmt -p libghostty-vt-sys
```

The generator reads the headers from `GHOSTTY_INCLUDE_DIR`, or from
`GHOSTTY_SOURCE_DIR/include`, and otherwise from the headers the build script
installed for that same build.

### Miri Verification

Run the Rust-owned soundness checks with:

```sh
nix run .#miri
```

The command supplies its own nightly Miri toolchain and checks both binding and
wrapper crates. These checks intentionally stay on Rust-owned unsafe seams such
as string, slice, and allocator plumbing. They do not execute the native
Ghostty FFI backend.

Use `nix develop .#miri` when working interactively with the same toolchain.

### Running the example

The examples live in their own workspace under `example/`. To run one, enter
its folder and run:

```sh
cargo run
```

With `link-dynamic`, `cargo run` finds the shared library by itself. To run a
built example directly instead, set `LD_LIBRARY_PATH` on Linux or
`DYLD_LIBRARY_PATH` on macOS to the directory containing the generated
`libghostty-vt` shared library. The examples have their own target directory,
so with the default vendored build it is under `example/target/`, at
`<profile>/build/libghostty-vt-sys-*/out/ghostty-install/lib`.
