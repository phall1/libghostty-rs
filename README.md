# libghostty-rs

Rust bindings and safe API for [libghostty-vt](https://ghostty.org), the virtual terminal emulator library extracted from [Ghostty](https://ghostty.org).

## Workspace Layout

- `crates/ghostty-sys` — raw FFI bindings generated from `ghostty/vt.h`
- `crates/ghostty` — safe Rust wrappers (Terminal, RenderState, KeyEncoder, MouseEncoder, etc.)
- `example/ghostling_rs` — Rust port of [ghostling](https://github.com/ghostty-org/ghostling), a minimal terminal emulator using [macroquad](https://macroquad.rs)

## Building

Requires [Zig](https://ziglang.org/) 0.15.x on PATH. The ghostty source is fetched automatically at build time (pinned commit in `build.rs`). Set `GHOSTTY_SOURCE_DIR` to use a local checkout instead.

```sh
nix develop
cargo check
cargo test -p ghostty-sys
cargo build -p ghostling_rs
```

### Running the example

```sh
# Linux
LD_LIBRARY_PATH=$(dirname $(find target/debug/build/ghostty-sys-*/out -name "libghostty-vt*" | head -1)) \
  cargo run -p ghostling_rs

# macOS
DYLD_LIBRARY_PATH=$(dirname $(find target/debug/build/ghostty-sys-*/out -name "libghostty-vt*" | head -1)) \
  cargo run -p ghostling_rs
```
