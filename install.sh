#!/bin/sh
# glm installer: fetches the latest release from GitHub, verifies the sha256,
# and installs to ~/.local/bin/glm (or $GLM_INSTALL_DIR).
set -eu

REPO="r0bops/glm-wrapper"
VERSION="${GLM_VERSION:-latest}"
INSTALL_DIR="${GLM_INSTALL_DIR:-$HOME/.local/bin}"

os="$(uname -s)"
case "$os" in
    Linux)  os_name="linux" ;;
    Darwin) os_name="darwin" ;;
    *) echo "glm: unsupported OS: $os" >&2; exit 1 ;;
esac

arch="$(uname -m)"
case "$arch" in
    x86_64 | amd64) arch_name="x86_64" ;;
    arm64 | aarch64) arch_name="aarch64" ;;
    *) echo "glm: unsupported architecture: $arch" >&2; exit 1 ;;
esac

asset="glm-${os_name}-${arch_name}.tar.gz"
echo "glm: fetching $REPO release ${VERSION} ($asset)"

# resolve the download URL from the GitHub API
if [ "$VERSION" = "latest" ]; then
    api_url="https://api.github.com/repos/$REPO/releases/latest"
else
    api_url="https://api.github.com/repos/$REPO/releases/tags/$VERSION"
fi
json="$(curl -fsSL -H "Accept: application/vnd.github+json" "$api_url")"
tag="$(printf '%s' "$json" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"
dl_url="$(printf '%s' "$json" | sed -n "s/.*\"browser_download_url\": *\"\([^\"]*${asset}\)\".*/\1/p" | head -n1)"
sums_url="$(printf '%s' "$json" | sed -n "s/.*\"browser_download_url\": *\"\([^\"]*SHA256SUMS\)\".*/\1/p" | head -n1)"
if [ -z "${dl_url:-}" ] || [ -z "${sums_url:-}" ]; then
    echo "glm: release ${VERSION:-latest} has no $asset or SHA256SUMS asset" >&2
    exit 1
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

echo "glm: downloading ${tag:-$VERSION}..."
curl -fsSL "$dl_url" -o "$tmpdir/$asset"
curl -fsSL "$sums_url" -o "$tmpdir/SHA256SUMS"

want="$(awk -v a="$asset" '$2 == a {print $1}' "$tmpdir/SHA256SUMS")"
if [ -z "${want:-}" ]; then
    echo "glm: SHA256SUMS has no entry for $asset" >&2
    exit 1
fi
echo "glm: verifying sha256..."
if command -v sha256sum >/dev/null 2>&1; then
    got="$(sha256sum "$tmpdir/$asset" | awk '{print $1}')"
else
    got="$(shasum -a 256 "$tmpdir/$asset" | awk '{print $1}')"
fi
if [ "$got" != "$want" ]; then
    echo "glm: sha256 mismatch for $asset (expected $want, got $got)" >&2
    exit 1
fi

mkdir -p "$INSTALL_DIR"
tar -xzf "$tmpdir/$asset" -C "$tmpdir"
find "$tmpdir" -name glm -type f -exec install -m 0755 {} "$INSTALL_DIR/glm" \;

echo "glm: installed $INSTALL_DIR/glm (${tag:-$VERSION})"
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo
        echo "glm: $INSTALL_DIR is not on your PATH. Add it with:"
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac
echo
echo "glm: next step: run  glm init  to create ~/.config/glm/config.toml and store your Z.ai API key"
