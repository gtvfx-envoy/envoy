"""Release preparation and downstream compatibility automation for Envoy."""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import tempfile
from pathlib import Path

SEMVER_PATTERN = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?"
    r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$"
)
WORKSPACE_PACKAGE_NAMES = {
    "envoy-cli",
    "envoy-config",
    "envoy-core",
    "envoy-py",
    "envoy-shim",
    "envoy-store",
}
# Crates whose sources reach Envoy Utils: it depends on envoy-core, which depends on envoy-store.
UTILS_REACHABLE_CRATES = ("envoy-core", "envoy-store")
DIRECT_UTILS_APIS = (
    "envoy_core::discovery::{discover_bundles_auto, Bundle}",
    "envoy_core::stack::Stack",
    "envoy_core::stack_registry::STACK_ROOTS_VAR",
)


# The stages of a release cycle: alpha.N while work heads for it, rc.N for a candidate, and no
# stage for the release itself. They sort in that order.
PRERELEASE_STAGES = re.compile(r"^(?:alpha|rc)\.[1-9]\d*$")


def validateVersion(version: str) -> str:
    """Validate and return an unprefixed release version.

    A release version is ``X.Y.Z``, ``X.Y.Z-alpha.N`` or ``X.Y.Z-rc.N``. Build metadata is left to
    builds, which add the commit they were built from.

    Args:
        version: Version without a ``v`` prefix.

    Returns:
        The version, unchanged.

    Raises:
        ValueError: If it is not a semantic version, or is not one a release may have.

    """
    match = SEMVER_PATTERN.fullmatch(version)
    if match is None:
        raise ValueError(f"Invalid semantic version: {version!r}")
    prerelease, metadata = match.group(4), match.group(5)
    if metadata is not None:
        raise ValueError(f"{version!r} carries build metadata, which only a build adds.")
    if prerelease is not None and not PRERELEASE_STAGES.fullmatch(prerelease):
        raise ValueError(f"{version!r} has a pre-release stage other than alpha.N or rc.N.")
    return version


def replaceWorkspaceVersion(manifest_path: Path, version: str) -> None:
    """Replace exactly one workspace package version."""
    contents = manifest_path.read_text(encoding="utf-8")
    pattern = re.compile(r"(?ms)(^\[workspace\.package\]\s*.*?)^version\s*=\s*\"[^\"]+\"$")
    updated, replacement_count = pattern.subn(
        lambda match: f'{match.group(1)}version = "{version}"',
        contents,
        count=1,
    )
    if replacement_count != 1:
        raise RuntimeError(
            f"Expected one [workspace.package] version in {manifest_path}; "
            f"found {replacement_count}."
        )
    manifest_path.write_text(updated, encoding="utf-8")


def workspaceVersions(repository_root: Path) -> tuple[str, dict[str, str]]:
    """Return the manifest version and local package versions from Cargo.lock."""
    rust_root = repository_root / "rust"
    manifest_contents = (rust_root / "Cargo.toml").read_text(encoding="utf-8")
    manifest_match = re.search(
        r"(?ms)^\[workspace\.package\]\s*.*?^version\s*=\s*\"([^\"]+)\"$",
        manifest_contents,
    )
    if manifest_match is None:
        raise RuntimeError("Cargo.toml has no [workspace.package] version.")
    manifest_version = manifest_match.group(1)
    lock_contents = (rust_root / "Cargo.lock").read_text(encoding="utf-8")
    package_versions = {}
    for package_block in re.split(r"(?m)^\[\[package\]\]\s*$", lock_contents)[1:]:
        name_match = re.search(r'(?m)^name = "([^"]+)"$', package_block)
        version_match = re.search(r'(?m)^version = "([^"]+)"$', package_block)
        if name_match and version_match and name_match.group(1) in WORKSPACE_PACKAGE_NAMES:
            package_versions[name_match.group(1)] = version_match.group(1)
    return manifest_version, package_versions


def checkRelease(repository_root: Path, expected_version: str | None = None) -> str:
    """Validate Envoy's release versions and return the current version."""
    manifest_version, package_versions = workspaceVersions(repository_root)
    validateVersion(manifest_version)
    if set(package_versions) != WORKSPACE_PACKAGE_NAMES:
        missing_names = sorted(WORKSPACE_PACKAGE_NAMES - set(package_versions))
        raise RuntimeError(f"Cargo.lock is missing workspace packages: {missing_names}")
    mismatches = {
        package_name: package_version
        for package_name, package_version in package_versions.items()
        if package_version != manifest_version
    }
    if mismatches:
        raise RuntimeError(
            f"Cargo workspace version {manifest_version} disagrees with Cargo.lock: {mismatches}"
        )
    if expected_version is not None and manifest_version != validateVersion(expected_version):
        raise RuntimeError(
            f"Expected Envoy {expected_version}, but the workspace is {manifest_version}."
        )
    return manifest_version


def prepareRelease(repository_root: Path, version: str) -> None:
    """Update Envoy's Cargo version and refresh the lockfile."""
    validated_version = validateVersion(version)
    rust_root = repository_root / "rust"
    replaceWorkspaceVersion(rust_root / "Cargo.toml", validated_version)
    subprocess.run(
        ["cargo", "check", "--workspace", "--exclude", "envoy-py"],
        cwd=rust_root,
        check=True,
    )
    checkRelease(repository_root, validated_version)


def changedFiles(repository_root: Path, base_revision: str, head_revision: str) -> tuple[str, ...]:
    """Return files changed between two Git revisions."""
    completed_process = subprocess.run(
        ["git", "diff", "--name-only", f"{base_revision}...{head_revision}"],
        cwd=repository_root,
        check=True,
        capture_output=True,
        text=True,
    )
    return tuple(line.strip() for line in completed_process.stdout.splitlines() if line.strip())


def lockfileHasDependencyChanges(
    repository_root: Path,
    base_revision: str,
    head_revision: str,
) -> bool:
    """Return whether Cargo.lock changed beyond workspace version lines."""
    completed_process = subprocess.run(
        [
            "git",
            "diff",
            "--unified=0",
            base_revision,
            head_revision,
            "--",
            "rust/Cargo.lock",
        ],
        cwd=repository_root,
        check=True,
        capture_output=True,
        text=True,
    )
    changed_lines = []
    for line in completed_process.stdout.splitlines():
        if not line.startswith(("+", "-")) or line.startswith(("+++", "---")):
            continue
        changed_lines.append(line[1:].strip())
    return any(not re.fullmatch(r'version = "[^"]+"', line) for line in changed_lines)


def workspaceManifestHasDependencyChanges(
    repository_root: Path,
    base_revision: str,
    head_revision: str,
) -> bool:
    """Return whether the workspace manifest changed beyond its release version."""
    completed_process = subprocess.run(
        [
            "git",
            "diff",
            "--unified=0",
            base_revision,
            head_revision,
            "--",
            "rust/Cargo.toml",
        ],
        cwd=repository_root,
        check=True,
        capture_output=True,
        text=True,
    )
    changed_lines = [
        line[1:].strip()
        for line in completed_process.stdout.splitlines()
        if line.startswith(("+", "-")) and not line.startswith(("+++", "---"))
    ]
    return any(not re.fullmatch(r'version = "[^"]+"', line) for line in changed_lines)


def classifyImpact(repository_root: Path, base_revision: str, head_revision: str) -> dict:
    """Classify whether a revision range can affect Envoy Utils."""
    changed_files = changedFiles(repository_root, base_revision, head_revision)
    relevant_files = [
        file_path
        for file_path in changed_files
        if any(
            file_path.startswith(f"rust/{crate}/src/") or file_path == f"rust/{crate}/Cargo.toml"
            for crate in UTILS_REACHABLE_CRATES
        )
    ]
    if "rust/Cargo.lock" in changed_files and lockfileHasDependencyChanges(
        repository_root,
        base_revision,
        head_revision,
    ):
        relevant_files.append("rust/Cargo.lock")
    if "rust/Cargo.toml" in changed_files and workspaceManifestHasDependencyChanges(
        repository_root,
        base_revision,
        head_revision,
    ):
        relevant_files.append("rust/Cargo.toml")
    return {
        "classification": "review" if relevant_files else "none",
        "relevant": bool(relevant_files),
        "changed_files": list(changed_files),
        "relevant_files": relevant_files,
        "direct_apis": list(DIRECT_UTILS_APIS),
    }


def replaceUtilsDependency(manifest_path: Path, envoy_root: Path) -> None:
    """Replace Envoy Utils' pinned dependency with the candidate local crate."""
    contents = manifest_path.read_text(encoding="utf-8")
    dependency_pattern = re.compile(r"(?m)^envoy-core\s*=\s*\{[^\n]+\}$")
    local_dependency = envoy_root / "rust" / "envoy-core"
    replacement = f'envoy-core = {{ path = "{local_dependency.as_posix()}" }}'
    updated, replacement_count = dependency_pattern.subn(replacement, contents)
    if replacement_count != 1:
        raise RuntimeError(
            f"Expected one envoy-core dependency in {manifest_path}; found {replacement_count}."
        )
    manifest_path.write_text(updated, encoding="utf-8")


def testUtilsCompatibility(
    repository_root: Path,
    utils_root: Path,
    output_path: Path | None = None,
) -> dict:
    """Test Envoy Utils against this Envoy checkout in an isolated copy."""
    with tempfile.TemporaryDirectory(prefix="envoy-utils-compat-") as temporary_directory:
        temporary_root = Path(temporary_directory) / "envoy_utils"
        shutil.copytree(
            utils_root,
            temporary_root,
            ignore=shutil.ignore_patterns(".git", "target", ".codebase-memory"),
        )
        replaceUtilsDependency(temporary_root / "rust" / "Cargo.toml", repository_root)
        completed_process = subprocess.run(
            ["cargo", "test", "--workspace"],
            cwd=temporary_root / "rust",
            check=False,
        )
    result = {
        "classification": "review" if completed_process.returncode == 0 else "required",
        "return_code": completed_process.returncode,
        "direct_apis": list(DIRECT_UTILS_APIS),
    }
    if output_path is not None:
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    return result


def writeGitHubOutput(output_path: Path, values: dict[str, str]) -> None:
    """Append simple values to a GitHub Actions output file."""
    with output_path.open("a", encoding="utf-8") as output_file:
        for name, value in values.items():
            output_file.write(f"{name}={value}\n")


def buildParser() -> argparse.ArgumentParser:
    """Build the command-line parser."""
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    check_parser = subparsers.add_parser("check")
    check_parser.add_argument("--expect-version")

    prepare_parser = subparsers.add_parser("prepare")
    prepare_parser.add_argument("--version", required=True)

    impact_parser = subparsers.add_parser("impact")
    impact_parser.add_argument("--base", required=True)
    impact_parser.add_argument("--head", required=True)
    impact_parser.add_argument("--output")
    impact_parser.add_argument("--github-output")

    compatibility_parser = subparsers.add_parser("compatibility")
    compatibility_parser.add_argument("--utils-root", required=True)
    compatibility_parser.add_argument("--output")
    return parser


def main(arguments: list[str] | None = None) -> int:
    """Run Envoy release automation."""
    parser = buildParser()
    args = parser.parse_args(arguments)
    repository_root = Path(__file__).resolve().parent.parent
    try:
        if args.command == "check":
            version = checkRelease(repository_root, args.expect_version)
            print(f"Envoy release state is valid for v{version}.")
        elif args.command == "prepare":
            prepareRelease(repository_root, args.version)
            print(f"Prepared Envoy v{args.version}.")
        elif args.command == "impact":
            result = classifyImpact(repository_root, args.base, args.head)
            rendered_result = json.dumps(result, indent=2) + "\n"
            if args.output:
                Path(args.output).write_text(rendered_result, encoding="utf-8")
            else:
                print(rendered_result, end="")
            if args.github_output:
                writeGitHubOutput(
                    Path(args.github_output),
                    {
                        "relevant": str(result["relevant"]).lower(),
                        "classification": str(result["classification"]),
                    },
                )
        else:
            result = testUtilsCompatibility(
                repository_root,
                Path(args.utils_root).resolve(),
                Path(args.output) if args.output else None,
            )
            if result["return_code"] != 0:
                print("Envoy Utils requires changes for this Envoy candidate.")
                return int(result["return_code"])
            print("Envoy Utils is compatible; release impact still requires review.")
    except (OSError, RuntimeError, subprocess.CalledProcessError, ValueError) as error:
        parser.error(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
