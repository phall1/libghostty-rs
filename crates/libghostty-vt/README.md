# libghostty-vt

Safe Rust API over `libghostty-vt-sys`.

Handle types (`Terminal`, `RenderState`, `KeyEncoder`, etc.) are `!Send + !Sync` by design. Callers should drive all operations from a single thread.

## Whole-terminal snapshots

`Terminal::encode_snapshot` captures the complete terminal state and live VT
stream continuation into an allocator-owned `EncodedSnapshot`. The wrapper
borrows the bytes without copying and frees them with libghostty's matching
allocator when dropped. `Terminal::decode_snapshot` restores one normal,
callback-capable `Terminal` and reports how many input bytes it consumed:

```rust
use libghostty_vt::{Terminal, TerminalOptions};

let mut source = Terminal::new(TerminalOptions {
    cols: 80,
    rows: 24,
    max_scrollback: 1000,
})?;
source.vt_write(b"hello");

let encoded = source.encode_snapshot()?;
let mut transport = encoded.to_vec();
transport.extend_from_slice(b"next message");

let restored = Terminal::decode_snapshot(&transport)?;
assert_eq!(&transport[restored.consumed..], b"next message");
# Ok::<(), libghostty_vt::Error>(())
```

The blob format is native to the pinned Ghostty revision and intentionally
opaque. Malformed, truncated, unsupported, or version-incompatible snapshots
return `Error::InvalidValue`; allocation failure returns `Error::OutOfMemory`.
Use the `_with_alloc` variants when the encoded bytes or decoded terminal must
use a custom allocator.
