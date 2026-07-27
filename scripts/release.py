#!/usr/bin/env python3
"""Cut a new devc release.

Steps:
  1. Bump the minor version in Cargo.toml (0.1.3 -> 0.2.0).
  2. Build release binaries for Intel and Apple Silicon macOS.
  3. Commit the version bump.
  4. Tag the commit with the version.
  5. Push the commit and the tag.
  6. Create a GitHub release for the tag (via the `gh` CLI).
  7. Upload the release binaries as assets.
  8. Publish the Homebrew formula to rameshvarun/homebrew-tap.

Run from anywhere inside the repo:  python3 scripts/release.py
"""

import hashlib
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CARGO_TOML = REPO_ROOT / "Cargo.toml"
BIN_NAME = "devc"

# GitHub repo (for building release asset download URLs) and the Homebrew tap to publish to.
GITHUB_REPO = "rameshvarun/devc"
TAP_REPO = "https://github.com/rameshvarun/homebrew-tap.git"

# macOS targets to build, as (Rust target triple, asset suffix). The suffix must match what
# install.sh derives from `uname -s` / `uname -m` (lowercased) so the installer can find the right
# asset: on macOS that's darwin + arm64 (Apple Silicon) / x86_64 (Intel) — note arm64, not aarch64.
MAC_TARGETS = [
    ("aarch64-apple-darwin", "darwin-arm64"),
    ("x86_64-apple-darwin", "darwin-x86_64"),
]

# Homebrew formula rendered into the tap. The binaries are plain executables (not tarballs), so
# Homebrew stages each under its URL's filename — hence `bin.install "<asset name>" => "devc"`.
FORMULA_TEMPLATE = """\
class Devc < Formula
  desc "A simpler alternative to the Dev Containers CLI"
  homepage "https://github.com/rameshvarun/devc"
  version "{version}"

  on_macos do
    on_intel do
      url "{url_x86_64}"
      sha256 "{sha256_x86_64}"
    end
    on_arm do
      url "{url_arm64}"
      sha256 "{sha256_arm64}"
    end
  end

  def install
    binary = Hardware::CPU.arm? ? "{name_arm64}" : "{name_x86_64}"
    bin.install binary => "devc"
  end

  test do
    system bin/"devc", "--help"
  end
end
"""


def fail(msg: str) -> "None":
    print(f"release: {msg}", file=sys.stderr)
    sys.exit(1)


def run(cmd: list[str], cwd: Path = REPO_ROOT, **kwargs) -> subprocess.CompletedProcess:
    """Run a command (in the repo root by default), echoing it, and abort on failure."""
    print(f"$ {' '.join(cmd)}")
    result = subprocess.run(cmd, cwd=cwd, **kwargs)
    if result.returncode != 0:
        fail(f"command failed ({result.returncode}): {' '.join(cmd)}")
    return result


def capture(cmd: list[str], cwd: Path = REPO_ROOT) -> str:
    result = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True)
    if result.returncode != 0:
        fail(f"command failed ({result.returncode}): {' '.join(cmd)}\n{result.stderr}")
    return result.stdout.strip()


def check_prerequisites() -> None:
    for tool in ("cargo", "rustup", "git", "gh"):
        if shutil.which(tool) is None:
            fail(f"required tool not found on PATH: {tool}")
    # A clean tree keeps the release commit to exactly the version bump.
    if capture(["git", "status", "--porcelain"]):
        fail("working tree is not clean; commit or stash changes first")
    # Make sure the final `git push` will fast-forward. We bump, commit, and tag before pushing, so
    # a rejected push would leave the repo half-released; catch a stale branch up front instead.
    check_branch_up_to_date()
    # `gh` must be authenticated to create the release and upload the asset.
    if subprocess.run(["gh", "auth", "status"], cwd=REPO_ROOT).returncode != 0:
        fail("gh is not authenticated; run `gh auth login`")


def check_branch_up_to_date() -> None:
    """Fetch from origin and abort if the current branch is behind its remote counterpart."""
    branch = capture(["git", "rev-parse", "--abbrev-ref", "HEAD"])
    if branch == "HEAD":
        fail("detached HEAD; check out a branch before releasing")

    # Refresh remote-tracking refs so the behind-check below reflects the real remote state.
    run(["git", "fetch", "origin"])

    remote_ref = f"origin/{branch}"
    remote_exists = subprocess.run(
        ["git", "rev-parse", "--verify", "--quiet", remote_ref],
        cwd=REPO_ROOT, capture_output=True, text=True,
    ).returncode == 0
    # A brand-new branch has no remote counterpart yet; the push will simply create it.
    if not remote_exists:
        return

    behind = capture(["git", "rev-list", "--count", f"HEAD..{remote_ref}"])
    if behind != "0":
        fail(
            f"local {branch} is behind {remote_ref} by {behind} commit(s); "
            "integrate the remote changes first (e.g. `git pull --rebase`) before releasing"
        )


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


def sha256_of_file(path: Path) -> str:
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    print(f"  sha256({path.name}) = {digest}")
    return digest


def publish_homebrew_tap(version: str, tag: str, assets: dict[str, Path]) -> None:
    """Render the Homebrew formula pointing at this release's GitHub assets and push it to the tap.

    The SHA256s are computed from the local asset files, which are byte-identical to the ones just
    uploaded to the GitHub release, so the formula matches what `brew` will download.
    """
    arm, intel = assets["darwin-arm64"], assets["darwin-x86_64"]
    base_url = f"https://github.com/{GITHUB_REPO}/releases/download/{tag}"

    print("\nRendering Homebrew formula...")
    formula = FORMULA_TEMPLATE.format(
        version=version,
        url_arm64=f"{base_url}/{arm.name}",
        url_x86_64=f"{base_url}/{intel.name}",
        sha256_arm64=sha256_of_file(arm),
        sha256_x86_64=sha256_of_file(intel),
        name_arm64=arm.name,
        name_x86_64=intel.name,
    )

    with tempfile.TemporaryDirectory() as temp_dir:
        repo = Path(temp_dir) / "homebrew-tap"
        run(["git", "clone", "--depth", "1", TAP_REPO, str(repo)])

        formula_dir = repo / "Formula"
        formula_dir.mkdir(exist_ok=True)
        formula_path = formula_dir / f"{BIN_NAME}.rb"
        formula_path.write_text(formula)
        print(f"Wrote {formula_path.relative_to(repo)}")

        run(["git", "add", "-A"], cwd=repo)
        # Version always bumps, so there is normally a diff; guard against a no-op commit anyway.
        if not capture(["git", "status", "--porcelain"], cwd=repo):
            print("Formula already up to date; nothing to publish.")
            return
        run(["git", "commit", "-m", f"Update {BIN_NAME} formula to {version}"], cwd=repo)
        run(["git", "push"], cwd=repo)

    print(f"Published {BIN_NAME} {version} to the Homebrew tap.")
    print(f"Install with: brew install {GITHUB_REPO.split('/')[0]}/tap/{BIN_NAME}")


if __name__ == "__main__":
    check_prerequisites()

    old, version = bump_minor_version()
    tag = f"v{version}"

    if tag in capture(["git", "tag"]).split():
        fail(f"tag {tag} already exists")

    # Build a release binary for each macOS target (also refreshes Cargo.lock with the new version).
    # Name each asset with its platform suffix, since the binaries are architecture-specific.
    assets: dict[str, Path] = {}
    for triple, suffix in MAC_TARGETS:
        run(["rustup", "target", "add", triple])
        run(["cargo", "build", "--release", "--target", triple])

        binary = REPO_ROOT / "target" / triple / "release" / BIN_NAME
        if not binary.is_file():
            fail(f"release binary not found at {binary}")

        asset = binary.with_name(f"{BIN_NAME}-{version}-{suffix}")
        shutil.copy2(binary, asset)
        assets[suffix] = asset

    # Commit the version bump (Cargo.toml + the Cargo.lock update).
    run(["git", "add", "Cargo.toml", "Cargo.lock"])
    run(["git", "commit", "-m", f"Release {version}"])
    run(["git", "tag", "-a", tag, "-m", f"Release {version}"])

    # Push the commit and the tag to the current branch's upstream.
    branch = capture(["git", "rev-parse", "--abbrev-ref", "HEAD"])
    run(["git", "push", "origin", branch])
    run(["git", "push", "origin", tag])

    # Create the GitHub release and attach the per-platform binaries.
    run([
        "gh", "release", "create", tag,
        "--title", tag,
        "--notes", f"Release {version}",
        *(str(a) for a in assets.values()),
    ])

    # Publish the Homebrew formula pointing at the release assets we just uploaded.
    publish_homebrew_tap(version, tag, assets)

    print(f"\nReleased {tag} 🎉")
