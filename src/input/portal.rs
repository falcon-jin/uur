use anyhow::{anyhow, Context, Result};
use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop};
use ashpd::desktop::screencast::Screencast;
use ashpd::desktop::PersistMode;
use futures::StreamExt;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use super::InputBackend;

type Reply = SyncSender<std::result::Result<(), String>>;

enum Event {
    Key(i32, bool, Reply),
    Button(i32, bool, Reply),
    FlushMotion,
    Wheel(Axis, i32, Reply),
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Motion {
    absolute: bool,
    x: f64,
    y: f64,
}

/// Select a consented stream in the same logical layout used by the compositor.
/// A point in a monitor gap is clamped to the closest selected monitor.
fn absolute_target(
    streams: &[crate::capture::CaptureStream],
    x: f64,
    y: f64,
) -> Result<(u32, f64, f64)> {
    anyhow::ensure!(
        x.is_finite() && y.is_finite(),
        "invalid absolute pointer position"
    );
    anyhow::ensure!(
        !streams.is_empty() && streams.len() <= 16,
        "invalid input stream count"
    );
    let mut streams = streams.to_vec();
    if streams
        .iter()
        .any(|stream| stream.width == 0 || stream.height == 0)
    {
        use std::io::Read;
        let mut header = [0u8; 704];
        std::fs::File::open(crate::capture::frame_path()?)?.read_exact(&mut header)?;
        let field = |offset| u32::from_le_bytes(header[offset..offset + 4].try_into().unwrap());
        anyhow::ensure!(
            field(0) == 0x4652_5555
                && field(4) == 3
                && field(36) as usize == streams.len()
                && u64::from_le_bytes(header[24..32].try_into().unwrap()) > 0,
            "capture layout is not ready for pointer routing"
        );
        for (index, stream) in streams.iter_mut().enumerate() {
            let offset = 64 + index * 40;
            stream.x = field(offset) as i32;
            stream.y = field(offset + 4) as i32;
            stream.width = field(offset + 8);
            stream.height = field(offset + 12);
        }
    }
    anyhow::ensure!(
        streams
            .iter()
            .all(|stream| stream.width > 0 && stream.height > 0),
        "missing pointer stream dimensions"
    );
    if streams
        .iter()
        .any(|stream| stream.x == i32::MIN || stream.y == i32::MIN)
    {
        let mut next_x = 0i64;
        for stream in &mut streams {
            stream.x = i32::try_from(next_x).context("input stream layout is too wide")?;
            stream.y = 0;
            next_x += i64::from(stream.width);
        }
    }
    let left = streams
        .iter()
        .map(|stream| i64::from(stream.x))
        .min()
        .unwrap();
    let top = streams
        .iter()
        .map(|stream| i64::from(stream.y))
        .min()
        .unwrap();
    let right = streams
        .iter()
        .map(|stream| i64::from(stream.x) + i64::from(stream.width))
        .max()
        .unwrap();
    let bottom = streams
        .iter()
        .map(|stream| i64::from(stream.y) + i64::from(stream.height))
        .max()
        .unwrap();
    let global_x = left as f64 + x.clamp(0.0, 65535.0) * (right - left - 1) as f64 / 65535.0;
    let global_y = top as f64 + y.clamp(0.0, 65535.0) * (bottom - top - 1) as f64 / 65535.0;
    let mut best = None;
    let mut distance = f64::INFINITY;
    for stream in &streams {
        let local_x = (global_x - f64::from(stream.x)).clamp(0.0, f64::from(stream.width - 1));
        let local_y = (global_y - f64::from(stream.y)).clamp(0.0, f64::from(stream.height - 1));
        let dx = global_x - f64::from(stream.x) - local_x;
        let dy = global_y - f64::from(stream.y) - local_y;
        let current = dx * dx + dy * dy;
        // The producer draws later streams last when rectangles overlap.
        if current <= distance {
            best = Some((stream.node, local_x, local_y));
            distance = current;
        }
    }
    best.context("no input stream matches the pointer position")
}

use crate::capture::CaptureHelper as CaptureGuard;

/// Wayland input through the freedesktop RemoteDesktop portal. The actual
/// backend can be GNOME, KDE, COSMIC, or any future implementation; uur never
/// branches on a desktop name.
pub struct PortalBackend {
    sender: UnboundedSender<Event>,
    pending_motion: Arc<Mutex<Option<Motion>>>,
    motion_queued: Arc<AtomicBool>,
    held_keys: HashSet<i32>,
    held_buttons: HashSet<i32>,
    failure: Arc<Mutex<Option<String>>>,
}

impl PortalBackend {
    pub fn connect() -> Result<Self> {
        let mut config = crate::config::Config::load()?;
        let (sender, receiver) = unbounded_channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let pending_motion = Arc::new(Mutex::new(None));
        let motion_queued = Arc::new(AtomicBool::new(false));
        let worker_motion = pending_motion.clone();
        let worker_queued = motion_queued.clone();
        let failure = Arc::new(Mutex::new(None));
        let worker_failure = failure.clone();
        let worker_config = config.clone();

        std::thread::Builder::new()
            .name("uur-portal-input".into())
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(anyhow::Error::from)
                    .and_then(|runtime| {
                        runtime.block_on(portal_worker(
                            receiver,
                            ready_sender.clone(),
                            worker_config,
                            worker_motion,
                            worker_queued,
                            worker_failure.clone(),
                        ))
                    });
                if let Err(error) = result {
                    *worker_failure.lock().unwrap() = Some(error.to_string());
                    eprintln!("portal input worker stopped: {error:#}");
                    let _ = ready_sender.send(Err(error.to_string()));
                }
            })
            .context("starting portal input worker")?;

        let fresh_token = ready_receiver
            .recv()
            .context("portal input worker stopped during setup")?
            .map_err(|error| anyhow!(error))?;
        if fresh_token != config.remote_desktop_restore_token {
            config.remote_desktop_restore_token = fresh_token;
            crate::config::Config::update(|stored| {
                if stored.capture_source == config.capture_source
                    && stored.capture_multiple == config.capture_multiple
                    && stored.capture_backend == config.capture_backend
                {
                    stored.remote_desktop_restore_token =
                        config.remote_desktop_restore_token.clone();
                }
            })?;
        }

        Ok(Self {
            sender,
            pending_motion,
            motion_queued,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            failure,
        })
    }

    fn submit(&self, event: impl FnOnce(Reply) -> Event) -> Result<()> {
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        self.sender
            .send(event(reply_sender))
            .map_err(|_| anyhow!("portal input session closed"))?;
        let result = reply_receiver
            .recv_timeout(Duration::from_secs(3))
            .context("portal input request timed out")
            .and_then(|result| result.map_err(|error| anyhow!(error)));
        if let Err(error) = &result {
            *self.failure.lock().unwrap() = Some(error.to_string());
        }
        result
    }
}

async fn portal_worker(
    receiver: UnboundedReceiver<Event>,
    ready: SyncSender<std::result::Result<Option<String>, String>>,
    configured: crate::config::Config,
    pending_motion: Arc<Mutex<Option<Motion>>>,
    motion_queued: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
) -> Result<()> {
    let mut worker = PortalWorker {
        restore_token: configured.remote_desktop_restore_token.clone(),
        configured,
        receiver,
        ready,
        pending_motion,
        motion_queued,
        failure,
        initial: true,
        opened: false,
        held_keys: HashSet::new(),
        held_buttons: HashSet::new(),
    };
    let mut display_changes = crate::capture::watch_display_changes().await;
    let mut failures = 0u8;
    loop {
        worker.opened = false;
        match portal_session_worker(&mut worker, &mut display_changes).await {
            Ok(SessionExit::Shutdown) => return Ok(()),
            Ok(SessionExit::Reconnect { layout, healthy }) => {
                if healthy {
                    failures = 0;
                }
                if !layout {
                    failures += 1;
                }
                anyhow::ensure!(failures <= 3, "portal capture recovery exhausted");
                anyhow::ensure!(worker.restore_token.is_some(),
                    "portal capture changed without a restorable grant; restart uur run to share again");
                eprintln!(
                    "restoring the consented RemoteDesktop session after {}",
                    if layout {
                        "a display change"
                    } else {
                        "a capture failure"
                    }
                );
                tokio::time::sleep(Duration::from_millis(if layout { 250 } else { 1000 })).await;
                while tokio::time::timeout(
                    Duration::from_millis(100),
                    crate::capture::next_display_change(&mut display_changes),
                )
                .await
                .is_ok()
                {}
            }
            Err(error)
                if !worker.initial
                    && !worker.opened
                    && failures < 3
                    && worker.restore_token.is_some()
                    && !crate::capture::user_cancelled(&error) =>
            {
                failures += 1;
                eprintln!("portal restore {failures}/3 failed: {error:#}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

struct PortalWorker {
    receiver: UnboundedReceiver<Event>,
    ready: SyncSender<std::result::Result<Option<String>, String>>,
    configured: crate::config::Config,
    restore_token: Option<String>,
    pending_motion: Arc<Mutex<Option<Motion>>>,
    motion_queued: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    initial: bool,
    opened: bool,
    held_keys: HashSet<i32>,
    held_buttons: HashSet<i32>,
}

enum SessionExit {
    Shutdown,
    Reconnect { layout: bool, healthy: bool },
}

async fn portal_session_worker(
    worker: &mut PortalWorker,
    display_changes: &mut Option<zbus::proxy::SignalStream<'static>>,
) -> Result<SessionExit> {
    let PortalWorker {
        receiver,
        ready,
        configured,
        restore_token,
        pending_motion,
        motion_queued,
        failure,
        initial,
        opened,
        held_keys,
        held_buttons,
    } = worker;
    let portal = RemoteDesktop::new().await?;
    let screencast = Screencast::new().await?;
    let source = crate::capture::source_type(&configured.capture_source);
    let available = portal.available_device_types().await?;
    if !available.contains(DeviceType::Keyboard) || !available.contains(DeviceType::Pointer) {
        anyhow::bail!("RemoteDesktop portal does not provide keyboard and pointer devices");
    }

    let session = portal.create_session().await?;
    let serialized = serde_json::to_value(&session)?;
    let session_proxy = zbus::Proxy::new_owned(
        portal.connection().clone(),
        "org.freedesktop.portal.Desktop",
        serialized
            .as_str()
            .context("invalid portal session path")?
            .to_owned(),
        "org.freedesktop.portal.Session",
    )
    .await?;
    let mut closed = session_proxy.receive_signal("Closed").await?;
    portal
        .select_devices(
            &session,
            DeviceType::Keyboard | DeviceType::Pointer,
            restore_token.as_deref(),
            PersistMode::ExplicitlyRevoked,
        )
        .await?
        .response()?;
    screencast
        .select_sources(
            &session,
            crate::capture::preferred_cursor_mode(&screencast).await?,
            source.into(),
            configured.capture_multiple && configured.capture_source == "monitor",
            None,
            PersistMode::DoNot,
        )
        .await?
        .response()?;
    let selection = portal.start(&session, None).await?.response()?;
    *opened = true;
    anyhow::ensure!(
        selection.devices().contains(DeviceType::Keyboard)
            && selection.devices().contains(DeviceType::Pointer),
        "RemoteDesktop grant did not include keyboard and pointer"
    );
    let selected_streams = selection
        .streams()
        .context("RemoteDesktop portal returned no screen streams")?;
    anyhow::ensure!(
        !selected_streams.is_empty(),
        "RemoteDesktop portal returned no screen streams"
    );
    let streams = selected_streams
        .iter()
        .map(|stream| {
            let (x, y) = stream.position().unwrap_or((i32::MIN, i32::MIN));
            let (width, height) = stream.size().unwrap_or((0, 0));
            crate::capture::CaptureStream {
                node: stream.pipe_wire_node_id(),
                x,
                y,
                width: u32::try_from(width).unwrap_or(0),
                height: u32::try_from(height).unwrap_or(0),
            }
        })
        .collect::<Vec<_>>();
    let pipewire = screencast.open_pipe_wire_remote(&session).await?;
    let mut capture = CaptureGuard(crate::capture::spawn_helper(pipewire, &streams)?);
    let token = selection.restore_token().map(ToOwned::to_owned);
    *restore_token = token.clone();
    if *initial {
        ready
            .send(Ok(token))
            .map_err(|_| anyhow!("portal setup receiver closed"))?;
        *initial = false;
    } else {
        crate::config::Config::update(|stored| {
            if stored.capture_source == configured.capture_source
                && stored.capture_multiple == configured.capture_multiple
                && stored.capture_backend == configured.capture_backend
            {
                stored.remote_desktop_restore_token = token;
            }
        })?;
        for key in held_keys.iter().copied() {
            portal
                .notify_keyboard_keycode(&session, key, KeyState::Pressed)
                .await?;
        }
        for button in held_buttons.iter().copied() {
            portal
                .notify_pointer_button(&session, button, KeyState::Pressed)
                .await?;
        }
    }
    let mut healthy_since = None;
    let mut health_tick = tokio::time::interval(Duration::from_millis(50));
    let outcome: Result<SessionExit> = async {
    loop {
        if let Some(error) = failure.lock().unwrap().clone() {
            anyhow::bail!("portal input session failed: {error}");
        }
        let event = tokio::select! {
            biased;
            _ = closed.next() => {
                let layout = tokio::time::timeout(Duration::from_millis(250),
                    crate::capture::next_display_change(display_changes)).await.is_ok();
                anyhow::ensure!(layout, "screen sharing was closed by the portal; restart uur run to share again");
                break Ok(SessionExit::Reconnect { layout: true,
                    healthy: healthy_since.is_some_and(|since: Instant| since.elapsed() >= Duration::from_secs(30)) });
            },
            _ = crate::capture::next_display_change(display_changes) => {
                break Ok(SessionExit::Reconnect { layout: true,
                    healthy: healthy_since.is_some_and(|since: Instant| since.elapsed() >= Duration::from_secs(30)) });
            },
            _ = health_tick.tick() => {
                if crate::capture::helper_stalled(&crate::capture::frame_path()?) {
                    eprintln!("portal capture helper heartbeat expired; reconnecting");
                    break Ok(SessionExit::Reconnect { layout: false,
                        healthy: healthy_since.is_some_and(|since: Instant| since.elapsed() >= Duration::from_secs(30)) });
                }
                if crate::capture::has_frame(&crate::capture::frame_path()?) && healthy_since.is_none() {
                    healthy_since = Some(Instant::now());
                }
                if let Some(status) = capture.0.try_wait()? {
                    anyhow::ensure!(status.code() == Some(75), "portal capture helper exited ({status})");
                    break Ok(SessionExit::Reconnect { layout: false,
                        healthy: healthy_since.is_some_and(|since: Instant| since.elapsed() >= Duration::from_secs(30)) });
                }
                continue;
            },
            event = receiver.recv() => match event {
                Some(event) => event,
                None => break Ok(SessionExit::Shutdown),
            },
        };
        match event {
            Event::Key(code, down, reply) => {
                let result = portal
                    .notify_keyboard_keycode(
                        &session,
                        code,
                        if down {
                            KeyState::Pressed
                        } else {
                            KeyState::Released
                        },
                    )
                    .await;
                if result.is_ok() {
                    if down { held_keys.insert(code); } else { held_keys.remove(&code); }
                }
                answer(reply, result)?;
            }
            Event::Button(button, down, reply) => {
                let result = portal
                    .notify_pointer_button(
                        &session,
                        button,
                        if down {
                            KeyState::Pressed
                        } else {
                            KeyState::Released
                        },
                    )
                    .await;
                if result.is_ok() {
                    if down { held_buttons.insert(button); } else { held_buttons.remove(&button); }
                }
                answer(reply, result)?;
            }
            Event::FlushMotion => {
                motion_queued.store(false, Ordering::Release);
                let motion = pending_motion.lock().unwrap().take();
                let Some(motion) = motion else {
                    continue;
                };
                let result = if motion.absolute {
                    let (stream_id, logical_x, logical_y) = match absolute_target(&streams, motion.x, motion.y) {
                        Ok(target) => target,
                        Err(error) => { eprintln!("portal pointer layout not ready: {error}"); continue; }
                    };
                    portal
                        .notify_pointer_motion_absolute(&session, stream_id, logical_x, logical_y)
                        .await
                } else {
                    portal
                        .notify_pointer_motion(&session, motion.x, motion.y)
                        .await
                };
                if let Err(error) = result {
                    eprintln!("portal pointer motion failed: {error}");
                }
            },
            Event::Wheel(axis, steps, reply) => {
                answer(
                    reply,
                    portal
                        .notify_pointer_axis_discrete(&session, axis, steps)
                        .await,
                )?;
            }
            Event::Shutdown => break Ok(SessionExit::Shutdown),
        }
    }
    }.await;
    drop(closed);
    drop(capture);
    let _ = tokio::time::timeout(Duration::from_secs(2), session.close()).await;
    outcome
}

fn answer<T>(reply: Reply, result: std::result::Result<T, ashpd::Error>) -> Result<()> {
    let failure = result.as_ref().err().map(ToString::to_string);
    let _ = reply.send(result.map(|_| ()).map_err(|error| error.to_string()));
    failure.map_or(Ok(()), |error| Err(anyhow!(error)))
}

impl InputBackend for PortalBackend {
    fn name(&self) -> &'static str {
        "portal"
    }

    fn check_health(&self) -> Result<()> {
        if let Some(error) = self.failure.lock().unwrap().clone() {
            anyhow::bail!("portal input worker failed: {error}");
        }
        anyhow::ensure!(!self.sender.is_closed(), "portal input worker stopped");
        Ok(())
    }

    fn key(&mut self, vkey: u16, down: bool) -> Result<()> {
        let code = super::uinput::vk_to_linux_key(vkey) as i32;
        if code == 0 {
            anyhow::bail!("Windows virtual key 0x{vkey:02x} has no evdev mapping");
        }
        self.submit(|reply| Event::Key(code, down, reply))?;
        if down {
            self.held_keys.insert(code);
        } else {
            self.held_keys.remove(&code);
        }
        Ok(())
    }

    fn button(&mut self, button: u16, down: bool) -> Result<()> {
        let evdev = match button {
            1 => input_linux_sys::BTN_LEFT,
            2 => input_linux_sys::BTN_RIGHT,
            3 => input_linux_sys::BTN_MIDDLE,
            8 => input_linux_sys::BTN_SIDE,
            9 => input_linux_sys::BTN_EXTRA,
            _ => anyhow::bail!("unsupported pointer button {button}"),
        } as i32;
        self.submit(|reply| Event::Button(evdev, down, reply))?;
        if down {
            self.held_buttons.insert(evdev);
        } else {
            self.held_buttons.remove(&evdev);
        }
        Ok(())
    }

    fn motion(&mut self, absolute: bool, x: i32, y: i32) -> Result<()> {
        let mut pending = self.pending_motion.lock().unwrap();
        merge_motion(
            &mut pending,
            Motion {
                absolute,
                x: x as f64,
                y: y as f64,
            },
        );
        drop(pending);
        if !self.motion_queued.swap(true, Ordering::AcqRel)
            && self.sender.send(Event::FlushMotion).is_err()
        {
            self.motion_queued.store(false, Ordering::Release);
            anyhow::bail!("portal input session closed");
        }
        Ok(())
    }

    fn wheel(&mut self, horizontal: bool, delta: i32) -> Result<()> {
        let axis = if horizontal {
            Axis::Horizontal
        } else {
            Axis::Vertical
        };
        let steps = super::wheel_steps(delta);
        self.submit(|reply| Event::Wheel(axis, steps, reply))
    }

    fn release_all(&mut self) -> Result<()> {
        self.pending_motion.lock().unwrap().take();
        for key in self.held_keys.clone() {
            let _ = self.submit(|reply| Event::Key(key, false, reply));
        }
        self.held_keys.clear();
        for button in self.held_buttons.clone() {
            let _ = self.submit(|reply| Event::Button(button, false, reply));
        }
        self.held_buttons.clear();
        Ok(())
    }
}

fn merge_motion(pending: &mut Option<Motion>, next: Motion) {
    if !next.absolute {
        if let Some(current) = pending.as_mut().filter(|motion| !motion.absolute) {
            current.x += next.x;
            current.y += next.y;
            return;
        }
    }
    *pending = Some(next);
}

impl Drop for PortalBackend {
    fn drop(&mut self) {
        let _ = self.release_all();
        let _ = self.sender.send(Event::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::{absolute_target, merge_motion, Motion};

    #[test]
    fn absolute_motion_keeps_only_the_latest_position() {
        let mut pending = Some(Motion {
            absolute: true,
            x: 10.0,
            y: 20.0,
        });
        let latest = Motion {
            absolute: true,
            x: 30.0,
            y: 40.0,
        };
        merge_motion(&mut pending, latest);
        assert_eq!(pending, Some(latest));
        let streams = [
            crate::capture::CaptureStream {
                node: 41,
                x: -1280,
                y: 0,
                width: 1280,
                height: 720,
            },
            crate::capture::CaptureStream {
                node: 42,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        ];
        assert_eq!(absolute_target(&streams, 0.0, 0.0).unwrap(), (41, 0.0, 0.0));
        assert_eq!(
            absolute_target(&streams, 65535.0, 65535.0).unwrap(),
            (42, 1919.0, 1079.0)
        );
        let target = absolute_target(&streams, 1280.0 * 65535.0 / 3199.0, 0.0).unwrap();
        assert_eq!(target.0, 42);
        assert!(target.1.abs() < 1e-6);
        assert!(absolute_target(&streams, f64::NAN, 0.0).is_err());
        let mut gap = streams;
        gap[1].x = 200;
        let target = absolute_target(&gap, 1280.0 * 65535.0 / 3399.0, 0.0).unwrap();
        assert_eq!(target, (41, 1279.0, 0.0));
    }

    #[test]
    fn relative_motion_accumulates_without_building_a_queue() {
        let mut pending = Some(Motion {
            absolute: false,
            x: 3.0,
            y: -2.0,
        });
        merge_motion(
            &mut pending,
            Motion {
                absolute: false,
                x: 4.0,
                y: 5.0,
            },
        );
        assert_eq!(
            pending,
            Some(Motion {
                absolute: false,
                x: 7.0,
                y: 3.0,
            })
        );
    }
}
