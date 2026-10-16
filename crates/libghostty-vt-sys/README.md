# libghostty-vt-sys

Raw FFI bindings for libghostty-vt.

- Downloads a prebuilt, SHA-256-verified `libghostty-vt.a` from this fork's
  GitHub Releases when one is published for the target and flavor (static,
  `ReleaseFast`, `baseline` CPU), and otherwise fetches and builds it from
  ghostty sources via Zig. `LIBGHOSTTY_VT_SYS_FROM_SOURCE=1` or
  `CARGO_NET_OFFLINE=true` skips the download, and
  `LIBGHOSTTY_VT_SYS_PREBUILT_URL` names a mirror of the release downloads.
  See the repository README.
- Exposes checked-in generated bindings in `src/bindings.rs`.
- Static linking is the baseline rather than a Cargo feature. Enable the
  additive `link-dynamic` feature to link the shared library instead.
- Set `GHOSTTY_SOURCE_DIR` to force the build to use a local Ghostty checkout.
- Set `LIBGHOSTTY_VT_SYS_LIB_DIR` to a directory holding a prebuilt
  `libghostty-vt.a` (or the shared library with `link-dynamic`) to link it
  without running Zig. It must be built from the pinned Ghostty commit for the
  Cargo target; the optimize and CPU settings do not apply to it.
  `GHOSTTY_SOURCE_DIR` takes precedence over it.
- Set `GHOSTTY_ZIG_SYSTEM_DIR` to force Zig package resolution through a
  pre-fetched `zig build --system` directory. This is intended for Nix and other
  sandboxed package managers that cannot fetch during build scripts.
- Vendored builds target Zig's portable `baseline` CPU by default. Set
  `LIBGHOSTTY_VT_SYS_CPU` to `native`, a named CPU model such as `x86_64_v3`, or
  another Zig CPU expression to optimize for known deployment hardware.
- Set `LIBGHOSTTY_VT_SYS_OPTIMIZE` to `Debug`, `ReleaseSafe`, `ReleaseFast`, or
  `ReleaseSmall` to override the Zig optimize mode used by vendored builds.
- iOS targets (`aarch64-apple-ios`, `aarch64-apple-ios-sim`) build through
  ghostty's xcframework emit instead of a flat cross build. This requires a
  macOS host with Xcode and the iOS SDK installed, and supports static linking
  only. The simulator library is arm64-only (what simulators run on Apple
  silicon), so `x86_64-apple-ios` is not supported.
- If the `pkg-config` feature is enabled, the build will use an installed
  `libghostty-vt` found through `pkg-config` only when `GHOSTTY_SOURCE_DIR` is
  unset. With the default static link mode, it probes Ghostty's
  `libghostty-vt-static` pkg-config module instead.
- libghostty-vt is pre-1.0, so these bindings do not guarantee compatibility
  with arbitrary installed C API revisions.
