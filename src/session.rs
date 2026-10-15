//! Session orchestration: one command starts everything for a remote
//! session — desktop capture (portal, silent after first consent), the
//! input bridge, and the managed UU client.  When the client exits, the
//! session ends.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use crate::config::{data_dir, Config};

fn rotate_session_log() {
    let state = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    let _ = std::fs::create_dir_all(state.join("uur"));
    let _ = std::fs::rename(
        state.join("uur/session.log"),
        state.join("uur/session.log.1"),
    );
}

/// Locate the packaged hook DLLs.  Packaged installs use /usr/lib/uur/hook;
/// a dev checkout falls back to build/hook next to the sources.
const HOOK_COMPONENTS: &[&str] = &[
    "winlogon.exe",
    "wevtapi.dll",
    "uur-hook.dll",
    "wtsapi32.dll",
    "uur-terminal-proxy.exe",
    "uur-mux-proxy.exe",
    "uur-launch-proxy.exe",
];

fn complete_hook_directory(dir: &Path) -> bool {
    HOOK_COMPONENTS
        .iter()
        .all(|component| dir.join(component).is_file())
}

fn hook_dir() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("../lib/uur/hook"));
            candidates.push(dir.join("../../build/hook"));
            candidates.push(dir.to_path_buf());
        }
    }
    candidates.push(PathBuf::from("/usr/lib/uur/hook"));
    candidates.push(PathBuf::from("/usr/local/lib/uur/hook"));
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(home).join(".local/lib/uur/hook"));
    }
    if let Ok(directory) = std::env::current_dir() {
        candidates.push(directory.join("build/hook"));
    }
    candidates
        .into_iter()
        .find(|dir| complete_hook_directory(dir))
        .context("complete hook runtime not found (run the build first)")
}

fn hook_source_dir() -> Result<PathBuf> {
    hook_dir()
}

static DEPLOY_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Replace a managed prefix file without inheriting the source's permissions.
/// Nix store files are read-only; copying them directly leaves the prefix
/// read-only too, so the next run cannot overwrite them. A temporary file in
/// the destination directory keeps the replacement atomic and also repairs
/// read-only files left by earlier uur versions.
fn deploy_file(source: &Path, target: &Path) -> Result<()> {
    let parent = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let filename = target
        .file_name()
        .context("deployment target has no file name")?
        .to_string_lossy();

    let mut temporary = None;
    for _ in 0..32 {
        let sequence = DEPLOY_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".{filename}.uur-{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => {
                temporary = Some((path, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("creating temporary deployment for {}", target.display())
                });
            }
        }
    }
    let Some((temporary_path, mut output)) = temporary else {
        anyhow::bail!("could not create a temporary file for {}", target.display());
    };

    let copy_result = (|| -> Result<()> {
        let mut input = File::open(source)
            .with_context(|| format!("opening deployment source {}", source.display()))?;
        std::io::copy(&mut input, &mut output)
            .with_context(|| format!("copying deployment source {}", source.display()))?;
        output.flush()?;
        Ok(())
    })();
    drop(output);
    if let Err(error) = copy_result {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error);
    }

    if let Err(error) = std::fs::rename(&temporary_path, target) {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(error).with_context(|| format!("deploying to {}", target.display()));
    }
    Ok(())
}

/// Install the native preload DLLs in the managed prefix, outside the
/// proprietary application directory. Wine's native override resolves them
/// from system32, so client updates never overwrite uur and uur never edits an
/// official executable or DLL.
fn deploy_hooks(prefix: &Path) -> Result<()> {
    let source = hook_source_dir()?;
    let target = prefix.join("drive_c/windows/system32");
    std::fs::create_dir_all(&target)?;
    for dll in ["wevtapi.dll", "uur-hook.dll", "wtsapi32.dll"] {
        let from = source.join(dll);
        let to = target.join(dll);
        deploy_file(&from, &to)
            .with_context(|| format!("deploying {} to {}", dll, target.display()))?;
    }
    Ok(())
}

/// Replace UU's Windows ConPTY transport at its documented command-line ABI.
/// The adapter receives UU's inherited stdin/stdout handles and forwards them
/// to the native Linux PTY service. The official binary is retained for
/// recovery, and an obsolete powershell-level adapter is removed on upgrade.
fn deploy_terminal_proxy(install_dir: &Path) -> Result<PathBuf> {
    let source_dir = hook_source_dir()?;
    let source = source_dir.join("uur-terminal-proxy.exe");
    let mux_source = source_dir.join("uur-mux-proxy.exe");
    let bin = crate::wine::client_payload_dir(install_dir);
    std::fs::create_dir_all(&bin)?;
    for status in ["uur-terminal-adapter.status", "uur-mux-adapter.status"] {
        let _ = std::fs::remove_file(bin.join(status));
    }
    if let Ok(entries) = std::fs::read_dir(&bin) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("uur-terminal-session-") && name.ends_with(".active") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    let legacy_target = bin.join("powershell.exe");
    let legacy_backup = bin.join("powershell.exe.uur-original");
    let legacy_marker = bin.join("powershell.exe.uur-managed");
    if legacy_marker.exists() {
        let _ = std::fs::remove_file(&legacy_target);
        if legacy_backup.exists() {
            std::fs::rename(&legacy_backup, &legacy_target)
                .with_context(|| format!("restoring {}", legacy_target.display()))?;
        }
        let _ = std::fs::remove_file(&legacy_marker);
    }

    let target = bin.join("conpty_bridge.exe");
    let backup = bin.join("conpty_bridge.exe.uur-original");
    let marker = bin.join("conpty_bridge.exe.uur-managed");
    if target.exists() && !marker.exists() && !backup.exists() {
        std::fs::copy(&target, &backup)
            .with_context(|| format!("backing up {}", target.display()))?;
    }
    deploy_file(&source, &target)
        .with_context(|| format!("deploying native terminal proxy to {}", target.display()))?;
    std::fs::write(marker, b"managed by uur\n")?;

    let mux_target = bin.join("uuyc-mux.exe");
    let mux_backup = bin.join("uuyc-mux.exe.uur-original");
    let mux_marker = bin.join("uuyc-mux.exe.uur-managed");
    if mux_target.exists() && !mux_marker.exists() && !mux_backup.exists() {
        std::fs::copy(&mux_target, &mux_backup)
            .with_context(|| format!("backing up {}", mux_target.display()))?;
    }
    deploy_file(&mux_source, &mux_target)
        .with_context(|| format!("deploying native mux proxy to {}", mux_target.display()))?;
    std::fs::write(mux_marker, b"managed by uur\n")?;
    Ok(bin.join("uu-terminal-bridge.runtime"))
}

/// The client UI needs the Evergreen WebView2 runtime; the official
/// bootstrapper ships inside the client's bin directory.
const WEBVIEW2_DLL_OVERRIDES: &str = "wtsapi32=b,wevtapi=b";

fn webview2_install_command(prefix: &Path, bootstrapper: &Path) -> Command {
    let mut command = Command::new("wine");
    command
        .env("WINEPREFIX", prefix)
        .env("WINEDEBUG", wine_debug())
        // The bootstrapper is a 32-bit process. uur's native preload DLLs are
        // 64-bit, so inherited global native-only overrides make its updater
        // unable to load WTSAPI32.dll under WoW64.
        .env("WINEDLLOVERRIDES", WEBVIEW2_DLL_OVERRIDES)
        .arg(bootstrapper)
        .args(["/silent", "/install"]);
    command
}

fn ensure_webview2(prefix: &Path, install_dir: &Path) -> Result<()> {
    if crate::wine::webview2_installed(prefix) {
        return Ok(());
    }
    let bootstrapper =
        crate::wine::client_payload_dir(install_dir).join("MicrosoftEdgeWebview2Setup.exe");
    if !bootstrapper.is_file() {
        anyhow::bail!("{}", t!("session.webview_missing"));
    }
    println!("{}", t!("session.installing_webview"));
    let status = webview2_install_command(prefix, &bootstrapper)
        .status()
        .context("running the WebView2 bootstrapper")?;
    if !status.success() || !crate::wine::webview2_installed(prefix) {
        anyhow::bail!("{}", t!("session.webview_failed"));
    }
    Ok(())
}

/// Single-instance guard: an flock held for the lifetime of the session.
/// A second click on the launcher exits with a clear message instead of
/// fighting over the bridge port.
pub(crate) fn acquire_session_lock() -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let state = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    let dir = state.join("uur");
    std::fs::create_dir_all(&dir).context("creating state directory")?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("session.lock"))
        .context("opening session.lock")?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        anyhow::bail!("{}", t!("session.already_running"));
    }
    let mut file = file;
    file.set_len(0)?;
    writeln!(
        file,
        "{} {}",
        std::process::id(),
        process_start_time(std::process::id())?
    )?;
    file.sync_data()?;
    Ok(file)
}

struct ManagedChild {
    child: Child,
    name: String,
    terminated: bool,
}

impl ManagedChild {
    fn spawn_internal(argument: &str, log: &std::fs::File) -> Result<Self> {
        use std::os::unix::process::CommandExt;

        let executable = std::env::current_exe().context("resolving uur executable")?;
        let mut command = Command::new(executable);
        command
            .arg(argument)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log.try_clone()?))
            .process_group(0);
        let child = command
            .spawn()
            .with_context(|| format!("starting internal helper {argument}"))?;
        Ok(Self {
            child,
            name: argument.to_string(),
            terminated: false,
        })
    }

    fn ensure_running(&mut self) -> Result<()> {
        if let Some(status) = self.child.try_wait()? {
            anyhow::bail!(
                "helper {} exited ({status}); see $XDG_STATE_HOME/uur/session.log (normally ~/.local/state/uur/session.log)",
                self.name
            );
        }
        Ok(())
    }

    fn terminate(&mut self) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        let group = -(self.child.id() as i32);
        unsafe {
            libc::kill(group, libc::SIGTERM);
        }
        for _ in 0..20 {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(50));
        }
        unsafe {
            libc::kill(group, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

struct SessionResources {
    prefix: PathBuf,
    helpers: Vec<ManagedChild>,
    cleanup_files: Vec<PathBuf>,
    cleaned: bool,
}

impl SessionResources {
    fn new(prefix: PathBuf) -> Self {
        Self {
            prefix,
            helpers: Vec::new(),
            cleanup_files: Vec::new(),
            cleaned: false,
        }
    }

    fn shutdown(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        for helper in self.helpers.iter_mut().rev() {
            helper.terminate();
        }
        self.helpers.clear();
        for path in self.cleanup_files.drain(..) {
            let _ = std::fs::remove_file(path);
        }
        stop_wineserver(&self.prefix);
        if let Ok(path) = frame_path() {
            let _ = std::fs::remove_file(path);
        }
    }

    fn check_required_helpers(&mut self) -> Result<()> {
        for helper in &mut self.helpers {
            if matches!(
                helper.name.as_str(),
                "__bridge" | "__capture" | "__terminal" | "__launcher"
            ) {
                helper.ensure_running()?;
            }
        }
        Ok(())
    }
}

impl Drop for SessionResources {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Start everything and block until the client exits.
pub fn start() -> Result<()> {
    let _session_lock = acquire_session_lock()?;
    rotate_session_log();
    let config = Config::load()?;
    let on_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|session| session == "wayland");
    let prefix = data_dir()?.join("wine");
    let install_dir = crate::wine::find_client_dir(&prefix)
        .context("client installation incomplete; run `uur setup --repair` first")?;
    if !crate::wine::client_service_registered(&prefix) {
        anyhow::bail!("{}", t!("setup.service_incomplete"));
    }
    let mut resources = SessionResources::new(prefix.clone());
    if let Some(dpi) = xft_dpi() {
        match crate::wine::set_log_pixels(&prefix, dpi) {
            Ok(()) => println!("{}", t!("session.wine_dpi", dpi = dpi)),
            Err(error) => eprintln!("could not sync Wine DPI from Xft.dpi: {error}"),
        }
    }
    deploy_hooks(&prefix)?;
    crate::wine::set_dll_override(&prefix, "wtsapi32", "native")?;
    // Re-apply migrations on every launch so an updated uur package repairs
    // an existing prefix without requiring destructive reprovisioning.
    crate::wine::apply_registry_compat(&prefix)?;
    if let Some(proxy) = crate::proxy::sync(&prefix)? {
        println!("WinINet proxy synchronized: {proxy}");
    }
    let audio = crate::audio::sync_wine(&prefix)?;
    println!("{}", t!("session.audio_backend", backend = audio.label()));

    // Bridge configuration as a file inside the prefix: any process the
    // Wine service manager respawns reads it, env or not.
    let frames = frame_path()?;
    match std::fs::remove_file(&frames) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("removing the previous capture transport"),
    }
    let backend_status = crate::config::runtime_dir()?.join("backend");
    let _ = std::fs::remove_file(&backend_status);
    std::env::set_var("UUR_FRAME_PATH", &frames);
    std::env::set_var("UUR_BACKEND_STATUS", &backend_status);
    std::fs::write(
        prefix.join("drive_c/uur-bridge.ini"),
        format!(
            "[bridge]\nport={}\ntoken={}\nframe_path={}\n",
            config.bridge_port,
            config.bridge_token,
            unix_path_to_wine(&frames)
        ),
    )
    .context("writing uur-bridge.ini")?;

    let terminal_config = deploy_terminal_proxy(&install_dir)?;
    let launch_proxy = hook_source_dir()?.join("uur-launch-proxy.exe");
    let application_count = crate::launcher::prepare(&prefix, &launch_proxy)?;
    println!(
        "{}",
        t!("session.quick_launch_ready", count = application_count)
    );
    // Keep the start-menu entry pointing at the full session even if the
    // installer or an update regenerated it meanwhile.
    let _ = crate::wine::adopt_shortcut(
        &prefix,
        &std::env::current_exe().unwrap_or_else(|_| PathBuf::from("uur")),
    );
    ensure_webview2(&prefix, &install_dir)?;

    let stopping = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, stopping.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stopping.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGHUP, stopping.clone())?;

    let log = open_session_log()?;
    match ManagedChild::spawn_internal("__tray", &log) {
        Ok(helper) => resources.helpers.push(helper),
        Err(error) => eprintln!("native tray unavailable: {error}"),
    }
    let _ = std::fs::remove_file(&terminal_config);
    std::env::set_var("UUR_TERMINAL_CONFIG", &terminal_config);
    resources.cleanup_files.push(terminal_config.clone());
    resources
        .helpers
        .push(ManagedChild::spawn_internal("__terminal", &log)?);
    wait_for_runtime_file(&terminal_config, &stopping)?;
    let launcher_config = crate::launcher::runtime_config(&prefix);
    let _ = std::fs::remove_file(&launcher_config);
    resources.cleanup_files.push(launcher_config.clone());
    resources
        .helpers
        .push(ManagedChild::spawn_internal("__launcher", &log)?);
    wait_for_runtime_file(&launcher_config, &stopping)?;
    resources
        .helpers
        .push(ManagedChild::spawn_internal("__wallpaper", &log)?);
    match ManagedChild::spawn_internal("__inhibit", &log) {
        Ok(helper) => resources.helpers.push(helper),
        Err(error) => eprintln!("desktop inhibition unavailable: {error}"),
    }
    resources
        .helpers
        .push(ManagedChild::spawn_internal("__bridge", &log)?);
    wait_for_bridge(
        config.bridge_port,
        &stopping,
        resources.helpers.last_mut().unwrap(),
    )?;
    let selected_backend = std::fs::read_to_string(&backend_status).unwrap_or_default();
    let _ = std::fs::remove_file(&backend_status);
    if on_wayland && selected_backend.trim() != "portal" {
        resources
            .helpers
            .push(ManagedChild::spawn_internal("__capture", &log)?);
    }
    if on_wayland {
        println!(
            "{}",
            if config.capture_backend == "wayland" {
                t!("session.waiting_direct_capture").to_string()
            } else {
                t!("session.waiting_capture").to_string()
            }
        );
        wait_for_capture(&frames, &stopping, &mut resources)?;
    }

    // The service enumerates processes for a winlogon session token source
    // before it initialises; under plain Wine none exists.  A sleeping
    // process with that name satisfies the check.
    let marker = hook_dir()?.join("winlogon.exe");
    let _ = Command::new("wine")
        .env("WINEPREFIX", &prefix)
        .env("WINEDEBUG", wine_debug())
        .env("WINEDLLOVERRIDES", "wevtapi=n,wtsapi32=n")
        .env("UUR_HOOK_LOG", log_path()?)
        .env("UUR_BRIDGE_PORT", config.bridge_port.to_string())
        .env("UUR_BRIDGE_TOKEN", config.bridge_token.clone())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?))
        .arg(marker.to_string_lossy().as_ref())
        .spawn();

    // The launcher talks to the service through the Windows Service Control
    // Manager, so the service must be started through `sc start` — a
    // directly spawned --service process is invisible to it and the UI
    // waits forever.
    if !service_running(&prefix) {
        let _ = Command::new("wine")
            .env("WINEPREFIX", &prefix)
            .env("WINEDEBUG", wine_debug())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log.try_clone()?))
            .args(["sc", "start", "GameViewerService"])
            .status();
    }
    let service_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !service_running(&prefix) {
        resources.check_required_helpers()?;
        if stopping.load(Ordering::Relaxed) {
            anyhow::bail!("session startup cancelled");
        }
        if std::time::Instant::now() >= service_deadline {
            anyhow::bail!("{}", t!("session.service_failed"));
        }
        thread::sleep(std::time::Duration::from_millis(200));
    }
    println!("{}", t!("session.server_running"));

    println!("{}", t!("session.launching_client"));
    let ui = install_dir.join("GameViewer.exe");
    let launch = || -> Result<Child> {
        Command::new("wine")
            .env("WINEPREFIX", &prefix)
            .env("WINEDEBUG", wine_debug())
            .current_dir(&install_dir)
            .env("WINEDLLOVERRIDES", "wevtapi=n,wtsapi32=n")
            .env("UUR_HOOK_LOG", log_path().unwrap_or_default())
            .env("UUR_BRIDGE_PORT", config.bridge_port.to_string())
            .env("UUR_BRIDGE_TOKEN", config.bridge_token.clone())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log.try_clone()?))
            .arg(ui.to_string_lossy().as_ref())
            .spawn()
            .context("launching the managed client")
    };

    // Start the client detached: it IS the session's lifetime anchor.
    let mut client = launch()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    let mut seen_client = false;
    let mut missing_checks = 0u8;
    let mut relaunches = 0;
    while !stopping.load(Ordering::Relaxed) {
        resources.check_required_helpers()?;
        let running = wine_client_process_exists(&prefix);
        if running {
            seen_client = true;
            missing_checks = 0;
        } else if seen_client {
            missing_checks = missing_checks.saturating_add(1);
            // UU hands off between short-lived launchers and its real Qt
            // process. Require a sustained absence before treating it as a
            // user-requested exit.
            if missing_checks >= 6 {
                break;
            }
        }
        if !seen_client && std::time::Instant::now() >= deadline {
            anyhow::bail!("UU Remote did not create its client process within 45 seconds");
        }
        if !seen_client && client.try_wait()?.is_some() && relaunches < 2 {
            relaunches += 1;
            thread::sleep(std::time::Duration::from_secs(2));
            client = launch()?;
        }
        thread::sleep(std::time::Duration::from_millis(500));
    }

    println!("{}", t!("session.client_exited", code = 0));

    resources.shutdown();
    Ok(())
}

/// Query the Windows process table belonging to this exact Wine prefix.
/// Modern Wine may host a Windows process in a Linux process whose `/proc`
/// cmdline names another image, so Linux process-name matching is not a valid
/// lifecycle boundary.
fn wine_client_process_exists(prefix: &Path) -> bool {
    let output = run_with_timeout(
        Command::new("wine")
            .env("WINEPREFIX", prefix)
            .env("WINEDEBUG", wine_debug())
            .args([
                "tasklist",
                "/NH",
                "/FO",
                "CSV",
                "/FI",
                "IMAGENAME eq GameViewer.exe",
            ]),
        std::time::Duration::from_secs(5),
    );
    output.is_some_and(|text| tasklist_contains_image(&text, "GameViewer.exe"))
}

/// Run a command with a hard timeout, returning its stdout (None on
/// timeout or failure).  `wine sc query` has been observed to hang
/// indefinitely against a freshly booted wineserver; without a deadline
/// the whole session startup would block forever.
fn run_with_timeout(command: &mut Command, timeout: std::time::Duration) -> Option<String> {
    use std::process::Stdio;

    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
            Err(_) => return None,
        }
        thread::sleep(std::time::Duration::from_millis(50));
    }
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    Some(stdout)
}

/// True when the Windows service reports STATE 4 (RUNNING) via `sc query`.
fn service_running(prefix: &Path) -> bool {
    let output = run_with_timeout(
        Command::new("wine")
            .env("WINEPREFIX", prefix)
            .env("WINEDEBUG", wine_debug())
            .args(["sc", "query", "GameViewerService"]),
        std::time::Duration::from_secs(3),
    );
    match output {
        Some(text) => text.lines().any(|line| {
            let line = line.trim();
            line.starts_with("STATE") && line.contains("RUNNING")
        }),
        None => false,
    }
}

fn tasklist_contains_image(output: &str, image: &str) -> bool {
    output.lines().any(|line| {
        line.split(',')
            .next()
            .map(|field| field.trim().trim_matches('"').eq_ignore_ascii_case(image))
            .unwrap_or(false)
    })
}

fn log_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("hook.log"))
}

/// End the session: stop the capture helper, terminate the managed prefix's
/// Wine processes, and drop the bridge connection.
pub fn stop() -> Result<()> {
    let prefix = data_dir()?.join("wine");
    if let Some((pid, start)) = read_session_identity()? {
        if process_start_time(pid).ok() == Some(start) {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            for _ in 0..100 {
                if process_start_time(pid).is_err() {
                    println!("{}", t!("bridge.stop_requested"));
                    return Ok(());
                }
                thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    stop_wineserver(&prefix);
    if let Ok(path) = frame_path() {
        let _ = std::fs::remove_file(path);
    }
    println!("{}", t!("bridge.stop_requested"));
    Ok(())
}

/// Activate the official client's existing single-instance UI from the
/// native tray without starting another capture/input supervisor.
pub(crate) fn show_client() -> Result<()> {
    let prefix = data_dir()?.join("wine");
    let install =
        crate::wine::find_client_dir(&prefix).context("managed client is not installed")?;
    let Some((pid, start)) = read_session_identity()? else {
        anyhow::bail!("managed session is not running");
    };
    if process_start_time(pid).ok() != Some(start) {
        anyhow::bail!("managed session is not running");
    }
    let mut child = Command::new("wine")
        .env("WINEPREFIX", prefix)
        .env("WINEDEBUG", wine_debug())
        .env("WINEDLLOVERRIDES", "wevtapi=n,wtsapi32=n")
        .current_dir(&install)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg(install.join("GameViewer.exe"))
        .spawn()
        .context("showing the managed client")?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn state_dir() -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join("uur")
}

fn open_session_log() -> Result<std::fs::File> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("session.log"))
        .context("opening session log")
}

fn read_session_identity() -> Result<Option<(u32, u64)>> {
    let path = state_dir().join("session.lock");
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let mut fields = contents.split_whitespace();
    let Some(pid) = fields.next().and_then(|value| value.parse().ok()) else {
        return Ok(None);
    };
    let Some(start) = fields.next().and_then(|value| value.parse().ok()) else {
        return Ok(None);
    };
    Ok(Some((pid, start)))
}

fn process_start_time(pid: u32) -> Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let end = stat.rfind(") ").context("malformed /proc stat")?;
    stat[end + 2..]
        .split_whitespace()
        .nth(19)
        .context("missing process start time")?
        .parse()
        .context("invalid process start time")
}

fn frame_path() -> Result<PathBuf> {
    Ok(crate::config::runtime_dir()?.join("frames.v1"))
}

fn unix_path_to_wine(path: &Path) -> String {
    format!("Z:{}", path.display()).replace('/', "\\")
}

fn wait_for_bridge(port: u16, stopping: &AtomicBool, helper: &mut ManagedChild) -> Result<()> {
    // The first RemoteDesktop portal grant is intentionally interactive.
    // Keep the supervisor alive while the user chooses a monitor instead of
    // treating a two-second consent delay as a bridge failure.
    for _ in 0..6000 {
        if stopping.load(Ordering::Relaxed) {
            anyhow::bail!("session startup cancelled");
        }
        helper.ensure_running()?;
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Ok(());
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
    anyhow::bail!("input bridge did not become ready")
}

fn wait_for_capture(
    frames: &Path,
    stopping: &AtomicBool,
    resources: &mut SessionResources,
) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        if stopping.load(Ordering::Relaxed) {
            anyhow::bail!("session startup cancelled");
        }
        resources.check_required_helpers()?;
        if crate::capture::has_frame(frames) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("{}", t!("session.capture_not_ready"));
        }
        thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn wait_for_runtime_file(path: &Path, stopping: &AtomicBool) -> Result<()> {
    for _ in 0..250 {
        if stopping.load(Ordering::Relaxed) {
            anyhow::bail!("session startup cancelled");
        }
        if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
            return Ok(());
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
    anyhow::bail!("native terminal bridge did not become ready")
}

fn xft_dpi() -> Option<u32> {
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

    let (connection, screen) = x11rb::connect(None).ok()?;
    let root = connection.setup().roots.get(screen)?.root;
    let resource_manager = connection
        .intern_atom(false, b"RESOURCE_MANAGER")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let resource_manager = connection
        .get_property(false, root, resource_manager, AtomEnum::STRING, 0, 16_384)
        .ok()?
        .reply()
        .ok()?;
    parse_xft_dpi(&String::from_utf8_lossy(&resource_manager.value))
}

fn parse_xft_dpi(resources: &str) -> Option<u32> {
    resources.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim() != "Xft.dpi" {
            return None;
        }
        let dpi = value.trim().parse::<f64>().ok()?;
        (dpi.is_finite() && (96.0..=480.0).contains(&dpi)).then(|| dpi.round() as u32)
    })
}

fn stop_wineserver(prefix: &Path) {
    let _ = Command::new("wineserver")
        .env("WINEPREFIX", prefix)
        .arg("-k")
        .status();
}

fn wine_debug() -> String {
    std::env::var("WINEDEBUG").unwrap_or_else(|_| "-all".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn create() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            for _ in 0..32 {
                let path = std::env::temp_dir().join(format!(
                    "uur-session-deploy-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("creating test directory: {error}"),
                }
            }
            panic!("could not create a unique test directory");
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn deploy_file_repairs_readonly_targets_and_is_repeatable() {
        let directory = TestDirectory::create();
        let source = directory.path().join("store/uur-hook.dll");
        let target = directory.path().join("prefix/system32/uur-hook.dll");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&source, b"new hook contents").unwrap();
        std::fs::write(&target, b"old hook contents").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o444)).unwrap();

        deploy_file(&source, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new hook contents");
        assert_ne!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o200,
            0
        );

        deploy_file(&source, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new hook contents");
        assert_ne!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o200,
            0
        );
        assert_eq!(
            std::fs::read_dir(target.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn deploy_file_does_not_inherit_readonly_source_permissions() {
        let directory = TestDirectory::create();
        let source = directory.path().join("store/uur-hook.dll");
        let target = directory.path().join("prefix/system32/uur-hook.dll");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&source, b"hook contents").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o444)).unwrap();

        deploy_file(&source, &target).unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"hook contents");
        assert_ne!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o200,
            0
        );
    }

    #[test]
    fn parses_xft_dpi_resources() {
        assert_eq!(parse_xft_dpi("Xft.antialias: 1\nXft.dpi: 192\n"), Some(192));
        assert_eq!(parse_xft_dpi("Xft.dpi: 168.4\n"), Some(168));
    }

    #[test]
    fn ignores_invalid_xft_dpi_resources() {
        assert_eq!(parse_xft_dpi("Xft.dpi: 72\n"), None);
        assert_eq!(parse_xft_dpi("Xft.dpi: 500\n"), None);
        assert_eq!(parse_xft_dpi("Xft.dpi: automatic\n"), None);
        assert_eq!(parse_xft_dpi("Xft.antialias: 192\n"), None);
    }

    #[test]
    fn webview2_installer_uses_builtin_wow64_dlls() {
        let command = webview2_install_command(
            Path::new("/tmp/uur-wine-prefix"),
            Path::new("/tmp/MicrosoftEdgeWebview2Setup.exe"),
        );
        let overrides = command
            .get_envs()
            .find_map(|(name, value)| (name == OsStr::new("WINEDLLOVERRIDES")).then_some(value))
            .flatten();
        assert_eq!(overrides, Some(OsStr::new(WEBVIEW2_DLL_OVERRIDES)));
    }

    #[test]
    fn tasklist_parser_matches_the_exact_image() {
        let output = "\"GameViewer.exe\",\"456\",\"Console\",\"1\"\n\
                      \"GameViewerServer.exe\",\"464\",\"Console\",\"1\"\n";
        assert!(tasklist_contains_image(output, "GameViewer.exe"));
        assert!(!tasklist_contains_image(output, "GameViewerService.exe"));
    }

    #[test]
    fn current_process_identity_is_readable() {
        assert!(process_start_time(std::process::id()).unwrap() > 0);
    }

    #[test]
    fn runtime_path_converts_to_a_wine_z_path() {
        assert_eq!(
            unix_path_to_wine(Path::new("/run/user/1000/uur/frames.v1")),
            r"Z:\run\user\1000\uur\frames.v1"
        );
    }
}
