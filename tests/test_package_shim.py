"""Tests for the envoy launcher archive layout."""

from __future__ import annotations

import importlib.util
import sys
import tarfile
import zipfile
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "package_shim.py"
SPEC = importlib.util.spec_from_file_location("package_shim", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
package_shim = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(package_shim)

VERSION = "v1.2.3"


def runPackaging(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    target: str = "windows-x86_64",
    build: bool = True,
) -> Path:
    """Run the packaging script as the release workflow does; return the archive it wrote."""
    windows = target.startswith("windows")
    binaries = tmp_path / "binaries"
    binaries.mkdir(exist_ok=True)
    if build:
        (binaries / ("envoy-shim.exe" if windows else "envoy-shim")).write_bytes(b"launcher")
    arguments = [
        "package_shim.py",
        "--version",
        VERSION,
        "--target-name",
        target,
        "--binary-dir",
        str(binaries),
        "--output-dir",
        str(tmp_path / "dist"),
    ]
    if windows:
        arguments.append("--windows")
    monkeypatch.setattr(sys, "argv", arguments)
    assert package_shim.main() == 0
    return Path(capsys.readouterr().out.strip())


def testWindowsArchiveHoldsTheLauncherUnderBothNamesAndItsPowerShellScripts(
    tmp_path, monkeypatch, capsys
):
    """``envoy.exe`` and ``en.exe`` are the same program, and the scripts that install it ship."""
    archive = runPackaging(tmp_path, monkeypatch, capsys)

    assert archive.name == f"envoy-shim-{VERSION}-windows-x86_64.zip"
    with zipfile.ZipFile(archive) as bundle:
        names = set(bundle.namelist())
        assert bundle.read("envoy.exe") == bundle.read("en.exe") == b"launcher"

    assert names == {
        "envoy.exe",
        "en.exe",
        "install-envoy.ps1",
        "uninstall-envoy.ps1",
        "LICENSE",
    }


def testUnixArchiveLinksEnToEnvoyAndKeepsScriptsRunnable(tmp_path, monkeypatch, capsys):
    """``en`` is a link so it cannot drift, and every program and script is executable."""
    archive = runPackaging(tmp_path, monkeypatch, capsys, target="linux-x86_64-musl")

    assert archive.name == f"envoy-shim-{VERSION}-linux-x86_64-musl.tar.gz"
    with tarfile.open(archive) as bundle:
        members = {member.name: member for member in bundle.getmembers()}

    assert set(members) == {
        "envoy",
        "en",
        "install-envoy.sh",
        "uninstall-envoy.sh",
        "LICENSE",
    }
    assert members["en"].issym() and members["en"].linkname == "envoy"
    for name in ("envoy", "install-envoy.sh", "uninstall-envoy.sh"):
        assert members[name].mode == 0o755, name
    assert members["LICENSE"].mode == 0o644
    assert all(member.uid == member.gid == 0 for member in members.values())


def testPackagingNeedsTheBuiltLauncher(tmp_path, monkeypatch, capsys):
    """A missing build is an error that names the file, not an empty archive."""
    with pytest.raises(FileNotFoundError, match="envoy-shim"):
        runPackaging(tmp_path, monkeypatch, capsys, build=False)
