#!/usr/bin/env python3
"""Cut a new devc release.

Steps:
  1. Bump the minor version in Cargo.toml (0.1.3 -> 0.2.0).
  2. Build a release binary.
  3. Commit the version bump.
  4. Tag the commit with the version.
  5. Push the commit and the tag.
  6. Create a GitHub release for the tag (via the `gh` CLI).
  7. Upload the release binary as an asset.

Run from anywhere inside the repo:  python3 scripts/release.py
"""

import platform
import re
import shutil
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CARGO_TOML = REPO_ROOT / "Cargo.toml"
BIN_NAME = "devc"


def fail(msg: str) -> "None":
    print(f"release: {msg}", file=sys.stderr)
    sys.exit(1)


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    """Run a command in the repo root, echoing it, and abort on failure."""
    print(f"$ {' '.join(cmd)}")
    result = subprocess.run(cmd, cwd=REPO_ROOT, **kwargs)
    if result.returncode != 0:
        fail(f"command failed ({result.returncode}): {' '.join(cmd)}")
    return result


def capture(cmd: list[str]) -> str:
    result = subprocess.run(cmd, cwd=REPO_ROOT, capture_output=True, text=True)
    if result.returncode != 0:
        fail(f"command failed ({result.returncode}): {' '.join(cmd)}\n{result.stderr}")
    return result.stdout.strip()


def check_prerequisites() -> None:
    for tool in ("cargo", "git", "gh"):
        if shutil.which(tool) is None:
            fail(f"required tool not found on PATH: {tool}")
    # A clean tree keeps the release commit to exactly the version bump.
    if capture(["git", "status", "--porcelain"]):
        fail("working tree is not clean; commit or stash changes first")
    # `gh` must be authenticated to create the release and upload the asset.
    if subprocess.run(["gh", "auth", "status"], cwd=REPO_ROOT).returncode != 0:
        fail("gh is not authenticated; run `gh auth login`")


def bump_minor_version() -> tuple[str, str]:
    """Read the current version from Cargo.toml, bump the minor, and write it back."""
    text = CARGO_TOML.read_text()
    match = re.search(r'^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"', text, re.MULTILINE)
    if not match:
        fail("could not find a semver `version = \"x.y.z\"` in Cargo.toml")
    major, minor, _patch = (int(g) for g in match.groups())
    old = f"{major}.{minor}.{_patch}"
    new = f"{major}.{minor + 1}.0"

    start, end = match.span(0)
    updated = text[:start] + f'version = "{new}"' + text[end:]
    CARGO_TOML.write_text(updated)
    print(f"version: {old} -> {new}")
    return old, new


if __name__ == "__main__":
    check_prerequisites()

    old, version = bump_minor_version()
    tag = f"v{version}"

    if tag in capture(["git", "tag"]).split():
        fail(f"tag {tag} already exists")

    # Build the release binary (also refreshes Cargo.lock with the new version).
    run(["cargo", "build", "--release"])

    binary = REPO_ROOT / "target" / "release" / BIN_NAME
    if not binary.is_file():
        fail(f"release binary not found at {binary}")

    # Commit the version bump (Cargo.toml + the Cargo.lock update).
    run(["git", "add", "Cargo.toml", "Cargo.lock"])
    run(["git", "commit", "-m", f"Release {version}"])
    run(["git", "tag", "-a", tag, "-m", f"Release {version}"])

    # Push the commit and the tag to the current branch's upstream.
    branch = capture(["git", "rev-parse", "--abbrev-ref", "HEAD"])
    run(["git", "push", "origin", branch])
    run(["git", "push", "origin", tag])

    # Name the asset with the target platform, since the binary is host-specific.
    system = platform.system().lower()
    arch = platform.machine().lower()
    asset = REPO_ROOT / "target" / "release" / f"{BIN_NAME}-{version}-{system}-{arch}"
    shutil.copy2(binary, asset)

    # Create the GitHub release and attach the binary.
    run([
        "gh", "release", "create", tag,
        "--title", tag,
        "--notes", f"Release {version}",
        str(asset),
    ])

    print(f"\nReleased {tag} 🎉")
