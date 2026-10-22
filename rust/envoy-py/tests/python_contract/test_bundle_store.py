"""Contract tests for running commands from the local bundle store through the Python API.

A published bundle (one with a ``.bundle`` marker) is copied into the store the first time an
environment is built to *run* a command, exactly as ``envoy COMMAND`` does. Looking at a command
(tracing, diagnosing) reads the bundle where it is and copies nothing.
"""

import json
from pathlib import Path

import pytest

import envoy


def _makePublishedBundle(root: Path, name: str, version: str) -> Path:
    """Create a published bundle whose ``show`` command records the bundle root it loaded from."""
    bundle = root / name / version
    envoy_dir = bundle / ".envoy"
    envoy_dir.mkdir(parents=True)
    (bundle / ".bundle").write_text(
        json.dumps({"bndlid": f"gt:{name}", "name": name, "version": version}), encoding="utf-8"
    )
    (envoy_dir / "commands.json").write_text(
        json.dumps({"show": {"environment": ["show_env.json"], "alias": ["tool"]}}),
        encoding="utf-8",
    )
    (envoy_dir / "show_env.json").write_text(
        json.dumps({"SHOWN_ROOT": "${__BUNDLE__}"}), encoding="utf-8"
    )
    return bundle


@pytest.fixture
def share(tmp_path, monkeypatch):
    """A share holding one published bundle, with caching on and a cache root of its own."""
    root = tmp_path / "share"
    _makePublishedBundle(root, "tool", "1.0.0")
    monkeypatch.setenv("ENVOY_CACHE", "on")
    monkeypatch.setenv("ENVOY_CACHE_ROOT", str(tmp_path / "cache"))
    return root


def testBuildingAnEnvironmentToRunCopiesThePublishedBundleIntoTheStore(share, tmp_path):
    """``getEnvironment`` points the command at the cached copy, not at the share."""
    cache_root = tmp_path / "cache"

    env = envoy.getEnvironment("show", bundle_roots=[str(share)])

    shown = Path(env["SHOWN_ROOT"]).resolve()
    assert shown.is_relative_to(cache_root.resolve())
    assert (shown / ".envoy" / "commands.json").is_file()


def testTracingAndDiagnosingLeaveBundlesWhereTheyAre(share, tmp_path):
    """Looking at a command must not copy anything."""
    cache_root = tmp_path / "cache"
    source = (share / "tool" / "1.0.0").resolve()

    traced, _ = envoy.traceEnvironment("show", "SHOWN_ROOT", bundle_roots=[str(share)])
    diagnosed, _ = envoy.diagnoseEnvironment("show", bundle_roots=[str(share)])

    assert Path(traced["SHOWN_ROOT"]).resolve() == source
    assert Path(diagnosed["SHOWN_ROOT"]).resolve() == source
    assert not cache_root.exists()


def testCachingCanBeSwitchedOffForTheApi(share, tmp_path, monkeypatch):
    """``ENVOY_CACHE=off`` runs the command from where the bundle is."""
    monkeypatch.setenv("ENVOY_CACHE", "off")

    env = envoy.getEnvironment("show", bundle_roots=[str(share)])

    assert Path(env["SHOWN_ROOT"]).resolve() == (share / "tool" / "1.0.0").resolve()
    assert not (tmp_path / "cache").exists()
