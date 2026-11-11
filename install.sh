#!/bin/sh
# PAIG installer: download a release binary, verify its SHA-256, install it.
set -eu

repo=${PAIG_REPO:-Angkuh-Org/PAIG}
version=${1:-${PAIG_VERSION:-latest}}

if [ "$(uname -s)" != Linux ]; then
    echo "paig: only Linux binaries are published" >&2
    exit 1
fi

case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *)
        echo "paig: unsupported architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

asset=paig-linux-$arch
if [ -n "${PAIG_DOWNLOAD_BASE:-}" ]; then
    base=$PAIG_DOWNLOAD_BASE
elif [ "$version" = latest ]; then
    base=https://github.com/$repo/releases/latest/download
else
    base=https://github.com/$repo/releases/download/$version
fi
case "$base" in
    http://*) proto='=https,http' ;;
    *) proto='=https' ;;
esac

if [ -n "${PAIG_INSTALL_DIR:-}" ]; then
    dest=$PAIG_INSTALL_DIR
elif [ "$(id -u)" -eq 0 ]; then
    dest=/usr/local/bin
else
    dest=$HOME/.local/bin
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

printf 'downloading %s (%s)\n' "$asset" "$version"
curl --proto "$proto" --tlsv1.2 -fsSL "$base/$asset" -o "$tmp/$asset"
curl --proto "$proto" --tlsv1.2 -fsSL "$base/$asset.sha256" -o "$tmp/$asset.sha256"
(cd "$tmp" && sha256sum -c "$asset.sha256")

mkdir -p "$dest"
install -m 0755 "$tmp/$asset" "$dest/paig"

if installed=$("$dest/paig" --version 2>/dev/null); then
    printf 'installed %s -> %s/paig\n' "$installed" "$dest"
else
    printf 'installed %s/paig (version check failed; glibc >= 2.35 is required)\n' "$dest" >&2
fi

case ":${PATH:-}:" in
    *":$dest:"*) ;;
    *) printf 'add it to PATH:\n  export PATH="%s:$PATH"\n' "$dest" ;;
esac
