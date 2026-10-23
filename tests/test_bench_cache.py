"""Tests for the cache benchmark harness."""

from __future__ import annotations

import importlib.util
import json
import sys
from pathlib import Path

SCRIPT_PATH = Path(__file__).parents[1] / "scripts" / "bench_cache.py"
SPEC = importlib.util.spec_from_file_location("bench_cache", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
bench_cache = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bench_cache)


def testFixtureWritesPublishedBundlesAndAStackThatNamesThem(tmp_path):
    """The mini studio holds four published bundles, one of them with ~170 character paths."""
    studio = tmp_path / "studio"
    stack = bench_cache.buildFixture(studio, large_mb=1, small_files=60, deep_levels=12)

    markers = sorted(
        json.loads(marker.read_text(encoding="utf-8"))["bndlid"]
        for marker in (studio / "bndl").glob("*/*/*/.bundle")
    )
    assert markers == ["ext:bulk", "ext:deep", "ext:swarm", "gt:tools"]
    assert stack.read_text(encoding="utf-8").count("- path:") == 4

    deep = next((studio / "bndl").glob("ext/deep/*"))
    longest = max(
        len(path.relative_to(deep).as_posix()) for path in deep.rglob("*") if path.is_file()
    )
    assert longest >= 150
    assert len(list((studio / "bndl" / "ext" / "swarm").rglob("module_*.py"))) == 60


def testCountWarningsIgnoresColourCodesAndOtherLevels():
    """Envoy colours its log level even when piped, which must not hide a warning."""
    stderr = (
        "\x1b[2m2026-10-22T21:07:22Z\x1b[0m \x1b[33m WARN\x1b[0m envoy_core::runtime: a\n"
        "2026-10-22T21:07:22Z  WARN envoy_core::runtime: b\n"
        "2026-10-22T21:07:22Z  INFO envoy_core::runtime: c\n"
        "warning: a plain message from the CLI\n"
    )

    assert bench_cache.countWarnings(stderr) == 2


def testRunTimesOneColdRunAndSeveralWarmRuns(tmp_path):
    """A stand-in executable is timed once cold and ``--runs`` times warm."""
    options = bench_cache.parseArguments(
        [
            *["run", "--envoy", sys.executable, "--runs", "2"],
            *["--cache-root", str(tmp_path / "cache"), "--", "-c", "print(1)"],
        ]
    )

    summary = bench_cache.runBenchmark(options)

    assert len(summary["warm_ms"]) == 2
    assert summary["exit_codes"] == [0, 0, 0]
    assert summary["warnings_per_run"] == 0
    assert summary["cold_ms"] >= 0


def testDefaultCacheRootIsShortAndRemovedAfterTheRun(tmp_path, monkeypatch):
    """Without ``--cache-root`` the root sits beside the default one and does not linger."""
    monkeypatch.setenv("USERPROFILE", str(tmp_path))
    monkeypatch.setenv("HOME", str(tmp_path))
    seen = tmp_path / "seen.txt"
    probe = (
        "import os, pathlib; "
        f"pathlib.Path({str(seen)!r}).write_text(os.environ['ENVOY_CACHE_ROOT'])"
    )
    options = bench_cache.parseArguments(
        ["run", "--envoy", sys.executable, "--runs", "0", "--", "-c", probe]
    )

    bench_cache.runBenchmark(options)

    root = Path(seen.read_text(encoding="utf-8"))
    assert root.parent == tmp_path / ".envoy"
    assert len(root.name) <= 10
    assert not root.exists()
