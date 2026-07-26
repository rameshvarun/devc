#!/bin/sh
# Install the latest devc release into ~/.local/bin.
#
#   curl -fsSL https://raw.githubusercontent.com/rameshvarun/devc/main/install.sh | sh
#
# Downloads the release asset matching this machine's OS/architecture (as named by
# scripts/release.py: devc-<version>-<os>-<arch>) and drops the `devc` binary into ~/.local/bin.
set -eu

REPO="rameshvarun/devc"
BIN_NAME="devc"
INSTALL_DIR="${HOME}/.local/bin"

# Match the naming scheme release.py uses: platform.system()/machine(), lowercased.
os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m | tr '[:upper:]' '[:lower:]')

echo "Looking for the latest ${REPO} release for ${os}-${arch}..."

# Pull the latest release metadata and pick the asset whose name ends in -<os>-<arch>.
api="https://api.github.com/repos/${REPO}/releases/latest"
url=$(curl -fsSL "$api" \
  | grep -o '"browser_download_url": *"[^"]*"' \
  | sed 's/.*"browser_download_url": *"\([^"]*\)".*/\1/' \
  | grep -- "-${os}-${arch}$" \
  | head -n1)

if [ -z "$url" ]; then
  echo "error: no release asset found for ${os}-${arch} in ${REPO}" >&2
  echo "       see https://github.com/${REPO}/releases for available downloads." >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
dest="${INSTALL_DIR}/${BIN_NAME}"
tmp="${dest}.download"

echo "Downloading ${url}"
curl -fsSL "$url" -o "$tmp"
chmod +x "$tmp"
mv "$tmp" "$dest"

echo "Installed ${BIN_NAME} to ${dest}"

# Nudge the user if the install dir isn't on PATH, so `devc` actually resolves.
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "note: ${INSTALL_DIR} is not on your PATH; add it, e.g.:"
     echo "      export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac
