# libghostty-vt-sys

Raw FFI bindings for libghostty-vt.

- Fetches and builds vendored `libghostty-vt` artifacts from ghostty sources via Zig.
- Links dynamically by default. Enable the `link-static` feature to link the vendored static archive instead.
- Static consumers need a libc++-compatible final linker. On Linux GNU targets, standard Cargo builds will usually retain runtime `libc++.so` and `libc++abi.so` dependencies unless the final link uses `zig cc`.
- Exposes checked-in generated bindings in `src/bindings.rs`.
- Set `GHOSTTY_SOURCE_DIR` to use a local ghostty checkout.
