"""Tests for the Envoy release archive layout."""

from __future__ import annotations

import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).parents[1]
SCRIPT_PATH = REPO_ROOT / "scripts" / "package_release.py"
SPEC = importlib.util.spec_from_file_location("package_release", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
package_release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(package_release)

VERSION = "v1.2.3"
BUNDLE_ROOT = f"gt/envoy/{VERSION}"
BUNDLE_PATH_REFERENCE = re.compile(r"\$\{__BUNDLE__\}/([\w./-]+)")


def makeWheel(directory: Path, platform_tag: str, native: str = "_envoy.pyd") -> Path:
    """Write a minimal wheel holding the kinds of files a real one has."""
    directory.mkdir(parents=True, exist_ok=True)
    wheel_path = directory / f"envoy-0.0.0-cp310-abi3-{platform_tag}.whl"
    with zipfile.ZipFile(wheel_path, "w") as wheel:
        wheel.writestr("envoy/__init__.py", "")
        wheel.writestr(f"envoy/{native}", "native")
        wheel.writestr("envoy-0.0.0.dist-info/METADATA", "Name: envoy\n")
    return wheel_path


def runPackaging(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
    *extra: str,
    target: str = "windows-x86_64",
) -> Path:
    """Run the packaging script as the release workflow does; return the archive it wrote."""
    windows = target.startswith("windows")
    binaries = tmp_path / "binaries"
    binaries.mkdir(exist_ok=True)
    (binaries / ("envoy.exe" if windows else "envoy")).write_bytes(b"binary")
    arguments = [
        "package_release.py",
        "--version",
        VERSION,
        "--target-name",
        target,
        "--binary-dir",
        str(binaries),
        "--output-dir",
        str(tmp_path / "dist"),
        *extra,
    ]
    if windows:
        arguments.append("--windows")
    monkeypatch.setattr(sys, "argv", arguments)
    assert package_release.main() == 0
    return Path(capsys.readouterr().out.strip())


def testWindowsBundleShipsEntryPointAndPythonApi(tmp_path, monkeypatch, capsys):
    """The Windows bundle has a runnable ``en``, the unpacked wheel, and an env file for it."""
    makeWheel(tmp_path / "dist", "win_amd64")
    archive = runPackaging(tmp_path, monkeypatch, capsys)

    with zipfile.ZipFile(archive) as bundle:
        names = set(bundle.namelist())
        marker = json.loads(bundle.read(f"{BUNDLE_ROOT}/.bundle"))
        python_env = json.loads(bundle.read(f"{BUNDLE_ROOT}/.envoy/python_env.json"))

    expected = {
        "bin/envoy.exe",
        "bin/en.bat",
        ".envoy/global_env.json",
        "py/envoy/__init__.py",
        "py/envoy/_envoy.pyd",
    }
    assert {f"{BUNDLE_ROOT}/{path}" for path in expected} <= names
    assert marker["bndlid"] == "gt:envoy"
    assert marker["version"] == VERSION
    assert python_env == {"+=PYTHONPATH": "${__BUNDLE__}/py"}


def testEveryBundlePathAnEnvFileReferencesIsShipped(tmp_path, monkeypatch, capsys):
    """An env file must never point at a path the packaged bundle does not contain."""
    makeWheel(tmp_path / "dist", "win_amd64")
    archive = runPackaging(tmp_path, monkeypatch, capsys)

    with zipfile.ZipFile(archive) as bundle:
        names = set(bundle.namelist())
        env_files = [
            name
            for name in names
            if name.startswith(f"{BUNDLE_ROOT}/.envoy/") and name.endswith("_env.json")
        ]
        assert env_files
        for env_file in env_files:
            for reference in BUNDLE_PATH_REFERENCE.findall(bundle.read(env_file).decode()):
                shipped = f"{BUNDLE_ROOT}/{reference}"
                assert shipped in names or any(name.startswith(f"{shipped}/") for name in names), (
                    f"{env_file} references {reference}, which the archive does not contain"
                )


def testLinuxArchiveKeepsExecutablesRunnableAndShipsPythonApi(tmp_path, monkeypatch, capsys):
    """The Unix archive keeps ``envoy`` and ``en`` executable and carries the wheel."""
    makeWheel(
        tmp_path / "dist",
        "manylinux_2_17_x86_64.manylinux2014_x86_64",
        native="_envoy.abi3.so",
    )
    archive = runPackaging(tmp_path, monkeypatch, capsys, target="linux-x86_64-musl")

    with tarfile.open(archive) as bundle:
        members = {member.name: member for member in bundle.getmembers()}

    assert members[f"{BUNDLE_ROOT}/bin/envoy"].mode == 0o755
    assert members[f"{BUNDLE_ROOT}/bin/en"].mode == 0o755
    assert members[f"{BUNDLE_ROOT}/py/envoy/__init__.py"].mode == 0o644
    assert f"{BUNDLE_ROOT}/py/envoy/_envoy.abi3.so" in members


def testPackagingFailsWithoutAWheelUnlessSkipped(tmp_path, monkeypatch, capsys):
    """A release cannot silently lose the Python API; skipping it must be explicit."""
    with pytest.raises(FileNotFoundError, match="--no-wheel"):
        runPackaging(tmp_path, monkeypatch, capsys)

    archive = runPackaging(tmp_path, monkeypatch, capsys, "--no-wheel")
    with zipfile.ZipFile(archive) as bundle:
        names = bundle.namelist()

    assert not any(name.startswith(f"{BUNDLE_ROOT}/py/") for name in names)
    assert f"{BUNDLE_ROOT}/.envoy/python_env.json" not in names
    assert f"{BUNDLE_ROOT}/bin/envoy.exe" in names


def testWheelBuiltForAnotherPlatformIsNotPackaged(tmp_path, monkeypatch, capsys):
    """A leftover wheel for a different platform must not end up in the bundle."""
    makeWheel(tmp_path / "dist", "win_amd64")

    with pytest.raises(FileNotFoundError, match="linux-x86_64-musl"):
        runPackaging(tmp_path, monkeypatch, capsys, target="linux-x86_64-musl")


def testAmbiguousWheelsAreRejected(tmp_path, monkeypatch, capsys):
    """Two wheels for one target are an error, not a guess."""
    makeWheel(tmp_path / "dist", "macosx_10_12_x86_64")
    makeWheel(tmp_path / "dist", "macosx_10_15_x86_64")

    with pytest.raises(ValueError, match="Expected one Python wheel"):
        runPackaging(tmp_path, monkeypatch, capsys, target="macos-x86_64")


@pytest.mark.skipif(sys.platform != "win32", reason="runs the Windows batch launchers")
def testWindowsLaunchersRunTheEnvoyTheyFindAndKeepItsExitCode(tmp_path):
    """Each launcher runs the envoy next to it and returns that envoy's exit status.

    A copy of ``cmd.exe`` stands in for ``envoy.exe``: ``envoy.exe /c exit N`` exits with ``N``.
    """
    standin = Path(os.environ["COMSPEC"])

    def launch(launcher: Path, code: int) -> int:
        return subprocess.run([str(launcher), "/c", "exit", str(code)], check=False).returncode

    shipped = tmp_path / "shipped" / "bin"
    shipped.mkdir(parents=True)
    shutil.copy2(REPO_ROOT / "bin" / "en.bat", shipped / "en.bat")
    shutil.copy2(standin, shipped / "envoy.exe")
    assert launch(shipped / "en.bat", 7) == 7

    checkout = tmp_path / "checkout" / "bin"
    checkout.mkdir(parents=True)
    shutil.copy2(REPO_ROOT / "bin" / "en.bat", checkout / "en.bat")
    (checkout / "envoy.bat").write_text("@exit /b 9\r\n", encoding="ascii")
    assert launch(checkout / "en.bat", 0) == 9

    for profile, code in (("release", 5), ("debug", 6)):
        development = tmp_path / profile / "bin"
        development.mkdir(parents=True)
        shutil.copy2(REPO_ROOT / "bin" / "envoy.bat", development / "envoy.bat")
        build = tmp_path / profile / "rust" / "target" / profile
        build.mkdir(parents=True)
        shutil.copy2(standin, build / "envoy.exe")
        assert launch(development / "envoy.bat", code) == code
