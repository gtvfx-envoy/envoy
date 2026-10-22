"""Time Envoy startup cold and warm, and build a small studio to time it against.

Examples:
    python scripts/bench_cache.py fixture C:/mini --large-mb 64
    python scripts/bench_cache.py run --envoy envoy.exe --stack C:/mini/mini.estack -- hello
    python scripts/bench_cache.py run --envoy envoy -- -s studio python -c "print(1)"

``run`` measures one cold run against an empty cache root, then ``--runs`` warm runs against the
same root, and prints the timings as JSON.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import statistics
import subprocess
import sys
import tempfile
import time
from datetime import UTC, datetime
from pathlib import Path

KILOBYTE = 1024
MEGABYTE = 1024 * KILOBYTE

# The cache root is set under every name Envoy has used, so one harness times old and new builds.
CACHE_ROOT_VARIABLES = ("ENVOY_BUNDLE_CACHE", "ENVOY_CACHE_ROOT")

# Envoy's log output carries colour codes even when piped, so they are stripped before matching.
ANSI_ESCAPE = re.compile(r"\x1b\[[0-9;]*m")
WARN_LEVEL = re.compile(r"\bWARN\b")


def countWarnings(stderr: str) -> int:
    """Count log lines emitted at WARN level.

    Args:
        stderr: Captured standard error of one run.

    Returns:
        Number of lines whose log level is WARN.

    """
    return sum(bool(WARN_LEVEL.search(ANSI_ESCAPE.sub("", line))) for line in stderr.splitlines())


def parseArguments(argv: list[str]) -> argparse.Namespace:
    """Parse harness arguments.

    Args:
        argv: Arguments after the program name.

    Returns:
        Parsed command-line options.

    """
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawTextHelpFormatter
    )
    subcommands = parser.add_subparsers(dest="action", required=True)

    fixture = subcommands.add_parser("fixture", help="Write a mini studio of published bundles.")
    fixture.add_argument("destination", type=Path, help="Directory to create the studio in.")
    fixture.add_argument("--large-mb", type=int, default=64, help="Size of the large-file bundle.")
    fixture.add_argument(
        "--small-files", type=int, default=3000, help="Files in the many-files bundle."
    )
    fixture.add_argument(
        "--deep-levels", type=int, default=12, help="Depth of the deep-path bundle."
    )

    run = subcommands.add_parser("run", help="Time one cold run and several warm runs.")
    run.add_argument("--envoy", required=True, help="Envoy executable to time.")
    run.add_argument("--stack", help="Stack name or .estack path; passed as -s=<stack>.")
    run.add_argument("--runs", type=int, default=5, help="Warm runs after the cold one.")
    run.add_argument(
        "--cache-root", type=Path, help="Cache root to use (default: a fresh temp dir)."
    )
    run.add_argument("--json", type=Path, dest="json_path", help="Also write the result here.")
    run.add_argument(
        "command",
        nargs=argparse.REMAINDER,
        help="Arguments for envoy, after --. Default: python -c print(1)",
    )
    return parser.parse_args(argv)


def writeBundle(root: Path, bndlid: str, version: str) -> Path:
    """Create a published bundle directory: ``.envoy/`` plus a ``.bundle`` marker.

    Args:
        root: Directory holding bundles; the bundle lands in ``<root>/<name>/<version>``.
        bndlid: Bundle ID, ``<namespace>:<name>``.
        version: Version string, also the bundle's directory name.

    Returns:
        The bundle's root directory.

    """
    namespace, name = bndlid.split(":", 1)
    bundle = root / namespace / name / version
    (bundle / ".envoy").mkdir(parents=True)
    marker = {
        "bndlid": bndlid,
        "name": name,
        "version": version,
        "published": datetime.now(UTC).isoformat(),
    }
    (bundle / ".bundle").write_text(json.dumps(marker, indent=2) + "\n", encoding="utf-8")
    return bundle


def writeLargeFile(bundle: Path, megabytes: int) -> None:
    """Add one incompressible file of ``megabytes`` MiB, so a copy cannot be short-circuited.

    Args:
        bundle: Bundle root to add the file to.
        megabytes: Size of the file.

    """
    with (bundle / "payload.bin").open("wb") as stream:
        for _ in range(megabytes):
            stream.write(os.urandom(MEGABYTE))


def writeManySmallFiles(bundle: Path, count: int) -> None:
    """Add ``count`` one-kilobyte files spread over 50 directories, like a Python install.

    Args:
        bundle: Bundle root to add the files to.
        count: Number of files.

    """
    body = ("x" * (KILOBYTE - 1)) + "\n"
    for index in range(count):
        directory = bundle / "lib" / f"package_{index % 50:02d}"
        directory.mkdir(parents=True, exist_ok=True)
        (directory / f"module_{index:05d}.py").write_text(body, encoding="utf-8")


def writeDeepPaths(bundle: Path, levels: int) -> None:
    """Add a file under ``levels`` nested directories of 12 characters each.

    Args:
        bundle: Bundle root to add the tree to.
        levels: Directory depth; 12 levels reach a relative path near 170 characters.

    """
    deep = bundle.joinpath(*(f"nested_{level:05d}" for level in range(levels)))
    deep.mkdir(parents=True)
    (deep / "leaf.txt").write_text("deep\n", encoding="utf-8")


def buildFixture(destination: Path, large_mb: int, small_files: int, deep_levels: int) -> Path:
    """Write the mini studio and a stack that loads all of it.

    Args:
        destination: Directory to create; it must not exist yet.
        large_mb: Size of the large-file bundle in MiB.
        small_files: File count of the many-files bundle.
        deep_levels: Directory depth of the deep-path bundle.

    Returns:
        Path to the ``.estack`` file.

    """
    destination.mkdir(parents=True)
    bundles = destination / "bndl"

    tools = writeBundle(bundles, "gt:tools", "v1.0.0")
    commands = {
        "hello": {
            "environment": [],
            "alias": [sys.executable, "-c", "print('hello from the mini studio')"],
        }
    }
    (tools / ".envoy" / "commands.json").write_text(
        json.dumps(commands, indent=2), encoding="utf-8"
    )

    writeLargeFile(writeBundle(bundles, "ext:bulk", "1.0.0-envoy.1"), large_mb)
    writeManySmallFiles(writeBundle(bundles, "ext:swarm", "3.11.9"), small_files)
    writeDeepPaths(writeBundle(bundles, "ext:deep", "2.0.0"), deep_levels)

    entries = "".join(
        f"  - path: '{path}'\n" for path in sorted(bundles.glob("*/*/*")) if path.is_dir()
    )
    stack_path = destination / "mini.estack"
    stack_path.write_text(f"namespace: gt\nbundles:\n{entries}", encoding="utf-8")
    return stack_path


def directorySize(path: Path) -> int:
    """Return the total size in bytes of the files under ``path``.

    Args:
        path: Directory to measure.

    Returns:
        Total bytes, or 0 when the directory does not exist.

    """
    return sum(entry.stat().st_size for entry in path.rglob("*") if entry.is_file())


def timeRun(command: list[str], environment: dict[str, str]) -> tuple[float, int, str]:
    """Run a command once.

    Args:
        command: Executable and arguments.
        environment: Environment for the child process.

    Returns:
        Elapsed milliseconds, the exit code, and the captured stderr.

    """
    started = time.perf_counter()
    result = subprocess.run(command, env=environment, capture_output=True, text=True, check=False)
    elapsed = (time.perf_counter() - started) * 1000
    return elapsed, result.returncode, result.stderr


def runBenchmark(options: argparse.Namespace) -> dict:
    """Time one cold run and ``options.runs`` warm runs.

    Args:
        options: Parsed ``run`` options.

    Returns:
        A JSON-serializable summary of the timings.

    """
    command = list(options.command)
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        command = ["python", "-c", "print(1)"]
    base = [options.envoy]
    if options.stack:
        base.append(f"-s={options.stack}")

    with tempfile.TemporaryDirectory(prefix="envoy-bench-") as scratch:
        cache_root = options.cache_root or Path(scratch) / "cache"
        cache_root.mkdir(parents=True, exist_ok=True)
        environment = os.environ.copy()
        for variable in CACHE_ROOT_VARIABLES:
            environment[variable] = str(cache_root)

        version = subprocess.run(
            [options.envoy, "--version"], capture_output=True, text=True, check=False
        ).stdout.strip()

        timings: list[float] = []
        exit_codes: list[int] = []
        warnings = 0
        for _ in range(options.runs + 1):
            elapsed, code, stderr = timeRun(base + command, environment)
            timings.append(round(elapsed))
            exit_codes.append(code)
            warnings = max(warnings, countWarnings(stderr))

        return {
            "envoy": version,
            "command": base[1:] + command,
            "cold_ms": timings[0],
            "warm_ms": timings[1:],
            "warm_median_ms": round(statistics.median(timings[1:])) if timings[1:] else None,
            "warnings_per_run": warnings,
            "exit_codes": exit_codes,
            "cache_bytes": directorySize(cache_root),
        }


def main(argv: list[str] | None = None) -> int:
    """Run the harness.

    Args:
        argv: Arguments after the program name; defaults to ``sys.argv[1:]``.

    Returns:
        Process exit status.

    """
    options = parseArguments(sys.argv[1:] if argv is None else argv)
    if options.action == "fixture":
        print(
            buildFixture(
                options.destination, options.large_mb, options.small_files, options.deep_levels
            )
        )
        return 0

    summary = runBenchmark(options)
    text = json.dumps(summary, indent=2)
    print(text)
    if options.json_path:
        options.json_path.write_text(text + "\n", encoding="utf-8")
    return 0 if all(code == 0 for code in summary["exit_codes"]) else 1


if __name__ == "__main__":
    raise SystemExit(main())
