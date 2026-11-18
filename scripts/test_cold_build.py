"""Exercise the real build script with an inert compiler (no network/native cache)."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[1]
ARO = "aro-0.0.0-JSD1QtuBNwCASyBtNF3pqTl_W3oAJQGEVyFAtrBSE_Pa"
FAKE_ZIG = """#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
with open(os.environ["FETCH_TEST_LOG"], "a") as log:
    log.write(json.dumps(args) + "\\n")
if "--fetch=needed" in args:
    assert Path(os.environ["ZIG_GLOBAL_CACHE_DIR"]) == Path(os.environ["OUT_DIR"]) / "zig-mirror-global-cache"
    manifest = Path("build.zig.zon").read_text()
    assert os.environ["FETCH_TEST_ARO"] in manifest
    assert "https://deps.files.ghostty.org/aro-d0c8c4d9c55daa7ef6e40cf0f630a5b5e900989b.tar.gz" in manifest
    if os.environ.get("FETCH_TEST_FAIL"):
        sys.exit(23)
    package = Path(args[args.index("--pkg-dir") + 1]) / os.environ["FETCH_TEST_ARO"]
    package.mkdir(parents=True)
    sys.exit(0)
if "--system" not in args:
    assert (Path("zig-pkg") / os.environ["FETCH_TEST_ARO"]).is_dir()
prefix = Path(args[args.index("--prefix") + 1])
(prefix / "lib").mkdir(parents=True)
(prefix / "lib/libghostty-vt.a").write_bytes(b"inert fixture")
(prefix / "include/ghostty").mkdir(parents=True)
(prefix / "include/ghostty/vt.h").write_text("fixture")
"""


@unittest.skipIf(os.name == "nt", "fixture compiler uses a Unix shebang")
class ColdBuildTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="libghostty-build-test-")
        cls.root = Path(cls.temporary.name)
        cls.script = cls.root / "build-script"
        version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
        subprocess.run(
            ["rustc", "--edition=2024", str(ROOT / "crates/libghostty-vt-sys/build.rs"),
             "-o", str(cls.script)], check=True, env={**os.environ, "CARGO_PKG_VERSION": version},
        )

    @classmethod
    def tearDownClass(cls):
        cls.temporary.cleanup()

    def exercise(self, *, relative_source=False, **overrides):
        directory = self.root / self._testMethodName
        directory.mkdir()
        source = directory / "source"
        source.mkdir()
        (source / "build.zig").write_text("fixture")
        compiler = directory / "custom-zig"
        compiler.write_text(FAKE_ZIG)
        compiler.chmod(0o755)
        env = {k: v for k, v in os.environ.items()
               if not k.startswith(("GHOSTTY_", "LIBGHOSTTY_", "DOCS_RS", "CARGO_CFG_MIRI"))}
        env.update(
            OUT_DIR=str(directory / "out"), HOST="aarch64-apple-darwin",
            TARGET="aarch64-apple-darwin",
            GHOSTTY_SOURCE_DIR="source" if relative_source else str(source),
            ZIG=str(compiler), ZIG_GLOBAL_CACHE_DIR=str(directory / "zig-cache"),
            FETCH_TEST_LOG=str(directory / "calls.jsonl"), FETCH_TEST_ARO=ARO,
        )
        env.update(overrides)
        result = subprocess.run([str(self.script)], cwd=directory, env=env, capture_output=True, text=True)
        log = directory / "calls.jsonl"
        calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        return result, calls

    def test_cold_source_prefetches_exact_mirror_before_build_with_custom_zig(self):
        result, calls = self.exercise()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        self.assertIn("--fetch=needed", calls[0])
        self.assertIn("-Demit-lib-vt=true", calls[1])

    def test_explicit_system_packages_remain_offline(self):
        result, calls = self.exercise(GHOSTTY_ZIG_SYSTEM_DIR=str(self.root))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 1)
        self.assertIn("--system", calls[0])
        self.assertNotIn("--fetch=needed", calls[0])

    def test_relative_source_prefetches_into_the_real_project_before_building(self):
        result, calls = self.exercise(relative_source=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 2)
        package_dir = Path(calls[0][calls[0].index("--pkg-dir") + 1])
        self.assertEqual(package_dir, self.root / self._testMethodName / "source/zig-pkg")
        self.assertTrue((package_dir / ARO).is_dir())
        self.assertIn("-Demit-lib-vt=true", calls[1])

    def test_failed_mirror_fetch_never_runs_the_build(self):
        result, calls = self.exercise(FETCH_TEST_FAIL="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fetch pinned aro", result.stderr)
        self.assertEqual(len(calls), 1)
        self.assertIn("--fetch=needed", calls[0])


if __name__ == "__main__":
    unittest.main()
