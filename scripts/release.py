#!/usr/bin/env python3
"""Cut a new devc release.

Steps:
  1. Bump the minor version in Cargo.toml (0.1.3 -> 0.2.0).
  2. Build release binaries for Intel/Apple Silicon macOS (cargo) and x86_64/aarch64 Linux
     (static musl via `cross`, Docker).
  3. Commit the version bump.
  4. Tag the commit with the version.
  5. Push the commit and the tag.
  6. Create a GitHub release for the tag (via the `gh` CLI).
  7. Upload the release binaries as assets.
  8. Publish the Homebrew formula to rameshvarun/homebrew-tap.

Run from anywhere inside the repo:  python3 scripts/release.py
"""

import hashlib
import os
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

# Pinned `cross` CLI version (its last release) so Linux builds are reproducible on any checkout.
# The build *images* are pinned separately by digest in Cross.toml — deliberately newer than the
# 0.2.5 images, whose glibc is too old for modern Rust's cross-arch build scripts (see Cross.toml).
CROSS_VERSION = "0.2.5"

# cross 0.2.5 runs the build in an amd64 Linux image and mounts a host rustup toolchain matching the
# image into it (a full toolchain, not just a target). We install it up front; assumes the default
# `stable` channel (there is no rust-toolchain.toml pinning a channel in this repo).
CROSS_HOST_TOOLCHAIN = "stable-x86_64-unknown-linux-gnu"

# macOS targets to build, as (Rust target triple, asset suffix). The suffix must match what
# install.sh derives from `uname -s` / `uname -m` (lowercased) so the installer can find the right
# asset: on macOS that's darwin + arm64 (Apple Silicon) / x86_64 (Intel) — note arm64, not aarch64.
MAC_TARGETS = [
    ("aarch64-apple-darwin", "darwin-arm64"),
    ("x86_64-apple-darwin", "darwin-x86_64"),
]

# Linux targets, cross-compiled from macOS with `cross` (Docker). We build static musl binaries so
# they run on any distro with no libc/OpenSSL/cert deps (devc bundles CA roots via webpki-roots).
# The suffix matches `uname -m` on Linux (x86_64 / aarch64), so install.sh finds these unchanged.
LINUX_TARGETS = [
    ("x86_64-unknown-linux-musl", "linux-x86_64"),
    ("aarch64-unknown-linux-musl", "linux-aarch64"),
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
      url "{url_darwin_x86_64}"
      sha256 "{sha256_darwin_x86_64}"
    end
    on_arm do
      url "{url_darwin_arm64}"
      sha256 "{sha256_darwin_arm64}"
    end
  end

  on_linux do
    on_intel do
      url "{url_linux_x86_64}"
      sha256 "{sha256_linux_x86_64}"
    end
    on_arm do
      url "{url_linux_aarch64}"
      sha256 "{sha256_linux_aarch64}"
    end
  end

  def install
    binary = if OS.mac?
      Hardware::CPU.arm? ? "{name_darwin_arm64}" : "{name_darwin_x86_64}"
    else
      Hardware::CPU.arm? ? "{name_linux_aarch64}" : "{name_linux_x86_64}"
    end
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
    # Linux binaries are cross-compiled in Docker via `cross`, so the daemon must be reachable.
    if subprocess.run(
        ["docker", "info"], cwd=REPO_ROOT, capture_output=True
    ).returncode != 0:
        fail("Docker is not available; `cross` needs a running Docker daemon to build Linux targets")
    ensure_cross()


def ensure_cross() -> None:
    """Install the pinned `cross` and the environment it needs to cross-compile Linux from here.

    Pinning the CLI alongside the image tags (Cross.toml) keeps Linux builds reproducible on any
    checkout. `--locked` builds cross from its own published Cargo.lock for a deterministic CLI.
    """
    installed = None
    if shutil.which("cross") is not None:
        # `cross --version` prints e.g. "cross 0.2.5"; grab the version token.
        out = subprocess.run(
            ["cross", "--version"], cwd=REPO_ROOT, capture_output=True, text=True
        )
        if out.returncode == 0:
            first = out.stdout.split("\n", 1)[0].split()
            installed = first[1] if len(first) >= 2 else None

    if installed == CROSS_VERSION:
        print(f"cross {CROSS_VERSION} already installed")
    else:
        if installed is None:
            print(f"cross not found; installing pinned version {CROSS_VERSION}...")
        else:
            print(f"cross {installed} installed but {CROSS_VERSION} pinned; reinstalling...")
        # A version mismatch means cargo replaces any existing cross without needing --force.
        run(["cargo", "install", "cross", "--version", CROSS_VERSION, "--locked"])

    # Install the toolchain cross mounts into its build container. rustup >= 1.28 refuses to add a
    # non-host toolchain without --force-non-host, a flag cross 0.2.5 predates and can't pass itself.
    # Idempotent, so it runs whether or not cross itself was just installed.
    run(["rustup", "toolchain", "install", CROSS_HOST_TOOLCHAIN, "--force-non-host"])


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
    mac_arm, mac_intel = assets["darwin-arm64"], assets["darwin-x86_64"]
    linux_intel, linux_arm = assets["linux-x86_64"], assets["linux-aarch64"]
    base_url = f"https://github.com/{GITHUB_REPO}/releases/download/{tag}"

    print("\nRendering Homebrew formula...")
    formula = FORMULA_TEMPLATE.format(
        version=version,
        url_darwin_arm64=f"{base_url}/{mac_arm.name}",
        url_darwin_x86_64=f"{base_url}/{mac_intel.name}",
        url_linux_x86_64=f"{base_url}/{linux_intel.name}",
        url_linux_aarch64=f"{base_url}/{linux_arm.name}",
        sha256_darwin_arm64=sha256_of_file(mac_arm),
        sha256_darwin_x86_64=sha256_of_file(mac_intel),
        sha256_linux_x86_64=sha256_of_file(linux_intel),
        sha256_linux_aarch64=sha256_of_file(linux_arm),
        name_darwin_arm64=mac_arm.name,
        name_darwin_x86_64=mac_intel.name,
        name_linux_x86_64=linux_intel.name,
        name_linux_aarch64=linux_arm.name,
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

    # Build a release binary for each target (also refreshes Cargo.lock with the new version).
    # Name each asset with its platform suffix, since the binaries are architecture-specific.
    # macOS builds run natively via cargo; Linux builds cross-compile in Docker via `cross`.
    assets: dict[str, Path] = {}

    def build(triple: str, suffix: str, builder: list[str], add_target: bool,
              env: dict | None = None) -> None:
        if add_target:
            run(["rustup", "target", "add", triple])
        run([*builder, "build", "--release", "--target", triple], env=env)

        binary = REPO_ROOT / "target" / triple / "release" / BIN_NAME
        if not binary.is_file():
            fail(f"release binary not found at {binary}")

        asset = binary.with_name(f"{BIN_NAME}-{version}-{suffix}")
        shutil.copy2(binary, asset)
        assets[suffix] = asset

    for triple, suffix in MAC_TARGETS:
        build(triple, suffix, ["cargo"], add_target=True)
    # cross provisions the target toolchain inside its container, so no `rustup target add` needed.
    # Force amd64 for the cross build (scoped via env to just these calls). The pinned images are
    # amd64-only, so on Apple Silicon this makes Docker pull them and run under emulation instead of
    # failing to find a linux/arm64 variant. It also keeps the build environment (image + the
    # hardcoded x86_64 host toolchain) identical on every release machine, so output is reproducible.
    cross_env = {**os.environ, "DOCKER_DEFAULT_PLATFORM": "linux/amd64"}
    for triple, suffix in LINUX_TARGETS:
        build(triple, suffix, ["cross"], add_target=False, env=cross_env)

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
