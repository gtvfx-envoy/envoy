"""Check that a built executable reports the version it is packaged as."""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

# A label that names a version: v0.8.0, 0.8.0-rc.1, v0.8.0-alpha.2+g8ec805c. Any other label,
# such as ``dev``, belongs to a validation build, which is not held to a version.
VERSION_LABEL = re.compile(r"^[vV]?(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)(?:\+[0-9A-Za-z.-]+)?$")

# How long an executable gets to answer, so a hung one fails the packaging step.
ANSWER_TIMEOUT_SECONDS = 60


def versionOf(name: str) -> str | None:
    """Return the version a name stands for, or ``None`` when it names no version.

    A leading ``v`` and build metadata (``+g8ec805c``) are not part of the version: the metadata
    says which commit built it, not which version it is.

    Args:
        name: A label or an answer, such as ``v0.8.0-rc.1+g8ec805c``.

    Returns:
        The bare version, such as ``0.8.0-rc.1``.

    """
    match = VERSION_LABEL.match(name.strip())
    return match.group(1) if match else None


def checkReportedVersion(program: Path, label: str, argument: str = "--version") -> None:
    """Require that a built executable reports the version ``label`` names.

    Nothing is checked when the label names no version, as for a validation build.

    Args:
        program: Built executable to ask.
        label: Version the executable is being packaged as, including any ``v`` prefix.
        argument: What to ask it, whose answer ends in its version.

    Raises:
        ValueError: If the executable fails, or reports another version than the label.

    """
    wanted = versionOf(label)
    if wanted is None:
        return

    result = subprocess.run(
        [str(program), argument],
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        check=False,
        timeout=ANSWER_TIMEOUT_SECONDS,
    )
    lines = result.stdout.strip().splitlines()
    answer = lines[0].strip() if lines else ""
    reported = versionOf(answer.split()[-1]) if answer else None
    if result.returncode != 0 or reported != wanted:
        raise ValueError(
            f"{program.name} {argument} says {answer!r} (exit status {result.returncode}), "
            f"which is not version {label}. Build the executable from the commit that is being "
            "released, with the version in Cargo.toml set to it."
        )
