//! Portal side of the capture path.
//!
//! Negotiates an xdg-desktop-portal ScreenCast session, obtains a private
//! PipeWire file descriptor and the video node id, then hands both to the
//! C capture helper (`uur-pw-capture`) which runs the PipeWire stream and
//! publishes frames into a private XDG runtime file (see
//! docs/capture-protocol.md).
//!
//! Consent: the first run shows the compositor's share dialog once.  The
//! returned restore token is stored in uur's config; every later run hands
//! it back with PersistMode::Persistent and the portal restores the choice
//! silently — no dialog, no local interaction, which is the whole point on
//! a headless remote host.

use anyhow::{anyhow, Context, Result};
use futures::{FutureExt, StreamExt};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::Config;

type PortalSession =
    ashpd::desktop::Session<'static, ashpd::desktop::screencast::Screencast<'static>>;

struct PortalCapture {
    fd: OwnedFd,
    node: u32,
    restore_token: Option<String>,
    session: PortalSession,
    closed: zbus::proxy::SignalStream<'static>,
}

/// Run the capture path until interrupted.
pub fn run_standalone() -> Result<()> {
    // Public capture uses the same lifecycle lock as run/setup so stop and
    // uninstall can also shut down a diagnostic ScreenCast session. Internal
    // capture already runs under the supervisor's lock.
    let _session_lock = crate::session::acquire_session_lock()?;
    run()
}

pub fn run() -> Result<()> {
    let mut config = Config::load()?;
    let stopping = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, stopping.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stopping.clone())?;
    if config.capture_backend == "wayland" {
        if config.capture_source != "monitor" {
            anyhow::bail!("direct Wayland capture requires a monitor source");
        }
        let mut command = Command::new(find_helper("uur-wl-capture")?);
        command.env("UUR_FRAME_PATH", frame_path()?);
        if let Some(output) = &config.capture_output {
            command.env("UUR_CAPTURE_OUTPUT", output);
        }
        let mut helper = CaptureHelper(command.spawn().context("starting direct Wayland capture")?);
        println!("{}", t!("capture.direct_started"));
        while !stopping.load(Ordering::Relaxed) {
            if let Some(status) = helper.0.try_wait()? {
                if !status.success() {
                    anyhow::bail!("direct Wayland capture failed ({status})");
                }
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        return Ok(());
    }
    if config.capture_backend != "portal" {
        anyhow::bail!("unknown capture backend {:?}", config.capture_backend);
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("creating capture runtime")?;
    // The portal's D-Bus connection runs on this runtime. Keep it running
    // throughout capture, including on capture-only compositors such as niri.
    runtime.block_on(async {
        let frames = frame_path()?;
        let mut failures = 0;
        let mut initial_session = true;
        let mut display_changes = watch_display_changes().await;
        loop {
            let opened = tokio::select! {
                result = portal_session(&mut config, initial_session) => result,
                _ = wait_for_stop(&stopping) => return Ok(()),
            };
            let PortalCapture {
                fd: pw_fd,
                node,
                session,
                mut closed,
                ..
            } = match opened {
                Ok(opened) => opened,
                Err(error) if !initial_session && failures < 3 => {
                    failures += 1;
                    eprintln!("capture reconnect {failures}/3 failed: {error:#}");
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(1)) => continue,
                        _ = wait_for_stop(&stopping) => return Ok(()),
                    }
                }
                Err(error) => return Err(error),
            };
            initial_session = false;
            println!("{}", t!("capture.session_ready", node = node));
            match std::fs::remove_file(&frames) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("retiring the previous capture transport"),
            }
            let mut helper = CaptureHelper(spawn_helper(pw_fd, node)?);
            let mut healthy_since = None;
            let mut portal_closed = false;
            let mut reconfigure = false;
            let status = loop {
                tokio::select! {
                    biased;
                    _ = wait_for_stop(&stopping) => break None,
                    _ = closed.next() => {
                        // Mutter can close the old stream before forwarding
                        // MonitorsChanged. Correlate that layout transaction;
                        // a user Stop without a display change stays closed.
                        reconfigure = tokio::time::timeout(
                            Duration::from_millis(250),
                            next_display_change(&mut display_changes),
                        ).await.is_ok();
                        portal_closed = !reconfigure;
                        break None;
                    }
                    _ = next_display_change(&mut display_changes) => {
                        reconfigure = true;
                        break None;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(50)) => {
                        if has_frame(&frames) && healthy_since.is_none() {
                            healthy_since = Some(Instant::now());
                        }
                        if let Some(status) = helper.0.try_wait()
                            .context("waiting for capture helper")? {
                            break Some(status);
                        }
                    }
                }
            };
            drop(closed);
            drop(helper);
            let _ = tokio::time::timeout(Duration::from_secs(2), session.close()).await;
            if stopping.load(Ordering::Relaxed) {
                return Ok(());
            }
            // Closed without a matching layout change means Stop sharing.
            // Never silently restore that permission or use direct capture.
            if portal_closed {
                anyhow::bail!(
                    "screen sharing was closed by the portal; restart uur run to share again"
                );
            }
            if healthy_since.is_some_and(|since| since.elapsed() >= Duration::from_secs(30)) {
                failures = 0;
            }
            if reconfigure {
                eprintln!("display configuration changed; restoring capture");
            } else {
                failures += 1;
                if failures > 3 {
                    anyhow::bail!(
                        "capture recovery failed repeatedly; restart uur run to share again"
                    );
                }
                let status = status.context("capture helper stopped without an exit status")?;
                if !matches!(status.code(), Some(0 | 75) | None) {
                    anyhow::bail!(
                        "{} ({status}); capture recovery stopped",
                        t!("capture.helper_failed")
                    );
                }
                eprintln!("capture stream interrupted ({status}); reconnecting {failures}/3");
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(1)) => {},
                _ = wait_for_stop(&stopping) => return Ok(()),
            }
            // Several signals can describe one layout transaction. The next
            // handshake reads the final layout rather than replaying each one.
            if reconfigure {
                while let Some(stream) = display_changes.as_mut() {
                    match stream.next().now_or_never() {
                        Some(Some(_)) => {}
                        Some(None) => {
                            display_changes = None;
                            break;
                        }
                        None => break,
                    }
                }
            }
        }
    })
}

async fn watch_display_changes() -> Option<zbus::proxy::SignalStream<'static>> {
    let connection = zbus::Connection::session().await.ok()?;
    let bus = zbus::fdo::DBusProxy::new(&connection).await.ok()?;
    let name = "org.gnome.Mutter.DisplayConfig".try_into().ok()?;
    if !bus.name_has_owner(name).await.ok()? {
        return None;
    }
    drop(bus);
    let proxy = zbus::Proxy::new_owned(
        connection,
        "org.gnome.Mutter.DisplayConfig",
        "/org/gnome/Mutter/DisplayConfig",
        "org.gnome.Mutter.DisplayConfig",
    )
    .await
    .ok()?;
    proxy.receive_signal("MonitorsChanged").await.ok()
}

async fn next_display_change(changes: &mut Option<zbus::proxy::SignalStream<'static>>) {
    loop {
        match changes.as_mut() {
            Some(stream) => {
                if stream.next().await.is_some() {
                    return;
                }
                *changes = None;
            }
            None => std::future::pending::<()>().await,
        }
    }
}

async fn wait_for_stop(stopping: &AtomicBool) {
    while !stopping.load(Ordering::Relaxed) {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

struct CaptureHelper(Child);

impl Drop for CaptureHelper {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_some() {
            return;
        }
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        for _ in 0..100 {
            if self.0.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) fn has_frame(path: &Path) -> bool {
    let mut header = [0u8; 64];
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    let field = |offset| u32::from_le_bytes(header[offset..offset + 4].try_into().unwrap());
    field(0) == 0x4652_5555
        && field(4) == 1
        && field(8) > 0
        && field(12) > 0
        && field(16) >= field(8).saturating_mul(4)
        && field(20) == 1
        && u64::from_le_bytes(header[24..32].try_into().unwrap()) > 0
}

/// Start the native PipeWire consumer with a portal-owned descriptor. The
/// descriptor remains private to the child and is never reopened by path.
pub(crate) fn spawn_helper(pw_fd: OwnedFd, node: u32) -> Result<Child> {
    // The helper inherits this descriptor: strip close-on-exec and keep the
    // OwnedFd alive across the spawn (closing it would invalidate the number
    // the child receives).
    let raw = pw_fd.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFD);
        if flags < 0 {
            return Err(std::io::Error::last_os_error()).context("fcntl F_GETFD");
        }
        if libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
            return Err(std::io::Error::last_os_error()).context("fcntl F_SETFD");
        }
    }

    let helper = find_helper("uur-pw-capture")?;
    let mut command = Command::new(&helper);
    command
        .env("UUR_PW_FD", raw.to_string())
        .env("UUR_PW_NODE", node.to_string())
        .env("UUR_FRAME_PATH", frame_path()?);
    if let Ok(debug) = std::env::var("UUR_PW_DEBUG") {
        command.env("PIPEWIRE_DEBUG", debug);
    }
    let child = command
        .spawn()
        .with_context(|| format!("running {}", helper.display()))?;
    Ok(child)
}

fn frame_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("UUR_FRAME_PATH") {
        return Ok(PathBuf::from(path));
    }
    Ok(crate::config::runtime_dir()?.join("frames.v1"))
}

/// Open the portal session and return (inherited fd number, node id).
/// First attempt reuses the stored restore token; if that fails (stale
/// token, monitor changed, compositor restart) fall back to one
/// interactive attempt and store the fresh token.
async fn portal_session(config: &mut Config, interactive: bool) -> Result<PortalCapture> {
    let saved_token = config.capture_restore_token.clone();
    let screencast = ashpd::desktop::screencast::Screencast::new()
        .await
        .context("connecting to the ScreenCast portal")?;

    let mut last_error = None;
    let source = source_type(&config.capture_source);
    for token in saved_token
        .clone()
        .map(Some)
        .into_iter()
        .chain(interactive.then_some(None))
    {
        match negotiate(&screencast, source, token.as_deref()).await {
            Ok(capture) => {
                if capture.restore_token != saved_token {
                    config.capture_restore_token = capture.restore_token.clone();
                    config.store().context("storing the new restore token")?;
                }
                return Ok(capture);
            }
            Err(error) => {
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| {
        anyhow!("capture permission could not be restored; restart uur run to share again")
    }))
}

/// Choose a cursor mode from the flags the portal advertises.
///
/// `Embedded` is what the capture path wants (the hook composites the cursor
/// itself on X11, and an embedded cursor is the closest match elsewhere), then
/// `Hidden`, then `Metadata`.  `None` means the portal advertised no usable
/// mode at all.
fn choose_cursor_mode(
    available: ashpd::enumflags2::BitFlags<ashpd::desktop::screencast::CursorMode>,
) -> Option<ashpd::desktop::screencast::CursorMode> {
    use ashpd::desktop::screencast::CursorMode;
    [
        CursorMode::Embedded,
        CursorMode::Hidden,
        CursorMode::Metadata,
    ]
    .into_iter()
    .find(|mode| available.contains(*mode))
}

/// Pick a cursor mode the portal actually advertises.
///
/// `SelectSources` rejects a mode outside the portal's `AvailableCursorModes`
/// with `InvalidArgument: Unavailable cursor mode N`.  A portal frontend whose
/// backend property cache is empty reports a zeroed set and rejects every
/// mode, so choosing from the advertised flags keeps capture working on
/// backends that offer only a subset and turns the unusable case into an
/// actionable message instead of a raw D-Bus error.
pub(crate) async fn preferred_cursor_mode(
    screencast: &ashpd::desktop::screencast::Screencast<'_>,
) -> Result<ashpd::desktop::screencast::CursorMode> {
    use ashpd::desktop::screencast::CursorMode;

    let available = match screencast.available_cursor_modes().await {
        Ok(modes) => modes,
        // ScreenCast v2 introduced the property; older portals have no
        // advertisement to consult, so keep the historical request.
        Err(ashpd::Error::RequiresVersion(2, 0..=1)) => return Ok(CursorMode::Embedded),
        Err(error) => return Err(error).context("reading ScreenCast cursor modes"),
    };
    let mode = choose_cursor_mode(available)
        .ok_or_else(|| anyhow!("{}", t!("capture.cursor_unsupported")))?;
    if mode != CursorMode::Embedded {
        // The helper publishes pixels, not PipeWire cursor metadata.
        eprintln!("{}", t!("capture.cursor_not_embedded"));
    }
    Ok(mode)
}

/// One full portal handshake.  Returns the PipeWire fd, the video node id
/// and the restore token the portal issued for this selection.
async fn negotiate(
    screencast: &ashpd::desktop::screencast::Screencast<'static>,
    source: ashpd::desktop::screencast::SourceType,
    restore_token: Option<&str>,
) -> Result<PortalCapture> {
    use ashpd::desktop::PersistMode;

    let session = screencast.create_session().await?;
    // Session.Closed carries a{sv}. ashpd 0.10 deserializes it as (), which
    // silently drops the signal. Subscribe on the SAME connection (the
    // frontend unicasts it) before starting, and ignore its optional details.
    let serialized = serde_json::to_value(&session)?;
    let path = serialized
        .as_str()
        .context("invalid portal session path")?
        .to_owned();
    let session_proxy = zbus::Proxy::new_owned(
        screencast.connection().clone(),
        "org.freedesktop.portal.Desktop",
        path,
        "org.freedesktop.portal.Session",
    )
    .await?;
    let closed = session_proxy.receive_signal("Closed").await?;
    let cursor_mode = preferred_cursor_mode(screencast).await?;

    screencast
        .select_sources(
            &session,
            cursor_mode,
            source.into(),
            false,
            restore_token,
            PersistMode::ExplicitlyRevoked,
        )
        .await?
        .response()?;

    let response = screencast.start(&session, None).await?;
    let streams_response = response.response()?;
    let streams = streams_response.streams();
    let stream = streams.first().context("no monitor selected")?;
    let node = stream.pipe_wire_node_id();
    let fresh_token = streams_response.restore_token().map(String::from);

    let fd = screencast
        .open_pipe_wire_remote(&session)
        .await
        .context("opening the PipeWire remote")?;
    Ok(PortalCapture {
        fd,
        node,
        restore_token: fresh_token,
        session,
        closed,
    })
}

pub(crate) fn source_type(value: &str) -> ashpd::desktop::screencast::SourceType {
    use ashpd::desktop::screencast::SourceType;
    match value {
        "window" => SourceType::Window,
        "virtual" => SourceType::Virtual,
        _ => SourceType::Monitor,
    }
}

pub fn list_wayland_outputs() -> Result<()> {
    let status = Command::new(find_helper("uur-wl-capture")?)
        .arg("--list-outputs")
        .status()?;
    anyhow::ensure!(status.success(), "could not enumerate Wayland outputs");
    Ok(())
}

fn find_helper(name: &str) -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        // dev checkout: target/debug/uur -> capture/uur-pw-capture
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("../lib/uur").join(name));
            candidates.push(dir.join("../../capture").join(name));
            candidates.push(dir.join(name));
        }
    }
    candidates.push(PathBuf::from("/usr/lib/uur").join(name));
    candidates.push(PathBuf::from("/usr/local/lib/uur").join(name));
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(home).join(".local/lib/uur").join(name));
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("capture").join(name));
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow!("capture helper {name} not found (build with capture/build.sh)"))
}

#[cfg(test)]
mod tests {
    use super::choose_cursor_mode;
    use super::source_type;
    use ashpd::desktop::screencast::{CursorMode, SourceType};
    use ashpd::enumflags2::BitFlags;

    #[test]
    fn configured_portal_source_is_mapped() {
        assert_eq!(source_type("monitor"), SourceType::Monitor);
        assert_eq!(source_type("window"), SourceType::Window);
        assert_eq!(source_type("virtual"), SourceType::Virtual);
    }

    #[test]
    fn embedded_cursor_is_preferred_when_the_portal_offers_it() {
        // gnome advertises 7 = Hidden | Embedded | Metadata.
        let available = CursorMode::Hidden | CursorMode::Embedded | CursorMode::Metadata;
        assert_eq!(choose_cursor_mode(available), Some(CursorMode::Embedded));
    }

    #[test]
    fn a_backend_without_embedded_cursor_falls_back_instead_of_failing() {
        // wlr advertises 3 = Hidden | Embedded; a metadata-only backend must
        // still capture rather than requesting a mode the portal rejects.
        assert_eq!(
            choose_cursor_mode(CursorMode::Metadata.into()),
            Some(CursorMode::Metadata)
        );
        assert_eq!(
            choose_cursor_mode(CursorMode::Hidden.into()),
            Some(CursorMode::Hidden)
        );
        assert_eq!(
            choose_cursor_mode(CursorMode::Hidden | CursorMode::Metadata),
            Some(CursorMode::Hidden)
        );
    }

    #[test]
    fn an_empty_advertisement_selects_no_mode() {
        // Regression: a portal frontend with a zeroed backend property cache
        // reported AvailableCursorModes = 0 and rejected every request with
        // "InvalidArgument: Unavailable cursor mode 2".
        assert_eq!(choose_cursor_mode(BitFlags::empty()), None);
    }
}
