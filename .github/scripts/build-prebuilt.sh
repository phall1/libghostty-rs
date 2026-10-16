#!/usr/bin/env bash
# Build the prebuilt libghostty-vt archive for one Rust target.
#
#   .github/scripts/build-prebuilt.sh <rust-target> <out-dir>
#
# Runs the `zig build` libghostty-vt-sys's build script runs for a build that
# downloads the archive: the pinned Ghostty commit, its optimize mode and CPU
# from download.rs, and the same flags. macOS builds natively (the host must
# match the target), so Ghostty applies its own macOS 13 deployment floor, as
# in a Cargo build. Linux names the glibc target explicitly, so Zig uses its
# own glibc floor rather than the runner's headers and the archive links on
# older distributions.
#
# Writes <out-dir>/lib/libghostty-vt.a, <out-dir>/include/ghostty/..., and
# <out-dir>/BUILDINFO.
set -euo pipefail

target="${1:?usage: build-prebuilt.sh <rust-target> <out-dir>}"
out="${2:?usage: build-prebuilt.sh <rust-target> <out-dir>}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
sys="$root/crates/libghostty-vt-sys"

const() {
  sed -n "s/^const $1: &str = \"\\(.*\\)\";\$/\\1/p" "$sys/$2"
}
repo="$(const GHOSTTY_REPO build.rs)"
commit="$(const GHOSTTY_COMMIT build.rs)"
optimize="$(const PREBUILT_OPTIMIZE download.rs)"
cpu="$(const PREBUILT_CPU download.rs)"
for value in repo commit optimize cpu; do
  [ -n "${!value}" ] || { echo "build-prebuilt: no $value in $sys" >&2; exit 1; }
done

case "$target" in
  x86_64-unknown-linux-gnu) zig_target=x86_64-linux-gnu host_os=Linux host_arch=x86_64 ;;
  aarch64-unknown-linux-gnu) zig_target=aarch64-linux-gnu host_os=Linux host_arch=aarch64 ;;
  aarch64-apple-darwin) zig_target= host_os=Darwin host_arch=arm64 ;;
  x86_64-apple-darwin) zig_target= host_os=Darwin host_arch=x86_64 ;;
  *) echo "build-prebuilt: no prebuilt archive for $target" >&2; exit 1 ;;
esac
if [ "$(uname -s)" != "$host_os" ] || [ "$(uname -m)" != "$host_arch" ]; then
  echo "build-prebuilt: $target builds on a $host_os $host_arch host, not $(uname -s) $(uname -m)" >&2
  exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
git init -q "$work/ghostty"
git -C "$work/ghostty" fetch -q --depth 1 "$repo" "$commit"
git -C "$work/ghostty" checkout -q FETCH_HEAD

args=(
  build
  -Demit-lib-vt=true
  "-Doptimize=$optimize"
  "-Dcpu=$cpu"
  -Demit-xcframework=false
  -Dapp-runtime=none
  --prefix "$work/install"
  --cache-dir "$work/zig-cache"
)
[ -z "$zig_target" ] || args+=("-Dtarget=$zig_target")
(cd "$work/ghostty" && zig "${args[@]}")

test -f "$work/install/lib/libghostty-vt.a"
test -f "$work/install/include/ghostty/vt.h"
rm -rf "$out"
mkdir -p "$out/lib"
cp "$work/install/lib/libghostty-vt.a" "$out/lib/"
cp -R "$work/install/include" "$out/include"
cat >"$out/BUILDINFO" <<EOF
target $target
ghostty $repo $commit
zig $(zig version)
optimize $optimize
cpu $cpu
zig-target ${zig_target:-native}
EOF
cat "$out/BUILDINFO"
ls -l "$out/lib"
