# Feature parity

| Capability | Status | Linux route |
| --- | --- | --- |
| Control other devices | available | Official UU UI and decoder under Wine |
| Receive remote video | available | Official UU UI under Wine |
| Share X11 desktop | available | Wine X11 capture |
| Share Wayland desktop | available | ScreenCast/PipeWire or explicitly enabled native screencopy → GDI adapter |
| Mouse and keyboard | implemented | RemoteDesktop portal → uinput → XTest capability order |
| X11 multi-monitor | Wine integration | Native Wine/XRandR display topology |
| Wayland monitor/window selection | available | Persistent portal source selection |
| Wayland virtual source | available when advertised | `uur display source virtual` → ScreenCast Virtual source |
| Wayland multi-screen capture | available | Multiple PipeWire streams → virtual desktop composite → per-monitor crops |
| Clipboard and phone IME text | planned | RemoteDesktop Clipboard portal, then X11 fallback |
| Speaker and microphone audio | available through Wine | PulseAudio, pipewire-pulse, or ALSA selected from live capabilities |
| System-audio loopback and remote mute | Wine integration | WASAPI → winepulse/winealsa |
| Native tray | implemented | StatusNotifierItem + DBusMenu, supervised with the session; no Wine XEmbed dependency |
| Terminal | available | ConPTY/mux ABI adapters → authenticated native Rust PTY/login shell |
| Quick Launch | implemented | Windows app inventory adapter → XDG desktop catalog → `gio launch` |
| Desktop wallpaper | available | Capability providers → Wine desktop metadata |
| Prevent suspend/idle | available | Inhibit portal, scoped to `uur run` |
| System proxy | available | Standard proxy environment → Wine WinINet settings |
| Login autostart | available | User-controlled XDG autostart entry |
| Wake-on-LAN host setup | available | Physical-interface discovery → ethtool/NetworkManager configuration |
| Wake button in UU cloud UI | planned | Stable server capability API integration |
| VA-API capability | detected | DRM render nodes + `vainfo` |
| Vulkan Video capability | detected | Vulkan device and `VK_KHR_video_queue` |
| NVENC/NVDEC capability | detected | NVIDIA encode/decode driver libraries |
| Native hardware encode/decode data path | in progress | A common frame/codec interface feeds VA-API, Vulkan Video, and NVENC/NVDEC |
| Gamepad | Wine baseline | Wine HID/XInput for the controller UI; native host injection is planned |
| Resolution, orientation, HiDPI, zoom | partial | Portal stream geometry and Wine display settings |
| Privacy screen | planned | Compositor output policy and virtual display routing |
| Lock, reboot, shutdown, task manager | planned | logind/Polkit and desktop-native actions |
| Unattended login screen | compositor-dependent | Display-manager or compositor remote-desktop session |
| Upstream update detection | available | Official feed + API-contract CI probe |
