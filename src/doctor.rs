use anyhow::Result;

fn line(status: &str, label: &str, detail: &str) {
    println!("{status:<8} {label:<28} {detail}");
}

fn ok(missing: bool) -> &'static str {
    if missing {
        "MISS"
    } else {
        "ok"
    }
}

fn command_present(name: &str) -> bool {
    std::env::var("PATH")
        .map(|path| {
            path.split(':')
                .any(|dir| std::path::Path::new(dir).join(name).exists())
        })
        .unwrap_or(false)
}

fn wine_version() -> Option<String> {
    let output = std::process::Command::new("wine")
        .arg("--version")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Some(text)
}

fn distro() -> String {
    let raw = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let field = |key: &str| {
        raw.lines().find(|l| l.starts_with(key)).map(|l| {
            l.split('=')
                .nth(1)
                .unwrap_or("")
                .trim_matches('"')
                .to_string()
        })
    };
    match (field("PRETTY_NAME"), field("ID")) {
        (Some(pretty), _) => pretty,
        (None, Some(id)) => id,
        _ => "unknown".into(),
    }
}

fn portal_backends() -> Vec<String> {
    let mut found = Vec::new();
    let mut directories = vec![
        std::path::PathBuf::from("/usr/lib/xdg-desktop-portal/portals"),
        std::path::PathBuf::from("/usr/share/xdg-desktop-portal/portals"),
    ];
    directories.extend(
        std::env::var("XDG_DATA_DIRS")
            .unwrap_or_else(|_| "/usr/local/share:/usr/share".into())
            .split(':')
            .filter(|entry| !entry.is_empty())
            .map(|entry| std::path::Path::new(entry).join("xdg-desktop-portal/portals")),
    );
    for dir in directories {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let short = name
                    .trim_end_matches(".portal")
                    .trim_start_matches("portal-")
                    .to_string();
                if !found.contains(&short) {
                    found.push(short);
                }
            }
        }
    }
    found.sort();
    found
}

fn library_present(soname: &str) -> bool {
    // `ldconfig -p` output is small; capture fully to avoid SIGPIPE races.
    let cache = std::process::Command::new("ldconfig")
        .arg("-p")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    cache.contains(soname)
        || ["/lib", "/usr/lib", "/usr/local/lib"]
            .iter()
            .any(|directory| {
                std::fs::read_dir(directory)
                    .ok()
                    .into_iter()
                    .flatten()
                    .flatten()
                    .any(|entry| entry.file_name().to_string_lossy().contains(soname))
            })
}

fn runtime_component_present(name: &str) -> bool {
    let mut candidates = vec![
        std::path::PathBuf::from("/usr/lib/uur/hook").join(name),
        std::path::PathBuf::from("/usr/local/lib/uur/hook").join(name),
    ];
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(
            std::path::PathBuf::from(home)
                .join(".local/lib/uur/hook")
                .join(name),
        );
    }
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            candidates.push(directory.join("../lib/uur/hook").join(name));
            candidates.push(directory.join("../../build/hook").join(name));
        }
    }
    if let Ok(directory) = std::env::current_dir() {
        candidates.push(directory.join("build/hook").join(name));
    }
    candidates.into_iter().any(|path| path.is_file())
}

fn portal_capabilities() -> (bool, bool, String) {
    let Ok(runtime) = tokio::runtime::Runtime::new() else {
        return (false, false, "unavailable".into());
    };
    runtime.block_on(async {
        let (capture, sources) = match ashpd::desktop::screencast::Screencast::new().await {
            Ok(portal) => match portal.available_source_types().await {
                Ok(types) => (!types.is_empty(), format!("{types:?}")),
                Err(_) => (false, "unavailable".into()),
            },
            Err(_) => (false, "unavailable".into()),
        };
        let input = match ashpd::desktop::remote_desktop::RemoteDesktop::new().await {
            Ok(portal) => portal.available_device_types().await.is_ok_and(|types| {
                types.contains(ashpd::desktop::remote_desktop::DeviceType::Keyboard)
                    && types.contains(ashpd::desktop::remote_desktop::DeviceType::Pointer)
            }),
            Err(_) => false,
        };
        (capture, input, sources)
    })
}

/// Environment report.  Everything is advisory: doctor never gates.
pub fn report() -> Result<()> {
    println!("{}", t!("doctor.header"));
    println!();

    line("info", "distro", &distro());
    line(
        "info",
        "session",
        &format!(
            "type={} desktop={}",
            std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".into()),
            std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unknown".into()),
        ),
    );
    line(
        "info",
        "display",
        &format!(
            "DISPLAY={} WAYLAND_DISPLAY={}",
            std::env::var("DISPLAY").unwrap_or_else(|_| "-".into()),
            std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "-".into()),
        ),
    );
    println!();

    let missing_wine = !command_present("wine");
    let wine_detail: String = match (!missing_wine).then(wine_version).flatten() {
        Some(version) => version,
        None => t!("doctor.install_hint").to_string(),
    };
    line(ok(missing_wine), "wine", &wine_detail);

    let prefix = crate::config::data_dir()?.join("wine");
    line(
        ok(!crate::wine::prefix_initialized(&prefix)),
        "managed Wine prefix",
        &prefix.display().to_string(),
    );
    let client = crate::wine::find_client_dir(&prefix);
    let client_detail = client
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| t!("doctor.client_incomplete").to_string());
    line(ok(client.is_none()), "UU client and server", &client_detail);
    let service = crate::wine::client_service_registered(&prefix);
    let service_detail = if service {
        t!("doctor.service_registered").to_string()
    } else {
        t!("doctor.service_missing").to_string()
    };
    line(ok(!service), "GameViewerService", &service_detail);
    let webview_detail = if crate::wine::webview2_installed(&prefix) {
        t!("doctor.present").to_string()
    } else {
        t!("doctor.webview_pending").to_string()
    };
    line("info", "WebView2 runtime", &webview_detail);

    let backends = portal_backends();
    let has_portal_impl = !backends.is_empty();
    let portal_detail = if backends.is_empty() {
        t!("doctor.no_portal_backend").to_string()
    } else {
        backends.join(", ")
    };
    line(ok(!has_portal_impl), "xdg-desktop-portal", &portal_detail);

    let (portal_capture, portal_input, portal_sources) = portal_capabilities();
    let portal_capture_detail = if portal_capture {
        t!("doctor.portal_capture_advertised").to_string()
    } else {
        t!("doctor.portal_capture_missing").to_string()
    };
    line(
        ok(!portal_capture),
        "portal ScreenCast",
        &portal_capture_detail,
    );
    line("info", "portal source types", &portal_sources);
    let portal_input_detail = if portal_input {
        t!("doctor.portal_input_advertised").to_string()
    } else {
        t!("doctor.portal_input_missing").to_string()
    };
    line(
        ok(!portal_input),
        "portal RemoteDesktop",
        &portal_input_detail,
    );

    let missing_pw = !library_present("libpipewire");
    let pw_detail = if missing_pw {
        t!("doctor.install_hint").to_string()
    } else {
        t!("doctor.present").to_string()
    };
    line(ok(missing_pw), "PipeWire", &pw_detail);

    let audio = crate::audio::detect();
    line("info", "audio backend", audio.label());

    let video = crate::video::detect();
    line(
        "info",
        "DRM render node",
        crate::video::describe(video.render_node, "present", "not detected"),
    );
    line(
        "info",
        "VA-API capability",
        crate::video::describe(video.va_api, "available", "not probed; install vainfo"),
    );
    line(
        "info",
        "Vulkan capability",
        crate::video::describe(video.vulkan, "available", "not probed; install vulkaninfo"),
    );
    line(
        "info",
        "Vulkan Video",
        crate::video::describe(video.vulkan_video, "video queue available", "not detected"),
    );
    line(
        "info",
        "NVENC / NVDEC",
        &format!(
            "encode={} decode={}",
            crate::video::describe(video.nvenc, "available", "missing"),
            crate::video::describe(video.nvdec, "available", "missing")
        ),
    );

    let terminal_proxy = runtime_component_present("uur-terminal-proxy.exe");
    let terminal_detail = if terminal_proxy {
        t!("doctor.present").to_string()
    } else {
        t!("doctor.terminal_missing").to_string()
    };
    line(
        ok(!terminal_proxy),
        "native terminal adapter",
        &terminal_detail,
    );

    let mux_proxy = runtime_component_present("uur-mux-proxy.exe");
    let mux_detail = if mux_proxy {
        t!("doctor.present").to_string()
    } else {
        t!("doctor.terminal_missing").to_string()
    };
    line(ok(!mux_proxy), "terminal session adapter", &mux_detail);

    let launch_proxy = runtime_component_present("uur-launch-proxy.exe");
    let launch_detail = if launch_proxy {
        t!("doctor.present").to_string()
    } else {
        t!("doctor.launcher_missing").to_string()
    };
    line(ok(!launch_proxy), "UU Quick Launch adapter", &launch_detail);
    let has_application_launcher = command_present("gio") || command_present("gtk-launch");
    let application_launcher_detail = if has_application_launcher {
        t!("doctor.present").to_string()
    } else {
        t!("doctor.xdg_launcher_missing").to_string()
    };
    line(
        ok(!has_application_launcher),
        "XDG application launcher",
        &application_launcher_detail,
    );

    let wallpaper = crate::wallpaper::detect();
    let wallpaper_detail = wallpaper
        .as_ref()
        .map(|wallpaper| format!("{} ({})", wallpaper.path.display(), wallpaper.provider))
        .unwrap_or_else(|| t!("doctor.wallpaper_missing").to_string());
    line(
        ok(wallpaper.is_none()),
        "desktop wallpaper",
        &wallpaper_detail,
    );

    // /dev/uinput: the Wayland injection path.  Three states matter:
    // missing file, module not loaded (ENODEV), and permission (EACCES).
    let uinput_detail = match std::fs::OpenOptions::new().write(true).open("/dev/uinput") {
        Ok(_) => t!("doctor.present").to_string(),
        Err(error) => match error.raw_os_error() {
            Some(19) => t!("doctor.uinput_module").to_string(),
            Some(13) => t!("doctor.uinput_permission").to_string(),
            _ => t!("doctor.uinput_missing").to_string(),
        },
    };
    let uinput_missing = uinput_detail != t!("doctor.present");
    line(ok(uinput_missing), "uinput (/dev/uinput)", &uinput_detail);

    // Report the configured policy; portal properties do not validate that
    // the compositor can successfully create an authorized session.
    let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let on_wayland = session_type == "wayland" || std::env::var_os("WAYLAND_DISPLAY").is_some();
    let configured = crate::config::Config::load()?;
    line(
        "info",
        "capture backend",
        &format!(
            "{} ({}; multiple={})",
            configured.capture_backend,
            configured
                .capture_output
                .as_deref()
                .unwrap_or("selected/default output"),
            configured.capture_multiple
        ),
    );
    let mut preferred = std::env::var("UUR_INPUT_BACKEND").unwrap_or(configured.input_backend);
    if configured.capture_backend == "wayland" && preferred == "auto" {
        preferred = "uinput".into();
    }
    let (status, backend, backend_note) = match preferred.as_str() {
        "portal" => (
            ok(!portal_input),
            "portal",
            t!("doctor.backend_portal").to_string(),
        ),
        "uinput" => (
            ok(uinput_missing),
            "uinput",
            t!("doctor.backend_wayland").to_string(),
        ),
        "xtest" if on_wayland => ("WARN", "xtest", t!("doctor.backend_partial").to_string()),
        "xtest" => (
            ok(std::env::var_os("DISPLAY").is_none()),
            "xtest",
            t!("doctor.backend_x11").to_string(),
        ),
        "auto" if on_wayland && portal_input => {
            ("info", "portal", t!("doctor.backend_portal").to_string())
        }
        "auto" if on_wayland && !uinput_missing => {
            ("info", "uinput", t!("doctor.backend_wayland").to_string())
        }
        "auto" if on_wayland => (
            "MISS",
            "none",
            t!("doctor.backend_wayland_missing").to_string(),
        ),
        "auto" if std::env::var_os("DISPLAY").is_some() => {
            ("info", "xtest", t!("doctor.backend_x11").to_string())
        }
        _ => ("MISS", "none", t!("doctor.backend_none").to_string()),
    };
    line(
        status,
        "input backend",
        &format!("{backend} ({backend_note})"),
    );

    match crate::wol::inspect(None) {
        Ok(state) => line(
            "info",
            "Wake-on-LAN",
            &format!(
                "{} {} ({})",
                state.interface,
                state.mac,
                match state.enabled {
                    Some(true) => "enabled",
                    Some(false) => "disabled",
                    None => "unknown; install ethtool",
                }
            ),
        ),
        Err(error) => line("info", "Wake-on-LAN", &error.to_string()),
    }

    println!();
    println!("{}", t!("doctor.footer"));
    Ok(())
}
