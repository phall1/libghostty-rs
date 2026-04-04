# libghostty-vt

Safe Rust API over `libghostty-vt-sys`.

Enable the `link-static` feature to forward static-linking through to `libghostty-vt-sys`.

Handle types (`Terminal`, `RenderState`, `KeyEncoder`, etc.) are `!Send + !Sync` by design. Callers should drive all operations from a single thread.
