use anyhow::{anyhow, Context, Result};
use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop};
use ashpd::desktop::screencast::Screencast;
use ashpd::desktop::PersistMode;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
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

struct CaptureGuard(std::process::Child);

impl Drop for CaptureGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Wayland input through the freedesktop RemoteDesktop portal. The actual
/// backend can be GNOME, KDE, COSMIC, or any future implementation; uur never
/// branches on a desktop name.
pub struct PortalBackend {
    sender: UnboundedSender<Event>,
    pending_motion: Arc<Mutex<Option<Motion>>>,
    motion_queued: Arc<AtomicBool>,
    held_keys: HashSet<i32>,
    held_buttons: HashSet<i32>,
}

impl PortalBackend {
    pub fn connect() -> Result<Self> {
        let mut config = crate::config::Config::load()?;
        if config.capture_source == "monitor" && !config.remote_desktop_multi_select_migrated {
            // Older RemoteDesktop restore tokens may contain a single-screen
            // choice. Drop that token once so the portal can ask for all screens.
            config.remote_desktop_restore_token = None;
            config.remote_desktop_multi_select_migrated = true;
            config.store()?;
        }
        let restore_token = config.remote_desktop_restore_token.clone();
        let (sender, receiver) = unbounded_channel();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let pending_motion = Arc::new(Mutex::new(None));
        let motion_queued = Arc::new(AtomicBool::new(false));
        let worker_motion = pending_motion.clone();
        let worker_queued = motion_queued.clone();

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
                            restore_token,
                            worker_motion,
                            worker_queued,
                        ))
                    });
                if let Err(error) = result {
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
            config.store()?;
        }

        Ok(Self {
            sender,
            pending_motion,
            motion_queued,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
        })
    }

    fn submit(&self, event: impl FnOnce(Reply) -> Event) -> Result<()> {
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        self.sender
            .send(event(reply_sender))
            .map_err(|_| anyhow!("portal input session closed"))?;
        reply_receiver
            .recv_timeout(Duration::from_secs(3))
            .context("portal input request timed out")?
            .map_err(|error| anyhow!(error))
    }
}

async fn portal_worker(
    mut receiver: UnboundedReceiver<Event>,
    ready: SyncSender<std::result::Result<Option<String>, String>>,
    restore_token: Option<String>,
    pending_motion: Arc<Mutex<Option<Motion>>>,
    motion_queued: Arc<AtomicBool>,
) -> Result<()> {
    let portal = RemoteDesktop::new().await?;
    let screencast = Screencast::new().await?;
    let configured = crate::config::Config::load()?;
    let source = crate::capture::source_type(&configured.capture_source);
    let available = portal.available_device_types().await?;
    if !available.contains(DeviceType::Keyboard) || !available.contains(DeviceType::Pointer) {
        anyhow::bail!("RemoteDesktop portal does not provide keyboard and pointer devices");
    }

    let session = portal.create_session().await?;
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
            configured.capture_source == "monitor",
            None,
            PersistMode::DoNot,
        )
        .await?
        .response()?;
    let selection = portal.start(&session, None).await?.response()?;
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
    let stream = &selected_streams[0];
    let stream_id = stream.pipe_wire_node_id();
    let (logical_width, logical_height) = stream.size().unwrap_or((65535, 65535));
    let pipewire = screencast.open_pipe_wire_remote(&session).await?;
    let _capture = CaptureGuard(crate::capture::spawn_helper(pipewire, &streams)?);
    let token = selection.restore_token().map(ToOwned::to_owned);
    ready
        .send(Ok(token))
        .map_err(|_| anyhow!("portal setup receiver closed"))?;

    while let Some(event) = receiver.recv().await {
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
                answer(reply, result)?;
            }
            Event::FlushMotion => loop {
                let motion = pending_motion.lock().unwrap().take();
                let Some(motion) = motion else {
                    motion_queued.store(false, Ordering::Release);
                    let has_new_motion = pending_motion.lock().unwrap().is_some();
                    if has_new_motion
                        && motion_queued
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                    {
                        continue;
                    }
                    break;
                };
                let result = if motion.absolute {
                    let logical_x = motion.x.clamp(0.0, 65535.0) * logical_width as f64 / 65535.0;
                    let logical_y = motion.y.clamp(0.0, 65535.0) * logical_height as f64 / 65535.0;
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
            Event::Shutdown => break,
        }
    }
    Ok(())
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
    use super::{merge_motion, Motion};

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
