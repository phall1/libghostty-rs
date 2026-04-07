# libghostty-vt

Safe Rust API over `libghostty-vt-sys`.

Enable the `link-static` feature to forward static-linking through to `libghostty-vt-sys`. The `simd` feature (forwarded to `libghostty-vt-sys/simd`) controls SIMD-accelerated code paths: off in debug builds, on in release builds. Disable the feature to force SIMD off and remove the libc++ dependency.

Handle types (`Terminal`, `RenderState`, `KeyEncoder`, etc.) are `!Send + !Sync` by design. Callers should drive all operations from a single thread.
