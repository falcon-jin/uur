<p align="center">
  <img src="packaging/uur-icon.png" width="112" alt="UU Remote icon">
</p>

<h1 align="center">uur</h1>

<p align="center">
  A native Linux compatibility layer for the official NetEase UU Remote client.
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Linux-x86__64-FCC624?style=flat-square&logo=linux&logoColor=black" alt="Linux x86_64">
  <img src="https://img.shields.io/badge/Desktop-X11%20%7C%20Wayland-4A90E2?style=flat-square" alt="X11 and Wayland">
  <img src="https://img.shields.io/badge/Language-Rust-DEA584?style=flat-square&logo=rust&logoColor=black" alt="Rust">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue?style=flat-square" alt="MIT license"></a>
</p>

<p align="center">
  <strong>English</strong> · <a href="README.zh-CN.md">简体中文</a>
</p>

> [!IMPORTANT]
> `uur` does not redistribute UU Remote. It downloads the official Windows
> installer from NetEase after the user explicitly accepts the upstream EULA.

## ✨ Highlights

| Capability | Linux integration |
| --- | --- |
| Control other computers | Official UU interface and media stack under Wine |
| Allow this Linux computer to be controlled | ScreenCast portal/PipeWire capture and native Linux input |
| X11 and Wayland | Capability detection instead of distribution or desktop allowlists |
| Clean lifecycle | Bridges start with UU and stop with its managed Wine prefix |
| Native tools | Linux PTY terminal, wallpaper, Quick Launch, proxy, and sleep inhibition |
| Upstream resilience | API-level runtime adapters; no version/RVA patch matrix |
| Distribution | Arch/pacman, deb, rpm, portable tar.zst, and an AUR recipe |

## 🚀 First run

After installing any package, run these commands **in this order**:

```bash
# 1. Inspect the current Wine, portal, PipeWire, and input capabilities.
uur doctor

# 2. Accept NetEase's EULA, download the official client, and create the prefix.
uur setup --accept-eula

# 3. Start capture, input, the managed Wine services, and the UU interface.
uur run
```

`uur setup` without `--accept-eula` intentionally stops after printing the
official EULA URL. It does not install the proprietary client.

Many capabilities remain untested; runtime testing so far covers Arch Linux, KDE Plasma, and Wayland.

After setup, UU Remote is also available from the application menu. The menu
entry launches the complete `uur run` session.

## 📦 Installation

Download the newest package for the system from the
[GitHub Releases page](https://github.com/panxuc/uur/releases/latest). Native
recipes and release artifacts share one version and filesystem layout.

### Arch Linux

Install a downloaded release package with pacman:

```bash
sudo pacman -U ./dist/uur-0.1.6-1-x86_64.pkg.tar.zst
```

### AUR

After the package is available from AUR:

```bash
paru -S uur
```

### Debian and Ubuntu package family

```bash
sudo apt install ./uur_0.1.6_amd64.deb
```

### Fedora and compatible RPM systems

```bash
sudo dnf install ./uur-0.1.6-1.x86_64.rpm
```

On systems with different RPM dependency names, install the dependencies
manually or use the portable archive.

### Portable archive

```bash
sudo tar --zstd -C / -xf ./uur-0.1.6-linux-x86_64.tar.zst
```

### Nix and NixOS

```bash
nix build
nix run . -- doctor
```

The flake also exports `nixosModules.default`; see [Nix and NixOS](docs/nix.md).

### Alpine Linux

Alpine edge can build the native musl package layout with:

```bash
docker build -f packaging/alpine/Dockerfile .
```

An `APKBUILD` is included under `packaging/alpine/`.

### User installation from source

```bash
cargo build --release --locked
./hook/build.sh
./capture/build.sh
./scripts/install-user.sh
```

The source build requires Rust, a C compiler, PipeWire development files,
`pkg-config`, and the MinGW-w64 x86_64 toolchain.

Gentoo, Void Linux, Slackware, Solus, ALT Linux, Exherbo, Clear Linux, Mageia,
OpenMandriva, PCLinuxOS, Venom Linux, Guix System, and other package ecosystems
can use the portable archive or the shared `packaging/stage.sh` source layout.

See [Platform support and usage](docs/platform-support.md) for distribution
families, portal choices, X11/Wayland sessions, standalone window managers,
immutable systems, and libc boundaries.

## 🧭 Daily use

```bash
uur run                 # Start or show the managed UU session
uur doctor              # Read-only environment diagnostics
uur upstream check      # Check the official NetEase release feed
uur autostart enable    # Optional: start after graphical login
uur autostart disable   # Remove the managed login entry
uur wol status          # Inspect the physical wired adapter's WOL state
uur display status      # Monitor, window, or portal virtual source
uur display source virtual
uur display outputs     # Native Wayland output names
uur stop                # Stop only uur's helpers and Wine prefix
uur uninstall --purge --dry-run  # Preview complete user-data cleanup
uur uninstall --purge   # Confirm cleanup before removing the system package
```

If UU was started from the desktop, choose **Quit** from the launcher's
application actions. When running from a terminal, use `uur stop` or press
Ctrl+C.

On niri, uur provides a native tray and an explicitly enabled direct capture
path: `uur display backend wayland --output DP-1` (use your output name).
This uses uinput and captures the chosen monitor without a portal dialog.
Return to system-consented capture with `uur display backend portal`.

Package removal alone preserves the Wine prefix and account data. See
[complete removal](docs/distribution.md#package-removal-repair-and-per-user-state)
for the user-cleanup and package-manager steps.

Diagnostic input overrides:

```bash
UUR_INPUT_BACKEND=portal uur run
UUR_INPUT_BACKEND=uinput uur run
UUR_INPUT_BACKEND=xtest uur run
```

Use the default `auto` mode after troubleshooting.

## 🖥️ Desktop integration

- A Wayland session with both RemoteDesktop and ScreenCast portals uses one
  combined, unprivileged session for input and video.
- A Wayland session with only ScreenCast uses PipeWire for video and uinput
  for full-desktop input.
- An X11 session uses Wine's native X11 capture and XTest input.
- XTest under XWayland is only a partial fallback and is not advertised as
  full Wayland desktop control.

Run `uur doctor` instead of inferring support from a desktop or distribution
name.

## 🧩 Native Linux features

Screen capture, keyboard, pointer, lifecycle management, wallpaper metadata,
native terminal, Quick Launch, proxy synchronization, suspend inhibition, and
host WOL configuration are implemented. Quick Launch exposes the XDG
application catalog through UU and resolves each request back to `gio launch`;
it never accepts an arbitrary command from Wine. Other UU features require
native Linux service adapters rather than being left inside the Wine prefix:

| Feature | Direction |
| --- | --- |
| Terminal | Authenticated UU transport → native PTY and the user's login shell |
| Quick Launch | UU application inventory → authenticated XDG desktop activation |
| Wake-on-LAN setup | `uur wol` → real wired interface and NetworkManager profile |
| Received files | Validated staging → the user's XDG Downloads directory |
| Sending files | File portal or explicit CLI selection → bounded native staging |
| Port mapping | Policy-controlled native TCP/UDP broker; loopback by default |
| Clipboard and phone text | RemoteDesktop Clipboard portal with X11 fallback |

The protocol, security boundaries, lifecycle, and implementation phases are in
[Native service adapters](docs/native-services.md).

## 🔬 How it works

```text
Official UU Remote client (managed Wine prefix)
        │
        ├─ Windows API adapter ── authenticated input/control protocol
        │
        └─ GDI/DXGI capture boundary ◄── private triple-buffer frame transport
                                              ▲
Linux supervisor ── ScreenCast portal ── PipeWire
        │
        └─ RemoteDesktop portal / uinput / XTest
```

The Windows ABI adapter is small C code. Session policy, desktop integration,
capability selection, and lifecycle management are implemented in Rust.

## 🛠️ Troubleshooting

Start with:

```bash
uur doctor
```

Session logs are stored at:

```text
${XDG_STATE_HOME:-$HOME/.local/state}/uur/session.log
```

For Wine diagnostics only:

```bash
WINEDEBUG=+loaddll uur run
```

Normal runs suppress Wine's non-actionable `fixme` output.

## 📚 Documentation

- [Architecture](docs/architecture.md)
- [Platform support and usage](docs/platform-support.md)
- [Capture protocol](docs/capture-protocol.md)
- [Native service adapters](docs/native-services.md)
- [Feature parity](docs/feature-parity.md)
- [Upstream adaptation](docs/upstream-adaptation.md)
- [Package installation](docs/distribution.md)
- [Nix and NixOS](docs/nix.md)
- [Windows feature map](docs/windows-feature-map.md)
- [Video and display backends](docs/video-backends.md)

## 🙏 Credits

The project learned from
[GuoWQ222/uu-remote-for-linux](https://github.com/GuoWQ222/uu-remote-for-linux),
[lachlanchen/uu-remote-ubuntu-bridge](https://github.com/lachlanchen/uu-remote-ubuntu-bridge).

NetEase UU Remote is proprietary software of NetEase. `uur` is an unofficial
compatibility project and is not affiliated with NetEase.
