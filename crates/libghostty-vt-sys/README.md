# libghostty-vt-sys

Raw FFI bindings for libghostty-vt.

- Fetches and builds vendored `libghostty-vt` artifacts from ghostty sources via Zig.
- Links dynamically by default. Enable the `link-static` feature to link the vendored static archive instead.
- SIMD-accelerated code paths follow zig's convention: off in debug builds, on in release builds. The `simd` cargo feature (default-on) lets users force SIMD off entirely. The SIMD implementation uses simdutf (C++), which pulls in a `libc++` runtime dependency. Disabling `simd` removes the libc++ requirement.
- Static consumers with SIMD enabled need a libc++-compatible final linker. On Linux GNU targets, standard Cargo builds will usually retain runtime `libc++.so` and `libc++abi.so` dependencies unless the final link uses `zig cc`.
- Exposes checked-in generated bindings in `src/bindings.rs`.
- Set `GHOSTTY_SOURCE_DIR` to use a local ghostty checkout.
