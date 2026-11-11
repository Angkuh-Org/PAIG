# Install

Release binaries are built on Ubuntu 22.04 for Linux `x86_64` (`x86-64-v2`
baseline) and `aarch64` (`armv8-a` baseline). Both require glibc 2.35 or newer.

## One-liner

```sh
curl -fsSL https://raw.githubusercontent.com/Angkuh-Org/PAIG/main/install.sh | sh
```

Expected output (version depends on the latest release):

```text
downloading paig-linux-x86_64 (latest)
paig-linux-x86_64: OK
installed paig v0.1.0 -> /home/you/.local/bin/paig
```

The installer verifies the release asset against its published SHA-256 and
installs to `~/.local/bin` (or `/usr/local/bin` when run as root). If that
directory is not on `PATH`, it prints the `export` line to add; `paig` is not
resolvable until you run it (or start a new login shell on distros that add
`~/.local/bin` to `PATH`).

## Options

Pin a version:

```sh
curl -fsSL https://raw.githubusercontent.com/Angkuh-Org/PAIG/main/install.sh | sh -s -- v0.1.0
```

Choose the install directory:

```sh
curl -fsSL https://raw.githubusercontent.com/Angkuh-Org/PAIG/main/install.sh \
  | PAIG_INSTALL_DIR=/opt/paig/bin sh
```

`PAIG_VERSION` is the environment-variable form of the version argument, and
`PAIG_REPO` overrides the repository (for forks). `PAIG_DOWNLOAD_BASE`
replaces the GitHub release URL for mirrors; `http://` is accepted only
through that override.

## Manual install

```sh
version=v0.1.0
arch=x86_64   # or aarch64
base=https://github.com/Angkuh-Org/PAIG/releases/download/$version
curl -fLO "$base/paig-linux-$arch"
curl -fLO "$base/paig-linux-$arch.sha256"
sha256sum -c "paig-linux-$arch.sha256"
install -m 0755 "paig-linux-$arch" ~/.local/bin/paig
```

## Verify provenance

Every release binary has a GitHub build-provenance attestation. After
downloading it:

```sh
gh attestation verify paig-linux-x86_64 --repo Angkuh-Org/PAIG
```

This needs `gh` 2.49 or newer; older releases do not include the `attestation`
command.

## Uninstall

```sh
rm ~/.local/bin/paig
```

To build from source instead, see [build.md](build.md).
