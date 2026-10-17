# Package installation

Choose the package format native to the system. Every format installs the same
Rust supervisor, Wine adapters, PipeWire helper, desktop entry, icon, and
uinput integration files. `sudo`, `doas`, or another privilege frontend may be
needed for system-wide package installation.

## Arch Linux

```bash
# AUR
paru -S uur

# Downloaded release package
sudo pacman -U ./uur-0.1.6-1-x86_64.pkg.tar.zst
```

## Debian, Ubuntu, and derivatives

```bash
sudo apt install ./uur_0.1.6_amd64.deb
```

The same release artifact covers Debian, Ubuntu, Linux Mint, Pop!_OS, KDE neon,
Zorin OS, elementary OS, Kali Linux, MX Linux, TUXEDO OS, and compatible
derivatives.

## Fedora, RHEL, and openSUSE families

```bash
sudo dnf install ./uur-0.1.6-1.x86_64.rpm

# openSUSE
sudo zypper install ./uur-0.1.6-1.x86_64.rpm
```

This format also covers Fedora derivatives and RPM systems whose repositories
provide the declared Wine, GLib, PipeWire, and portal dependencies.

## Portable archive

The portable archive targets conventional glibc/FHS systems:

```bash
sudo tar --zstd -C / -xf ./uur-0.1.6-linux-x86_64.tar.zst
```

## Nix and NixOS

```bash
nix build
nix run . -- doctor
```

The flake includes a package, application, development shell, lock file, and
NixOS module. See [Nix and NixOS](nix.md).

## Alpine Linux and musl

`packaging/alpine/APKBUILD` builds an x86_64 musl package from a tagged source
archive. The container recipe runs the same Rust, MinGW, and PipeWire builds:

```bash
docker build -f packaging/alpine/Dockerfile .
```

Alpine edge provides native x86_64 Wine, PulseAudio/ALSA Wine drivers,
PipeWire, desktop portals, Rust, and the MinGW-w64 toolchain.

```bash
cd packaging/alpine
abuild checksum
abuild -r
sudo apk add ~/packages/*/x86_64/uur-*.apk
```

## Other package ecosystems

Gentoo, Funtoo, Void Linux, Slackware, Solus, Clear Linux, ALT Linux, Exherbo,
Mageia, OpenMandriva, PCLinuxOS, Venom Linux, and other FHS/glibc systems can
install the portable release archive or build from source. Their native recipe
can call `packaging/stage.sh` after the three standard builds; the staged tree
is shared with pacman, deb, rpm, APK, and the portable archive.

GNU Guix can wrap the same source build and staged tree. Flatpak and Snap need
manifests granting Wine execution, session D-Bus, PipeWire, and input access.

## Source installation for one user

```bash
cargo build --release --locked
./hook/build.sh
./capture/build.sh
./scripts/install-user.sh
```

This installs under `~/.local` by default. It requires Rust, a native C
compiler, PipeWire and Wayland development files (including `wayland-scanner`), `pkg-config`, and the MinGW-w64 x86_64
toolchain.

## Required first run

Package installation does not implicitly accept NetEase's EULA or download the
official client. Complete the setup explicitly:

```bash
uur doctor
uur setup --accept-eula
uur run
```

See [Platform support and usage](platform-support.md) for desktop-session and
portal requirements.

## Package removal, repair, and per-user state

Package removal leaves the managed Wine prefix, downloaded installer cache,
configuration and logs in the user's XDG directories. This preserves login
and client data across package upgrades and reinstalls. Reinstalling the
package therefore does not create a new Wine prefix. Start with
`uur stop` followed by `uur setup --repair` for an interrupted setup.

For a fresh prefix, stop uur and move the old prefix aside before setup:

```bash
uur stop
uur_prefix="${XDG_DATA_HOME:-$HOME/.local/share}/uur/wine"
mv -- "$uur_prefix" "$uur_prefix.backup-$(date +%Y%m%d-%H%M%S)"
uur setup --accept-eula
```

The fresh prefix needs the official client installed and logged in again.
The backup retains the previous client and account data.

`uur uninstall` stops the managed session and Wine server, removes this user's
autostart entry and recognized UU launchers/icons, and removes a source
installation identified by its installer receipt. It keeps the managed client,
account data, configuration and logs by default. Use `--purge` for a fresh
start or complete removal:

```bash
# Preview only: no processes stopped, no files changed
uur uninstall --purge --dry-run

# Run as the desktop user, NOT with sudo; review the paths and confirm
uur uninstall --purge

# Remove system package files afterwards (Arch Linux)
sudo pacman -Rns uur
```

Purge permanently deletes the dedicated Wine prefix (including Wine Mono,
WebView2 and the official client), login/account data, installer cache, all uur
configuration and logs, and private runtime files. Back up the `uur` XDG
directories first if those data are needed. For unattended removal, explicitly
confirm with `uur uninstall --purge --yes`. The command honors the current
`XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `XDG_RUNTIME_DIR`; use
the same values used for setup. A custom source install can be selected with
`UUR_INSTALL_PREFIX=/path/to/prefix uur uninstall --purge`.

On Debian/Ubuntu, use `sudo apt purge uur` after the user cleanup; on Fedora,
use `sudo dnf remove uur`. Package-manager removal alone leaves per-user data
intact, including when run with `purge`: package scripts run as root and do not
erase other users' home directories. Each user should run the cleanup command
before the system package is removed. If the package was already removed,
reinstall it to get the cleanup command, run `uur uninstall --purge`, then
remove the package again; running setup is not required. Shared Wine/Mono, PipeWire and portal
packages remain the package manager's responsibility. Kernel/device settings
and compositor-granted portal permissions are not reset by user-data cleanup.
Unpin any old launcher from the desktop panel afterwards if the panel retains
its own pinned copy.
