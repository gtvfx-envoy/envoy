"""Tests for the check that a built executable reports the version it is packaged as."""

from __future__ import annotations

import importlib.util
import os
import stat
from pathlib import Path

import pytest

SCRIPT_PATH = Path(__file__).parents[1] / "scripts" / "release_version.py"
SPEC = importlib.util.spec_from_file_location("release_version", SCRIPT_PATH)
assert SPEC is not None and SPEC.loader is not None
release_version = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release_version)


def makeProgram(directory: Path, answer: str, exit_status: int = 0) -> Path:
    """Write a program that answers every question with ``answer`` and ``exit_status``."""
    if os.name == "nt":
        program = directory / "envoy.bat"
        program.write_text(f"@echo off\r\necho {answer}\r\nexit /b {exit_status}\r\n")
    else:
        program = directory / "envoy"
        program.write_text(f'#!/bin/sh\necho "{answer}"\nexit {exit_status}\n')
        program.chmod(program.stat().st_mode | stat.S_IEXEC)
    return program


@pytest.mark.parametrize(
    ("name", "version"),
    [
        ("v0.8.0", "0.8.0"),
        ("0.8.0", "0.8.0"),
        ("v0.8.0-rc.1", "0.8.0-rc.1"),
        ("v0.8.0-alpha.2+g8ec805c", "0.8.0-alpha.2"),
        ("v0.8.0+g8ec805c.dirty", "0.8.0"),
        (" v0.8.0\n", "0.8.0"),
    ],
)
def testAVersionIsWhatRemainsOfItsNameWithoutAVOrBuildMetadata(name, version):
    """The commit that built a version says nothing about which version it is."""
    assert release_version.versionOf(name) == version


@pytest.mark.parametrize("name", ["dev", "main", "v0.8", "latest", "1.2.3.4"])
def testOtherNamesNameNoVersion(name):
    """A validation build can be labelled anything, and is not held to a version."""
    assert release_version.versionOf(name) is None


def testAnExecutableThatReportsTheVersionItIsPackagedAsPasses(tmp_path):
    """The answer may carry the commit it was built from."""
    program = makeProgram(tmp_path, "envoy v0.8.0-rc.1+g8ec805c")

    release_version.checkReportedVersion(program, "v0.8.0-rc.1")


def testAnExecutableThatReportsAnotherVersionIsRefused(tmp_path):
    """An archive labelled with one version must not hold an executable of another."""
    program = makeProgram(tmp_path, "envoy v0.7.0-51-g8ec805c")

    with pytest.raises(ValueError, match=r"is not version v0\.8\.0-rc\.1"):
        release_version.checkReportedVersion(program, "v0.8.0-rc.1")


def testACandidateIsNotTheReleaseItIsACandidateFor(tmp_path):
    """The stage is part of the version."""
    program = makeProgram(tmp_path, "envoy v0.8.0-rc.1")

    with pytest.raises(ValueError, match=r"is not version v0\.8\.0\. "):
        release_version.checkReportedVersion(program, "v0.8.0")


def testAnExecutableThatFailsIsRefused(tmp_path):
    """An answer from a program that failed cannot be trusted."""
    program = makeProgram(tmp_path, "envoy v0.8.0", exit_status=3)

    with pytest.raises(ValueError, match="exit status 3"):
        release_version.checkReportedVersion(program, "v0.8.0")


def testAValidationLabelIsNotCheckedAndTheExecutableIsNotRun(tmp_path):
    """A label such as ``dev`` names no version, so there is nothing to hold the build to."""
    release_version.checkReportedVersion(tmp_path / "missing" / "envoy", "dev")
