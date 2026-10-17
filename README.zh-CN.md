<p align="center">
  <img src="packaging/uur-icon.png" width="112" alt="UU 远程图标">
</p>

<h1 align="center">uur</h1>

<p align="center">
  适用于网易 UU 远程官方客户端的原生 Linux 兼容层。
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Linux-x86__64-FCC624?style=flat-square&logo=linux&logoColor=black" alt="Linux x86_64">
  <img src="https://img.shields.io/badge/Desktop-X11%20%7C%20Wayland-4A90E2?style=flat-square" alt="X11 与 Wayland">
  <img src="https://img.shields.io/badge/Language-Rust-DEA584?style=flat-square&logo=rust&logoColor=black" alt="Rust">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue?style=flat-square" alt="MIT 许可证"></a>
</p>

<p align="center">
  <a href="README.md">English</a> · <strong>简体中文</strong>
</p>

> [!IMPORTANT]
> `uur` 不分发 UU 远程客户端。只有用户明确接受网易官方协议后，它才会从网易官方
> 地址下载 Windows 安装程序。

## ✨ 功能亮点

| 能力 | Linux 集成方式 |
| --- | --- |
| 控制其他电脑 | 在 Wine 中使用 UU 官方界面和媒体栈 |
| 让本 Linux 设备被控 | ScreenCast Portal/PipeWire 画面与 Linux 原生输入 |
| X11 与 Wayland | 按实际能力探测，不维护发行版或桌面白名单 |
| 生命周期 | UU 启动时加载桥接，受管 Wine 前缀退出时一并清理 |
| 原生工具 | Linux PTY 终端、壁纸、快速启动、代理同步与防休眠 |
| 上游兼容 | 基于稳定 API 的运行时适配，不维护版本/RVA 补丁矩阵 |
| 软件分发 | Arch/pacman、deb、rpm、通用 tar.zst 与 AUR 配方 |

## 🚀 首次运行

安装任意格式的软件包后，必须**按顺序**执行：

```bash
# 1. 检查当前 Wine、Portal、PipeWire 和输入能力。
uur doctor

# 2. 接受网易协议、下载官方客户端并创建 Wine 前缀。
uur setup --accept-eula

# 3. 启动画面、输入桥、受管 Wine 服务和 UU 主界面。
uur run
```

不带 `--accept-eula` 的 `uur setup` 只会显示官方协议地址并停止，不会安装专有客户端。

很多功能尚未经测试；目前实际运行测试仅覆盖 Arch Linux、KDE Plasma 和 Wayland。

设置完成后，也可以从应用菜单打开 UU Remote。菜单入口会启动完整的 `uur run` 会话。

## 📦 安装

请从 [GitHub Releases](https://github.com/panxuc/uur/releases/latest) 下载适合当前系统的
最新软件包。所有原生配方与发布产物使用同一版本和文件布局。

### Arch Linux

使用 pacman 安装下载的正式发布包：

```bash
sudo pacman -U ./dist/uur-0.1.6-1-x86_64.pkg.tar.zst
```

### AUR

软件包正式发布到 AUR 后可执行：

```bash
paru -S uur
```

### Debian、Ubuntu 及兼容发行版

```bash
sudo apt install ./uur_0.1.6_amd64.deb
```

### Fedora 及兼容 RPM 发行版

```bash
sudo dnf install ./uur-0.1.6-1.x86_64.rpm
```

如果发行版的 RPM 依赖名称不同，请手动安装依赖或使用通用压缩包。

### 通用压缩包

```bash
sudo tar --zstd -C / -xf ./uur-0.1.6-linux-x86_64.tar.zst
```

### Nix 与 NixOS

```bash
nix build
nix run . -- doctor
```

flake 同时导出 `nixosModules.default`，详见 [Nix 与 NixOS](docs/nix.md)。

### Alpine Linux

Alpine edge 可直接构建原生 musl 软件包布局：

```bash
docker build -f packaging/alpine/Dockerfile .
```

`packaging/alpine/` 中也提供了 `APKBUILD`。

### 从源码安装到当前用户

```bash
cargo build --release --locked
./hook/build.sh
./capture/build.sh
./scripts/install-user.sh
```

源码构建需要 Rust、C 编译器、PipeWire 与 Wayland 开发文件（含 `wayland-scanner`）、`pkg-config` 和 MinGW-w64 x86_64
工具链。

Gentoo、Void Linux、Slackware、Solus、ALT Linux、Exherbo、Clear Linux、Mageia、
OpenMandriva、PCLinuxOS、Venom Linux、Guix System 等软件包生态可以使用通用压缩包，
或复用 `packaging/stage.sh` 的源码构建布局。

发行版家族、Portal 选择、X11/Wayland、独立窗口管理器、不可变系统和 libc 边界见
[平台支持与使用说明](docs/platform-support.md)。

## 🧭 日常使用

```bash
uur run                 # 启动或显示受管 UU 会话
uur doctor              # 只读环境诊断
uur upstream check      # 检查网易官方版本
uur autostart enable    # 可选：图形会话登录后自动启动
uur autostart disable   # 删除受管登录自启动项
uur wol status          # 检查物理有线网卡的 WOL 状态
uur display status      # 查看显示器、窗口或 Portal 虚拟源设置
uur display source virtual
uur display outputs     # 查看原生 Wayland 显示器名称
uur stop                # 只停止 uur 助手与专用 Wine 前缀
uur uninstall --purge --dry-run  # 预览完整用户数据清理范围
uur uninstall --purge   # 确认清理后，再用包管理器卸载系统包
```

从桌面启动 UU 后，可在启动器的应用操作菜单中选择“退出”；从终端运行时可执行
`uur stop` 或按 Ctrl+C。

niri 可使用原生托盘，并明确启用直接采集：先运行 `uur display outputs` 查看名称，再运行
`uur display backend wayland --output DP-1`（替换为自己的名称）。此模式使用 uinput，
直接采集指定显示器，不弹出 Portal 授权框；`uur display backend portal` 可恢复系统授权采集。

仅用包管理器卸载会保留 Wine 环境和账号数据。完整卸载请先以当前用户身份运行
`uur uninstall --purge`，再执行 `sudo pacman -Rns uur`（Arch）；其他发行版使用
对应的包管理器。详见[完整卸载说明](docs/distribution.md#package-removal-repair-and-per-user-state)。

输入后端排错：

```bash
UUR_INPUT_BACKEND=portal uur run
UUR_INPUT_BACKEND=uinput uur run
UUR_INPUT_BACKEND=xtest uur run
```

排错结束后应恢复默认的 `auto` 模式。

## 🖥️ 桌面集成

- 同时提供 RemoteDesktop 和 ScreenCast Portal 的 Wayland 会话，使用一个无特权组合
  会话传输输入与画面。
- 只提供 ScreenCast 的 Wayland 会话，使用 PipeWire 捕获和 uinput 全桌面输入。
- X11 会话使用 Wine 原生 X11 捕获与 XTest 输入。
- XWayland 下的 XTest 只是局部回退，不会被描述成完整 Wayland 桌面控制。

请以 `uur doctor` 的实际接口检查为准，不要仅根据桌面或发行版名称推断支持情况。

## 🧩 Linux 原生功能

画面、键盘、鼠标、生命周期管理、壁纸信息、原生终端、快速启动、代理同步、防休眠和
主机 WOL 配置已经实现。快速启动会把 XDG 应用目录提供给 UU，并将每次请求重新解析为
`gio launch`；Wine 侧不能传入任意命令。其他 UU 功能应由 Linux 原生服务承接，而不是
留在 Wine 虚拟环境中：

| 功能 | 实现方向 |
| --- | --- |
| 终端 | UU 鉴权通道 → 原生 PTY 与当前用户登录 Shell |
| 快速启动 | UU 应用清单 → 带鉴权的 XDG 桌面应用激活 |
| 网络唤醒配置 | `uur wol` → 真实有线网卡与 NetworkManager 配置 |
| 接收文件 | 安全暂存与校验 → 用户 XDG 下载目录 |
| 发送文件 | File Portal 或显式 CLI 选择 → 有界原生暂存区 |
| 端口映射 | 带策略的原生 TCP/UDP broker，默认只绑定回环地址 |
| 剪贴板与手机文字 | RemoteDesktop Clipboard Portal 与 X11 回退 |

协议、安全边界、生命周期和实施阶段见
[Linux 原生服务适配器](docs/native-services.md)。

## 🔬 工作原理

```text
官方 UU 远程客户端（受管 Wine 前缀）
        │
        ├─ Windows API 适配器 ── 带鉴权的输入/控制协议
        │
        └─ GDI/DXGI 捕获边界 ◄── 私有三缓冲画面传输
                                         ▲
Linux 监督器 ── ScreenCast Portal ── PipeWire
        │
        └─ RemoteDesktop Portal / uinput / XTest
```

Windows ABI 边界保留少量 C 代码；会话策略、桌面集成、能力选择和生命周期由 Rust
实现。

## 🛠️ 排错

首先执行：

```bash
uur doctor
```

会话日志位于：

```text
${XDG_STATE_HOME:-$HOME/.local/state}/uur/session.log
```

只在需要 Wine 诊断时执行：

```bash
WINEDEBUG=+loaddll uur run
```

正常启动会隐藏无操作价值的 Wine `fixme` 输出。

## 📚 文档

- [架构](docs/architecture.md)
- [平台支持与使用](docs/platform-support.md)
- [画面传输协议](docs/capture-protocol.md)
- [Linux 原生服务适配器](docs/native-services.md)
- [功能对齐](docs/feature-parity.md)
- [上游自动适配](docs/upstream-adaptation.md)
- [软件包安装](docs/distribution.md)
- [Nix 与 NixOS](docs/nix.md)
- [Windows 功能映射](docs/windows-feature-map.md)
- [视频与显示后端](docs/video-backends.md)

## 🙏 致谢

本项目参考了
[GuoWQ222/uu-remote-for-linux](https://github.com/GuoWQ222/uu-remote-for-linux)、
[lachlanchen/uu-remote-ubuntu-bridge](https://github.com/lachlanchen/uu-remote-ubuntu-bridge)。

网易 UU 远程是网易的专有软件。`uur` 是非官方兼容项目，与网易无隶属或合作关系。
