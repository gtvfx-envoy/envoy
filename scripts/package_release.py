"""Create one platform-specific Envoy release archive."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import tarfile
import tempfile
import zipfile
from datetime import UTC, datetime
from pathlib import Path, PurePosixPath

REPO_ROOT = Path(__file__).resolve().parent.parent

# Substrings a wheel's file name must contain to belong to a target, so a wheel built for
# another platform is never packaged by mistake. Unknown targets accept any wheel.
WHEEL_PLATFORM_TAGS = {
    "windows-x86_64": ("win_amd64",),
    "linux-x86_64-musl": ("manylinux", "x86_64"),
    "macos-x86_64": ("macosx", "x86_64"),
    "macos-aarch64": ("macosx", "arm64"),
}

# The wheel is unpacked into ``py/``, so the packaged bundle puts that directory on PYTHONPATH.
PACKAGED_PYTHON_ENV = {"+=PYTHONPATH": "${__BUNDLE__}/py"}


def parseArguments() -> argparse.Namespace:
    """Parse release-packaging arguments.

    Returns:
        Parsed command-line options.

    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, help="Release version, including any v prefix.")
    parser.add_argument("--target-name", required=True, help="Artifact target label.")
    parser.add_argument("--binary-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, default=REPO_ROOT / "dist")
    parser.add_argument("--windows", action="store_true", help="Package .exe files in a ZIP.")
    wheel_group = parser.add_mutually_exclusive_group()
    wheel_group.add_argument(
        "--wheel-dir",
        type=Path,
        help="Directory holding this target's Python wheel (default: the output directory).",
    )
    wheel_group.add_argument(
        "--no-wheel",
        action="store_true",
        help="Package without the Python API: no py/ directory and no python_env.json.",
    )
    return parser.parse_args()


def findWheel(wheel_directory: Path, target_name: str) -> Path:
    """Return the Python wheel built for a release target.

    Args:
        wheel_directory: Directory to search for ``envoy-*.whl`` files.
        target_name: Artifact target label, used to ignore wheels built for other platforms.

    Returns:
        Path to the single matching wheel.

    Raises:
        FileNotFoundError: If no wheel for the target is present.
        ValueError: If more than one wheel for the target is present.

    """
    tags = WHEEL_PLATFORM_TAGS.get(target_name, ())
    wheels = sorted(
        path
        for path in wheel_directory.glob("envoy-*.whl")
        if all(tag in path.name for tag in tags)
    )
    if not wheels:
        raise FileNotFoundError(
            f"No Python wheel for {target_name} found in {wheel_directory}. Build one with "
            "maturin, point --wheel-dir at it, or pass --no-wheel."
        )
    if len(wheels) > 1:
        names = ", ".join(path.name for path in wheels)
        raise ValueError(
            f"Expected one Python wheel for {target_name} in {wheel_directory}, found: {names}"
        )
    return wheels[0]


def writePythonPayload(bundle_root: Path, wheel_path: Path | None) -> None:
    """Give a staged bundle its Python API, or remove the file that would point at it.

    The checkout's ``python_env.json`` points at ``rust/envoy-py/python``, which a packaged
    bundle does not contain. With a wheel, its contents are unpacked into ``py/`` and the
    environment file points there instead; without one, the dangling file is removed.

    Args:
        bundle_root: Staged bundle whose ``.envoy`` directory is already populated.
        wheel_path: Wheel built for the target, or ``None`` to package without the Python API.

    """
    python_env = bundle_root / ".envoy" / "python_env.json"
    if wheel_path is None:
        python_env.unlink(missing_ok=True)
        return

    with zipfile.ZipFile(wheel_path) as wheel:
        wheel.extractall(bundle_root / "py")
    python_env.write_text(json.dumps(PACKAGED_PYTHON_ENV, indent=4) + "\n", encoding="utf-8")


def copyReleaseFiles(
    bundle_root: Path,
    binary_directory: Path,
    version: str,
    windows: bool,
    wheel_path: Path | None,
) -> None:
    """Populate a staged, versioned release bundle.

    Args:
        bundle_root: Destination corresponding to ``gt/envoy/<version>``.
        binary_directory: Directory containing the built native CLIs.
        version: Version written to the production bundle marker.
        windows: Whether to package Windows executables and launchers.
        wheel_path: Python wheel to unpack into ``py/``, or ``None`` to omit the Python API.

    """
    bundle_root.mkdir(parents=True)
    shutil.copytree(REPO_ROOT / ".envoy", bundle_root / ".envoy")
    writePythonPayload(bundle_root, wheel_path)
    shutil.copy2(REPO_ROOT / "LICENSE", bundle_root / "LICENSE")
    shutil.copy2(REPO_ROOT / "README.md", bundle_root / "README.md")
    marker_data = {
        "bndlid": "gt:envoy",
        "name": "envoy",
        "version": version,
        "published": datetime.now(UTC).isoformat(),
    }
    (bundle_root / ".bundle").write_text(
        json.dumps(marker_data, indent=2) + "\n",
        encoding="utf-8",
    )

    bin_directory = bundle_root / "bin"
    bin_directory.mkdir()
    suffix = ".exe" if windows else ""
    for executable_name in ("envoy",):
        source = binary_directory / f"{executable_name}{suffix}"
        if not source.is_file():
            raise FileNotFoundError(f"Native executable not found: {source}")
        destination = bin_directory / f"{executable_name}{suffix}"
        shutil.copy2(source, destination)
        if not windows:
            destination.chmod(destination.stat().st_mode | 0o755)

    alias_name = "en.bat" if windows else "en"
    alias_source = REPO_ROOT / "bin" / alias_name
    alias_destination = bin_directory / alias_name
    shutil.copy2(alias_source, alias_destination)
    if not windows:
        alias_destination.chmod(alias_destination.stat().st_mode | 0o755)


def writeArchive(
    staging_directory: Path,
    output_directory: Path,
    version: str,
    target_name: str,
    windows: bool,
) -> Path:
    """Write the staged tree as ZIP or compressed tar.

    Args:
        staging_directory: Directory containing the ``gt`` archive root.
        output_directory: Destination for the completed archive.
        version: Release version used in the filename.
        target_name: Platform label used in the filename.
        windows: Whether to emit ZIP instead of ``tar.gz``.

    Returns:
        Path to the completed archive.

    """
    output_directory.mkdir(parents=True, exist_ok=True)
    base_name = f"envoy-{version}-{target_name}"
    if windows:
        archive_path = output_directory / f"{base_name}.zip"
        with zipfile.ZipFile(archive_path, "w", zipfile.ZIP_DEFLATED) as archive:
            for source in sorted(staging_directory.rglob("*")):
                if source.is_file():
                    archive.write(source, source.relative_to(staging_directory).as_posix())
    else:
        archive_path = output_directory / f"{base_name}.tar.gz"
        with tarfile.open(archive_path, "w:gz") as archive:
            archive.add(
                staging_directory / "gt",
                arcname="gt",
                filter=normalizeTarMetadata,
            )

    return archive_path


def normalizeTarMetadata(member: tarfile.TarInfo) -> tarfile.TarInfo:
    """Make Unix archive ownership and permissions host-independent.

    Args:
        member: Archive entry generated by :mod:`tarfile`.

    Returns:
        The normalized archive entry.

    """
    archive_path = PurePosixPath(member.name)
    executable_names = {"en", "envoy"}
    is_executable = archive_path.parent.name == "bin" and archive_path.name in executable_names

    member.uid = 0
    member.gid = 0
    member.uname = "root"
    member.gname = "root"
    if member.isdir() or is_executable:
        member.mode = 0o755
    elif member.isfile():
        member.mode = 0o644
    return member


def main() -> int:
    """Create one release archive.

    Returns:
        Process exit status.

    """
    options = parseArguments()
    binary_directory = options.binary_dir.resolve()
    output_directory = options.output_dir.resolve()
    wheel_path = None
    if not options.no_wheel:
        wheel_directory = (options.wheel_dir or output_directory).resolve()
        wheel_path = findWheel(wheel_directory, options.target_name)

    with tempfile.TemporaryDirectory(prefix="envoy-release-") as temporary_path:
        staging_directory = Path(temporary_path)
        bundle_root = staging_directory / "gt" / "envoy" / options.version
        copyReleaseFiles(
            bundle_root,
            binary_directory,
            options.version,
            options.windows,
            wheel_path,
        )
        archive_path = writeArchive(
            staging_directory,
            output_directory,
            options.version,
            options.target_name,
            options.windows,
        )

    print(os.fspath(archive_path))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
