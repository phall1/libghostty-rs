"""Test prebuilt policy and the real build script without native builds/network."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[1]
COMMIT = "ce42b4c3df8d206092d55d7add0a4f15ac29a0a2"
REPO = "https://github.com/phall1/ghostty.git"
TARGET = "aarch64-apple-darwin"
TAG = "ghostty-vt-ce42b4c3df8d-releasefast-baseline"
ASSET = f"libghostty-vt-{TARGET}.tar.gz"
EXPECTED_SHA256 = "0e67e9e051643de76ee7b6a9e164aec4156f54433f67836b8b1f8a502d2c23fb"
TOOLS = r"""
import json, os, sys
from pathlib import Path
tool = Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ["POLICY_TEST_LOG"], "a") as log:
    log.write(json.dumps({"tool": tool, "args": args, "cwd": os.getcwd()}) + "\n")
if tool == "curl":
    os.execv(os.environ["POLICY_TEST_CURL"], ["curl", *args])
if tool == "tar":
    sys.exit("extraction must never run for these fixtures")
if tool == "git":
    if args[0] == "clone":
        source = Path(args[-1])
        source.mkdir()
        (source / "build.zig").write_text("inert fixture")
    sys.exit(0)
if tool == "zig":
    prefix = Path(args[args.index("--prefix") + 1])
    (prefix / "lib").mkdir(parents=True)
    (prefix / "lib/libghostty-vt.a").write_bytes(b"inert fixture, not an ABI test")
    (prefix / "lib/libghostty-vt.dylib").write_bytes(b"inert fixture, not an ABI test")
    (prefix / "include/ghostty").mkdir(parents=True)
    (prefix / "include/ghostty/vt.h").write_text("inert fixture")
"""


class PrebuiltTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="libghostty-prebuilt-test-")
        cls.root = Path(cls.temporary.name).resolve()
        cls.bin = cls.root / "bin"
        cls.bin.mkdir()
        for tool in ["curl", "tar", "git", "zig"]:
            executable = cls.bin / tool
            executable.write_text(f"#!{sys.executable}\n" + TOOLS)
            executable.chmod(0o755)
        cls.curl = shutil.which("curl")
        if cls.curl is None:
            raise RuntimeError("curl is required to exercise the local-file mirror")
        version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
        # Compilation needs the user's Rust toolchain, but never production phux
        # credentials or flavor overrides. Build-script executions below use a
        # wholly private environment and cannot reach shared native caches.
        compile_env = {k: v for k, v in os.environ.items() if "PHUX" not in k}
        compile_env["CARGO_PKG_VERSION"] = version
        for name, flags in [
            ("static", []),
            ("dynamic", ["--cfg", 'feature="link-dynamic"']),
            ("unit-tests", ["--test"]),
        ]:
            subprocess.run(
                ["rustc", "--edition=2024", str(ROOT / "crates/libghostty-vt-sys/build.rs"),
                 *flags, "-o", str(cls.root / name)],
                env=compile_env, check=True, timeout=120,
            )

    @classmethod
    def tearDownClass(cls):
        cls.temporary.cleanup()

    def exercise(self, *, archive=None, dynamic=False, case="", **overrides):
        directory = self.root / f"{self._testMethodName}{case}"
        directory.mkdir()
        out = directory / "out"
        out.mkdir()
        mirror = directory / "mirror"
        mirror.mkdir()
        if archive is not None:
            asset = mirror / TAG / ASSET
            asset.parent.mkdir()
            asset.write_bytes(archive)
        # A checksum error must not even delete a previous extraction.
        prefix = out / "ghostty-prebuilt"
        prefix.mkdir()
        (prefix / "untouched").write_text("previous extraction")
        env = dict(
            PATH=str(self.bin) + os.pathsep + os.defpath,
            HOME=str(directory), OUT_DIR=str(out), HOST=TARGET, TARGET=TARGET,
            DEBUG="false", OPT_LEVEL="3",
            LIBGHOSTTY_VT_SYS_CACHE_DIR="",
            LIBGHOSTTY_VT_SYS_PREBUILT_URL=mirror.as_uri(),
            GHOSTTY_ZIG_SYSTEM_DIR=str(directory),
            POLICY_TEST_LOG=str(directory / "calls.jsonl"),
            POLICY_TEST_CURL=self.curl,
        )
        env.update(overrides)
        script = self.root / ("dynamic" if dynamic else "static")
        result = subprocess.run(
            [str(script)], cwd=directory, env=env, capture_output=True, text=True, timeout=30,
        )
        log = directory / "calls.jsonl"
        calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        return result, calls, out

    def assert_source_intent(self, calls, out):
        source = out / "ghostty-src"
        git = [call for call in calls if call["tool"] == "git"]
        self.assertEqual(len(git), 2)
        self.assertEqual(git[0]["args"], [
            "clone", "--config", "core.longpaths=true", "--filter=blob:none",
            "--no-checkout", REPO, str(source),
        ])
        self.assertEqual(git[1]["args"], ["checkout", COMMIT])
        self.assertEqual(git[1]["cwd"], str(source))
        self.assertEqual((source / ".ghostty-commit").read_text(), COMMIT)
        zig = [call for call in calls if call["tool"] == "zig"]
        self.assertEqual(len(zig), 1)
        self.assertEqual(zig[0]["cwd"], str(source))
        args = zig[0]["args"]
        self.assertEqual(args[0], "build")
        self.assertIn("-Demit-lib-vt=true", args)
        self.assertIn("--system", args)
        self.assertEqual(args[args.index("--prefix") + 1], str(out / "ghostty-install"))
        self.assertNotIn("tar", [call["tool"] for call in calls])

    def test_build_script_unit_tests(self):
        env = {"PATH": os.environ["PATH"], "TMPDIR": str(self.root)}
        result = subprocess.run(
            [str(self.root / "unit-tests")], env=env, capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("0 failed", result.stdout)

    def test_wrong_digest_is_fatal_before_extraction_or_source_fallback(self):
        archive = b"deliberately not the published archive"
        result, calls, out = self.exercise(archive=archive)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"has SHA-256 {hashlib.sha256(archive).hexdigest()}, expected {EXPECTED_SHA256}", result.stderr)
        self.assertEqual([call["tool"] for call in calls], ["curl"])
        self.assertEqual(calls[0]["args"][-1], (out.parent / "mirror" / TAG / ASSET).as_uri())
        self.assertEqual((out / ASSET).read_bytes(), archive)
        self.assertEqual((out / "ghostty-prebuilt/untouched").read_text(), "previous extraction")
        self.assertFalse((out / "ghostty-src").exists())
        self.assertFalse((out / "ghostty-install").exists())
        self.assertNotIn("cargo:rustc-link-lib", result.stdout)
        self.assertNotIn("building from source", result.stdout)

    def test_transport_failure_falls_back_to_exact_pinned_source(self):
        result, calls, out = self.exercise()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call["tool"] for call in calls], ["curl", "git", "git", "zig"])
        self.assertIn("could not download", result.stdout)
        self.assertIn("building from source", result.stdout)
        self.assert_source_intent(calls, out)
        self.assertIn("-Doptimize=ReleaseFast", calls[-1]["args"])
        self.assertIn("-Dcpu=baseline", calls[-1]["args"])

    def test_source_only_flavors_never_attempt_a_download(self):
        cases = [
            ({"dynamic": True}, "ReleaseFast", "baseline"),
            ({"LIBGHOSTTY_VT_SYS_CPU": "native"}, "ReleaseFast", "native"),
            ({"TARGET": "x86_64-unknown-linux-musl"}, "ReleaseFast", "baseline"),
            ({"LIBGHOSTTY_VT_SYS_OPTIMIZE": "Debug"}, "Debug", "baseline"),
            ({"LIBGHOSTTY_VT_SYS_OPTIMIZE": "ReleaseSafe"}, "ReleaseSafe", "baseline"),
            ({"LIBGHOSTTY_VT_SYS_OPTIMIZE": "ReleaseSmall"}, "ReleaseSmall", "baseline"),
            ({"DEBUG": "true"}, "Debug", "baseline"),
            ({"OPT_LEVEL": "s"}, "ReleaseSmall", "baseline"),
            ({"OPT_LEVEL": "z"}, "ReleaseSmall", "baseline"),
            ({"LIBGHOSTTY_VT_SYS_FROM_SOURCE": "1"}, "ReleaseFast", "baseline"),
            ({"CARGO_NET_OFFLINE": "true"}, "ReleaseFast", "baseline"),
            ({"CARGO_NET_OFFLINE": "1"}, "ReleaseFast", "baseline"),
        ]
        for index, (overrides, optimize, cpu) in enumerate(cases):
            with self.subTest(overrides=overrides):
                # Separate outputs are essential: a previous source stamp must
                # not hide an unexpected fetch or make the next case pass.
                result, calls, out = self.exercise(case=f"-{index}", **overrides)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual([call["tool"] for call in calls], ["git", "git", "zig"])
                self.assert_source_intent(calls, out)
                self.assertIn(f"-Doptimize={optimize}", calls[-1]["args"])
                self.assertIn(f"-Dcpu={cpu}", calls[-1]["args"])


if __name__ == "__main__":
    unittest.main()
