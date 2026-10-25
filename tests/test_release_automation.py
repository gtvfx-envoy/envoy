"""Tests for Envoy release automation."""

from __future__ import annotations

import importlib.util
import re
from pathlib import Path
from unittest import mock

import pytest

SCRIPT_PATH = Path(__file__).parents[1] / "scripts" / "release_automation.py"
SPEC = importlib.util.spec_from_file_location("release_automation", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
release_automation = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release_automation)


@pytest.mark.parametrize(
    "version",
    ["0.6.0", "0.8.0-alpha.1", "1.0.0-rc.1", "2.3.4-rc.12"],
)
def testValidateVersionAcceptsReleaseVersions(version):
    """A release, an alpha, or a release candidate is returned unchanged."""
    assert release_automation.validateVersion(version) == version


@pytest.mark.parametrize("version", ["v0.6.0", "01.2.3", "1.2", "latest"])
def testValidateVersionRejectsInvalidValues(version):
    """Invalid release values are rejected."""
    with pytest.raises(ValueError, match="Invalid semantic version"):
        release_automation.validateVersion(version)


@pytest.mark.parametrize(
    "version",
    ["0.8.0-beta.1", "0.8.0-rc1", "0.8.0-rc", "0.8.0-rc.0", "0.8.0-alpha.1.2", "0.8.0-dev.1"],
)
def testValidateVersionRejectsOtherPreReleaseStages(version):
    """Only alpha.N and rc.N name a stage, so versions of a cycle sort and read the same way."""
    with pytest.raises(ValueError, match="alpha.N or rc.N"):
        release_automation.validateVersion(version)


def testValidateVersionLeavesBuildMetadataToBuilds():
    """A build adds its commit as metadata; a release version never carries it."""
    with pytest.raises(ValueError, match="build metadata"):
        release_automation.validateVersion("0.8.0-rc.1+g8ec805c")


def testReplaceWorkspaceVersionTargetsOnlyWorkspacePackage(tmp_path):
    """The workspace version replacement leaves dependency versions alone."""
    manifest_path = tmp_path / "Cargo.toml"
    manifest_path.write_text(
        '[workspace.package]\nversion = "0.5.1"\n\n[workspace.dependencies]\nitem = "1"\n',
        encoding="utf-8",
    )
    release_automation.replaceWorkspaceVersion(manifest_path, "0.6.0")
    assert manifest_path.read_text(encoding="utf-8") == (
        '[workspace.package]\nversion = "0.6.0"\n\n[workspace.dependencies]\nitem = "1"\n'
    )


def testPrepareReleaseRefreshesAndChecksLockfile(tmp_path):
    """Preparation asks Cargo to refresh the lockfile before validation."""
    rust_root = tmp_path / "rust"
    rust_root.mkdir()
    (rust_root / "Cargo.toml").write_text(
        '[workspace.package]\nversion = "0.5.1"\n',
        encoding="utf-8",
    )
    with (
        mock.patch.object(release_automation.subprocess, "run") as run_process,
        mock.patch.object(release_automation, "checkRelease") as check_release,
    ):
        release_automation.prepareRelease(tmp_path, "0.6.0")
    run_process.assert_called_once_with(
        ["cargo", "check", "--workspace", "--exclude", "envoy-py"],
        cwd=rust_root,
        check=True,
    )
    check_release.assert_called_once_with(tmp_path, "0.6.0")


def testReplaceUtilsDependencyUsesLocalEnvoyCore(tmp_path):
    """Compatibility testing removes the exact remote pin in its temporary copy."""
    manifest_path = tmp_path / "Cargo.toml"
    manifest_path.write_text(
        '[workspace.dependencies]\n'
        'envoy-core = { git = "https://example.test/envoy", tag = "v0.5.1", '
        'version = "=0.5.1" }\n',
        encoding="utf-8",
    )
    envoy_root = tmp_path / "envoy"
    release_automation.replaceUtilsDependency(manifest_path, envoy_root)
    assert 'envoy-core = { path = "' in manifest_path.read_text(encoding="utf-8")
    assert "tag =" not in manifest_path.read_text(encoding="utf-8")


def writeWorkspace(root: Path, versions: dict[str, str], manifest_version: str = "0.7.0") -> None:
    """Write a minimal Cargo workspace manifest and lockfile."""
    (root / "rust").mkdir()
    (root / "rust" / "Cargo.toml").write_text(
        f'[workspace.package]\nversion = "{manifest_version}"\n',
        encoding="utf-8",
    )
    blocks = "".join(
        f'[[package]]\nname = "{name}"\nversion = "{version}"\n\n'
        for name, version in versions.items()
    )
    (root / "rust" / "Cargo.lock").write_text(blocks, encoding="utf-8")


WORKSPACE_VERSIONS = {name: "0.7.0" for name in sorted(release_automation.WORKSPACE_PACKAGE_NAMES)}


def testCheckReleaseCoversEveryWorkspaceCrate(tmp_path):
    """A crate left behind at another version blocks the release, including the store."""
    assert "envoy-store" in release_automation.WORKSPACE_PACKAGE_NAMES
    writeWorkspace(tmp_path, WORKSPACE_VERSIONS)
    assert release_automation.checkRelease(tmp_path, "0.7.0") == "0.7.0"

    stale_root = tmp_path / "stale"
    stale_root.mkdir()
    writeWorkspace(stale_root, {**WORKSPACE_VERSIONS, "envoy-store": "0.6.2"})
    with pytest.raises(RuntimeError, match="envoy-store"):
        release_automation.checkRelease(stale_root, "0.7.0")

    missing_root = tmp_path / "missing"
    missing_root.mkdir()
    writeWorkspace(
        missing_root,
        {name: version for name, version in WORKSPACE_VERSIONS.items() if name != "envoy-store"},
    )
    with pytest.raises(RuntimeError, match="missing workspace packages"):
        release_automation.checkRelease(missing_root, "0.7.0")


def testEveryWorkspaceMemberIsCheckedForTheReleaseVersion():
    """A crate added to the workspace must also be added to the release check."""
    manifest = (Path(__file__).parents[1] / "rust" / "Cargo.toml").read_text(encoding="utf-8")
    members_block = re.search(r"members\s*=\s*\[(.*?)\]", manifest, re.DOTALL)
    assert members_block is not None
    members = set(re.findall(r'"([^"]+)"', members_block.group(1)))

    assert members == release_automation.WORKSPACE_PACKAGE_NAMES


@pytest.mark.parametrize(
    ("changed", "relevant"),
    [
        (["rust/envoy-core/src/runtime.rs"], True),
        (["rust/envoy-core/Cargo.toml"], True),
        (["rust/envoy-store/src/store.rs"], True),
        (["rust/envoy-store/Cargo.toml"], True),
        (["rust/envoy-cli/src/app.rs", "docs/concepts.md"], False),
        (["rust/envoy-store/tests/extra.rs"], False),
    ],
)
def testClassifyImpactWatchesTheCratesEnvoyUtilsBuilds(tmp_path, changed, relevant):
    """Envoy Utils builds envoy-core and, through it, envoy-store."""
    with mock.patch.object(release_automation, "changedFiles", return_value=changed):
        result = release_automation.classifyImpact(tmp_path, "base", "head")
    assert result["relevant"] is relevant
    assert result["classification"] == ("review" if relevant else "none")
