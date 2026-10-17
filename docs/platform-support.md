# Platform support and usage

`uur` selects Linux integrations by observable capability. Distribution and
desktop names below help users install the right dependencies. `uur doctor`
reports the active login session.

The current release target is Linux x86_64. Wine must be able to run the
official 64-bit UU Remote client.

## Runtime dependency axes

| Capability | Preferred route | Alternatives |
| --- | --- | --- |
| Wayland video | ScreenCast portal + PipeWire | Portal virtual/window sources |
| X11 video | Wine X11 capture | ScreenCast portal when explicitly selected |
| Wayland input | RemoteDesktop portal | uinput; XTest for XWayland windows |
| X11 input | XTest | uinput |
| Audio | PulseAudio protocol | pipewire-pulse; ALSA |
| Wine windows | X11/XWayland | Wine Wayland driver |
| Application launch | `gio launch` | `gtk-launch` |
| Suspend inhibition | Inhibit portal | Session continues without an inhibitor |
| WOL persistence | NetworkManager + ethtool | ethtool runtime state |
| Hardware inventory | DRM + VA-API/Vulkan Video | NVENC/NVDEC; software codec |

Common portal packages include `xdg-desktop-portal-gnome`,
`xdg-desktop-portal-kde`, `xdg-desktop-portal-hyprland`,
`xdg-desktop-portal-wlr`, `xdg-desktop-portal-lxqt`,
`xdg-desktop-portal-xapp`, `xdg-desktop-portal-gtk`, and compositor-specific
implementations. The active interfaces determine the selected path.

## Install by distribution family

### Arch package family

This includes Arch Linux and distributions that consume Arch packages or the
AUR, such as EndeavourOS, CachyOS, Manjaro, Garuda Linux, Artix Linux, and
Arch-based development systems. Init-system choice does not affect uur; it
does not require a persistent systemd service.

From AUR:

```bash
paru -S uur
```

From a downloaded Arch package:

```bash
sudo pacman -U ./uur-0.1.6-1-x86_64.pkg.tar.zst
```

Install the portal backend supplied or recommended by the selected desktop.
On a capture-only Wayland portal, ensure the active user can open
`/dev/uinput`; the package installs a udev rule and a module-load entry.

### Debian package family

The deb artifact is intended for current Debian, Ubuntu, Linux Mint, Pop!_OS,
Zorin OS, elementary OS, KDE neon, TUXEDO OS, Kali Linux, MX Linux, and other
compatible derivatives:

```bash
sudo apt install ./uur_0.1.6_amd64.deb
```

The package manager resolves Wine, PipeWire, and the portal frontend. Install
the session's portal backend separately when the distribution does not pull it
in with the desktop environment.

### RPM package family

The rpm artifact targets current Fedora and distributions with compatible RPM
dependency names. Install it with the native frontend:

```bash
# Fedora and compatible systems
sudo dnf install ./uur-0.1.6-1.x86_64.rpm

# openSUSE family
sudo zypper install ./uur-0.1.6-1.x86_64.rpm
```

RHEL, CentOS Stream, Rocky Linux, AlmaLinux, Oracle Linux, Nobara, Ultramarine,
openSUSE Tumbleweed, and openSUSE Leap can use the RPM when its dependency names
resolve, with the portable or source build covering other package layouts.

### Other conventional distributions

Gentoo, Funtoo, Void Linux, Slackware, Solus, Clear Linux, Mageia, OpenMandriva,
PCLinuxOS, and similar mutable/FHS systems can use the portable archive when
they provide compatible glibc, Wine, PipeWire, and portal libraries:

```bash
sudo tar --zstd -C / -xf ./uur-0.1.6-linux-x86_64.tar.zst
```

Alternatively, build and install for one user:

```bash
cargo build --release --locked
./hook/build.sh
./capture/build.sh
./scripts/install-user.sh
```

The source build needs Rust, a native C compiler, PipeWire development files,
`pkg-config`, and the MinGW-w64 x86_64 compiler and binutils. Package names are
distribution-specific; uur does not invoke a package manager itself.

### Immutable, declarative, and musl systems

NixOS uses the repository flake and NixOS module described in [Nix and
NixOS](nix.md). The module installs the package, udev integration, and `uinput`
kernel module declaratively.

Alpine edge uses `packaging/alpine/APKBUILD` or the included musl container
build. GNU Guix can package the same staged layout. Flatpak and Snap require
host input, Wine, and portal permissions that are best expressed as dedicated
manifests.

## Select the desktop integration path

### X11 session

Requirements: `DISPLAY`, Wine's X11 driver, and the XTest extension.

This path applies to full desktops and standalone window managers alike. Common
examples include Xorg sessions of GNOME, Plasma, Cinnamon, MATE, Xfce, LXQt,
LXDE, Budgie, Deepin, Pantheon, Trinity, and UKUI, plus i3, bspwm, awesome,
Openbox, Fluxbox, IceWM, dwm, qtile, herbstluftwm, spectrwm, and FVWM.

Wine captures the X11 desktop directly and uur injects input with XTest. A
ScreenCast portal is not required for this path.

### Wayland session with ScreenCast and RemoteDesktop portals

Requirements: a working session D-Bus, PipeWire, the xdg-desktop-portal
frontend, and a backend exposing both `ScreenCast` and `RemoteDesktop`.

Where those interfaces are available, uur creates one combined session for
capture and input. This is the preferred unprivileged route. Examples can
include current GNOME Shell, KDE Plasma, COSMIC, and other sessions whose selected portal backend
advertises both interfaces. Interface availability—not the desktop name—makes
the final decision.

### Wayland session with a capture-only portal

Requirements: ScreenCast/PipeWire plus writable `/dev/uinput` for full desktop
input. XTest remains only a partial fallback for XWayland surfaces.

This route commonly applies to compositors using Hyprland or wlroots-oriented
portal backends, including Hyprland, Sway, Wayfire, river, labwc, dwl, Waybox,
and other wlroots-based sessions. niri uses its recommended
`xdg-desktop-portal-gnome` ScreenCast integration and requires uinput when a
working RemoteDesktop session is unavailable. It may also apply to a desktop whose portal
installation is incomplete or whose selected backend omits RemoteDesktop.

Install the compositor's ScreenCast portal and enable uinput access. A
`portals.conf` selects the backend when several are installed.

### Wine window backend

The UU user interface still needs a Wine display driver:

- Existing X11 and most Wayland desktop sessions normally use Wine through
  X11 or XWayland.
- Compositors without built-in XWayland can use an external XWayland manager;
  niri, for example, integrates xwayland-satellite in current releases.
- Wine builds with a usable native Wayland driver may run without XWayland,
  but that combination must still pass UU WebView2 and window-management tests.

This choice is independent from desktop capture. A client window can use
XWayland while the controlled desktop is captured through the Wayland portal.

### Audio servers

Wine's PulseAudio driver is selected for both a native PulseAudio daemon and
the `pipewire-pulse` compatibility server. A native PipeWire session without
PulseAudio compatibility uses Wine's ALSA driver. `uur doctor` prints the live
choice; this is independent from PipeWire's Wayland video role.

### Displays and virtual sources

```bash
uur display status
uur display source monitor
uur display source window
uur display source virtual
```

The command clears the old restore token so the next Portal session can create
or select the requested source. X11 exposes its XRandR topology to Wine.
Wayland currently carries one selected stream; independent multi-stream
switching is the next display-topology adapter.

## First run and normal use

After installing any package format:

```bash
uur doctor
uur setup --accept-eula
uur run
```

Optional host integration is managed explicitly:

```bash
uur autostart enable
uur autostart status
uur wol status
sudo uur wol enable --interface enp3s0
```

The WOL command configures the real Linux adapter; it requires `ethtool`, and
persists the setting through an active NetworkManager wired profile when one
exists. The adapter name above is only an example—omit it for automatic
physical-interface selection.

The package installs `uur.desktop` as the canonical application-menu entry.
During setup, an existing Wine-generated Linux desktop entry for the official
client is converted into a hidden compatibility alias whose command is also
`uur run`. This keeps already pinned Dock, panel, or launcher favorites working
without showing a duplicate application in new searches.

The `.lnk` file inside the Wine prefix remains available for diagnosis. Normal
desktop integration launches `uur.desktop` or `uur run` so all helpers share
the same lifetime.

The first Wayland run may show a portal chooser. Select the monitor to expose
to remote controllers. A successful persistent portal grant is reused where
the backend supports restore tokens.

UU Quick Launch is populated from visible XDG desktop entries every time
`uur run` starts. This includes conventional packages and any containerized or
portable application that publishes a valid `.desktop` entry. Application
activation uses the desktop entry itself, so uur does not maintain a command
translation table.

Stop only the managed session and its Wine prefix with:

```bash
uur stop
```

The session tracks `GameViewer.exe` in its own Wine prefix through the
resident `winlogon.exe` adapter. This uses the Windows process table even when
the client changes its Linux process title. It keeps a three-second handoff
grace period without repeatedly starting `tasklist`. A failed process query
is not treated as a client exit; a disconnected or unresponsive monitor ends
the session with an explicit error. Stopping the session also stops this
adapter and the capture/input helpers. Remaining Wine workers, including
WebView2 processes that outlive their wineserver, are matched to this prefix
and signaled through Linux pidfds to avoid PID-reuse races. Other Wine prefixes
are left running. This residual-process cleanup needs Linux 5.3 or later;
older kernels still use Wine's normal wineserver shutdown.

Useful input-backend overrides for diagnosis are:

```bash
UUR_INPUT_BACKEND=portal uur run
UUR_INPUT_BACKEND=uinput uur run
UUR_INPUT_BACKEND=xtest uur run
```

An override is a diagnostic tool, not a desktop-specific configuration. The
default `auto` mode should be used after troubleshooting.

## Interrupted setup and Wine components

Retry an interrupted installation without deleting the managed prefix:

```bash
uur stop
uur setup --repair
uur doctor
uur run
```

The repair option runs `wineboot --update` for the managed prefix. This retries
Wine's component initialization, including its Wine Mono offer where needed.
It keeps the installed client's data, checks both the client and service
executables and the Windows service registration, and reruns the official
installer if that installation is incomplete. Failed initialization or a
failed/cancelled installer returns an error instead of reporting completion.

The **Wine Mono Installer** is Wine's .NET runtime installer. It is separate
from **WebView2**, which uur uses for the official client's management UI.
Cancelling Mono does not establish the cause of a Wayland black screen or
failed desktop input. Those paths depend on the native capture helper,
ScreenCast/PipeWire and an authorized RemoteDesktop session or uinput.
On Arch/CachyOS, installing the matching `wine-mono` package can avoid the
Wine dialog's network download; use the package supplied for the installed
Wine version. For the existing 0.1.4 release, the equivalent manual retry is:

```bash
uur stop
WINEPREFIX="${XDG_DATA_HOME:-$HOME/.local/share}/uur/wine" \
  WINEDLLOVERRIDES="wevtapi=b,wtsapi32=b" wineboot --update
uur setup
```

To open the Wine program/component manager for this prefix:

```bash
WINEPREFIX="${XDG_DATA_HOME:-$HOME/.local/share}/uur/wine" \
  WINEDLLOVERRIDES="wevtapi=b,wtsapi32=b" wine uninstaller
```

## niri: black video or missing desktop input

Use the niri session entry (or `niri-session`), its recommended
`xdg-desktop-portal-gnome` backend, and a running PipeWire service. A visible
Wine window only verifies its XWayland display path. Remote video still needs
the separate ScreenCast stream, and native Wayland input needs a working
RemoteDesktop session or writable `/dev/uinput`.

Start with an actual monitor selection:

```bash
uur display source monitor
uur doctor
test -w /dev/uinput && echo 'uinput is writable'
uur run
```

If the portal chooser offers **niri Dynamic Cast Target**, that stream remains
empty until a target is assigned through niri. Select a real monitor when
diagnosing black video. See [niri's screencasting documentation](https://github.com/niri-wm/niri/wiki/Screencasting).
If RemoteDesktop creation fails, the automatic policy tries uinput. When both
are unavailable it reports an error; explicitly selected XTEST can only reach
XWayland windows. The packaged udev rule and uinput module must be active for
the logged-in user; restart the graphical session after changing that setup.

`uur run` verifies a real Wayland frame and a running GameViewerService before
announcing readiness. Startup/helper failures are reported in the terminal;
details are in `${XDG_STATE_HOME:-$HOME/.local/state}/uur/session.log` and
`${XDG_DATA_HOME:-$HOME/.local/share}/uur/hook.log`. A missing tray icon alone
does not determine whether capture or input works.

For niri and other compositors exposing `wlr-screencopy`, uur also offers
direct, shared-memory monitor capture. This path avoids portal/PipeWire/GBM
dependencies for video and is useful when a portal cannot produce frames:

```bash
uur display outputs
uur display backend wayland --output DP-1  # Use the name printed above
uur run
```

Selecting `wayland` explicitly authorizes direct monitor capture for later
runs. It does not show a system screen-sharing dialog and does not activate
automatically after portal cancellation or failure. With one output, `--output`
can be omitted; multiple outputs require an explicit name. Automatic input
selection uses uinput in this mode. Run `uur display backend portal` to return
to portal-consented capture. Direct mode captures a monitor, not individual
windows or virtual portal sources; changes to its size require restarting
the session.

The supervised native StatusNotifierItem supplies its own icon pixels and
DBusMenu to Noctalia, Plasma and other compatible panels. Right-click it for
Open UU Remote and Quit. It re-registers when the panel's watcher restarts;
it does not depend on Wine's legacy tray being visible.

## Reading `uur doctor`

- `portal ScreenCast`: required to capture a Wayland desktop.
- `portal RemoteDesktop`: preferred unprivileged Wayland input route.
- `uinput (/dev/uinput)`: full-desktop fallback when RemoteDesktop is absent.
- `DISPLAY`: required for Wine/X11 or Wine/XWayland and for XTest fallback.
- `WAYLAND_DISPLAY`: identifies the active Wayland session.
- `PipeWire`: required for the Wayland frame transport.

If both Wayland input routes are unavailable, XTest controls XWayland windows.

## Session model

Portal sessions operate in the logged-in desktop seat. Display-manager access
uses a display-manager remote session, while headless operation uses a portal
virtual source or a provisioned virtual display. `uur doctor` reports the
active backend capabilities.

Relevant upstream documentation:

- [xdg-desktop-portal backend selection](https://flatpak.github.io/xdg-desktop-portal/docs/portals.conf.html)
- [RemoteDesktop portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html)
- [ScreenCast portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)
