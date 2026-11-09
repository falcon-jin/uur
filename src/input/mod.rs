pub mod portal;
pub mod uinput;
pub mod xtest;

use anyhow::Result;

/// A desktop input sink.  Records arrive already normalized by `protocol`.
pub trait InputBackend: Send {
    fn name(&self) -> &'static str;
    fn check_health(&self) -> Result<()> {
        Ok(())
    }

    fn key(&mut self, vkey: u16, down: bool) -> Result<()>;
    fn button(&mut self, button: u16, down: bool) -> Result<()>;
    /// Absolute (normalized 0..65535) or relative pointer motion.
    fn motion(&mut self, absolute: bool, x: i32, y: i32) -> Result<()>;
    fn wheel(&mut self, horizontal: bool, delta: i32) -> Result<()>;

    /// Release everything currently held (called when the hook disconnects).
    fn release_all(&mut self) -> Result<()>;
}

/// Choose the backend for this session:
///
/// - Wayland sessions prefer the standard RemoteDesktop portal, then use the
///   kernel-level uinput device when the compositor only provides ScreenCast.
/// - X11 sessions use XTEST, which needs no extra privileges.
pub fn select(preferred: &str) -> Result<Box<dyn InputBackend>> {
    let on_wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|session| session == "wayland");
    if !matches!(preferred, "auto" | "portal" | "uinput" | "xtest") {
        anyhow::bail!("unknown input backend {preferred:?}; use auto, portal, uinput, or xtest");
    }

    if preferred == "portal" || (preferred == "auto" && on_wayland) {
        match portal::PortalBackend::connect() {
            Ok(backend) => return Ok(Box::new(backend)),
            Err(error) if preferred == "portal" => return Err(error),
            Err(error) => eprintln!("{}: {error}", t!("input.portal_unavailable")),
        }
    }
    if preferred == "uinput" || (preferred == "auto" && on_wayland) {
        match uinput::UinputBackend::connect() {
            Ok(backend) => return Ok(Box::new(backend)),
            Err(error) if preferred == "uinput" => return Err(error),
            Err(error) => {
                eprintln!("{}: {error}", t!("input.uinput_unavailable"));
            }
        }
    }
    if on_wayland && preferred == "auto" {
        anyhow::bail!("{}", t!("input.wayland_no_full_input"));
    }
    if std::env::var("DISPLAY").is_ok() {
        return Ok(Box::new(xtest::XtestBackend::connect()?));
    }
    anyhow::bail!("{}", t!("input.no_display"));
}

pub(crate) fn wheel_steps(delta: i32) -> i32 {
    if delta.abs() >= 120 {
        (delta / 120).clamp(-16, 16)
    } else {
        delta.signum()
    }
}

#[cfg(test)]
mod tests {
    use super::wheel_steps;

    #[test]
    fn windows_wheel_units_are_normalized() {
        assert_eq!(wheel_steps(120), 1);
        assert_eq!(wheel_steps(-240), -2);
        assert_eq!(wheel_steps(1), 1);
        assert_eq!(wheel_steps(0), 0);
    }
}
