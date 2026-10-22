"""Create one platform-specific archive of the envoy launcher."""

from __future__ import annotations

import argparse
import os
import tarfile
import zipfile
from pathlib import Path
from typing import NamedTuple

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT_DIRECTORY = REPO_ROOT / "scripts" / "shim"

# Cargo builds the launcher as ``envoy-shim`` so that it cannot collide with the CLI's ``envoy``.
# It ships as ``envoy``, and ``en`` is the same program.
BUILT_NAME = "envoy-shim"

EXECUTABLE_MODE = 0o755
FILE_MODE = 0o644


class Entry(NamedTuple):
    """One file or symbolic link in the archive."""

    name: str
    source: Path | None = None
    link_to: str | None = None
    mode: int = FILE_MODE


def parseArguments() -> argparse.Namespace:
    """Parse launcher-packaging arguments.

    Returns:
        Parsed command-line options.

    """
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, help="Release version, including any v prefix.")
    parser.add_argument("--target-name", required=True, help="Artifact target label.")
    parser.add_argument(
        "--binary-dir",
        type=Path,
        required=True,
        help="Directory holding the built envoy-shim executable.",
    )
    parser.add_argument("--output-dir", type=Path, default=REPO_ROOT / "dist")
    parser.add_argument("--windows", action="store_true", help="Package a .exe in a ZIP.")
    return parser.parse_args()


def archiveEntries(binary_directory: Path, windows: bool) -> list[Entry]:
    """List what goes into the archive.

    The archive has no top-level folder, so a person publishing it to the share can unpack it
    straight into ``shim/<version>/<target>/`` and run the installer from there.

    Args:
        binary_directory: Directory holding the built launcher.
        windows: Whether to package the Windows launcher and its PowerShell scripts.

    Returns:
        The entries, in the order they are written.

    Raises:
        FileNotFoundError: If the launcher has not been built.

    """
    suffix = ".exe" if windows else ""
    built = binary_directory / f"{BUILT_NAME}{suffix}"
    if not built.is_file():
        raise FileNotFoundError(f"The launcher has not been built: {built}")

    if windows:
        entries = [
            Entry(f"envoy{suffix}", built, mode=EXECUTABLE_MODE),
            Entry(f"en{suffix}", built, mode=EXECUTABLE_MODE),
        ]
        script_names = ("install-envoy.ps1", "uninstall-envoy.ps1")
    else:
        # A link, so a copy of the launcher cannot drift from the one it is named for.
        entries = [
            Entry("envoy", built, mode=EXECUTABLE_MODE),
            Entry("en", link_to="envoy", mode=EXECUTABLE_MODE),
        ]
        script_names = ("install-envoy.sh", "uninstall-envoy.sh")

    entries.extend(
        Entry(name, SCRIPT_DIRECTORY / name, mode=EXECUTABLE_MODE) for name in script_names
    )
    entries.append(Entry("LICENSE", REPO_ROOT / "LICENSE"))
    return entries


def writeZip(entries: list[Entry], archive_path: Path) -> None:
    """Write the entries as a ZIP.

    Args:
        entries: Files to archive; ZIP holds no links, so each entry must have a source.
        archive_path: Destination of the archive.

    """
    with zipfile.ZipFile(archive_path, "w", zipfile.ZIP_DEFLATED) as archive:
        for entry in entries:
            if entry.source is None:
                raise ValueError(f"A ZIP cannot hold the link {entry.name}")
            archive.write(entry.source, entry.name)


def writeTar(entries: list[Entry], archive_path: Path) -> None:
    """Write the entries as a compressed tar with host-independent ownership and modes.

    Args:
        entries: Files and links to archive.
        archive_path: Destination of the archive.

    """
    with tarfile.open(archive_path, "w:gz") as archive:
        for entry in entries:
            if entry.source is not None:
                info = archive.gettarinfo(entry.source, arcname=entry.name)
            else:
                info = tarfile.TarInfo(entry.name)
                info.type = tarfile.SYMTYPE
                info.linkname = entry.link_to or ""
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            info.mode = entry.mode
            if entry.source is not None:
                with entry.source.open("rb") as stream:
                    archive.addfile(info, stream)
            else:
                archive.addfile(info)


def main() -> int:
    """Create one launcher archive.

    Returns:
        Process exit status.

    """
    options = parseArguments()
    entries = archiveEntries(options.binary_dir.resolve(), options.windows)
    output_directory = options.output_dir.resolve()
    output_directory.mkdir(parents=True, exist_ok=True)

    base_name = f"envoy-shim-{options.version}-{options.target_name}"
    if options.windows:
        archive_path = output_directory / f"{base_name}.zip"
        writeZip(entries, archive_path)
    else:
        archive_path = output_directory / f"{base_name}.tar.gz"
        writeTar(entries, archive_path)

    print(os.fspath(archive_path))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
