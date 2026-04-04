# libghostty-vt-sys

Raw FFI bindings for libghostty-vt.

- Fetches and builds vendored `libghostty-vt` artifacts from ghostty sources via Zig.
- Links dynamically by default. Enable the `link-static` feature to link the vendored static archive instead.
- Static linking follows upstream Ghostty's `pkg-config` metadata and expects `libc++` to be available to the linker.
- Exposes checked-in generated bindings in `src/bindings.rs`.
- Set `GHOSTTY_SOURCE_DIR` to use a local ghostty checkout.
