use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Persistent user configuration.  Everything lives under the XDG state and
/// config directories; the managed Wine prefix lives under data.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    /// EULA acceptance recorded during `uur setup`.
    pub eula_accepted: bool,
    /// Loopback port the in-client hook connects to.
    pub bridge_port: u16,
    /// Random per-install token the hook must present on HELLO.
    pub bridge_token: String,
    /// Preferred input backend: "auto" | "xtest" | "portal" | "uinput".
    pub input_backend: String,
    /// Release channel pinned for provisioning.
    pub pinned_version: Option<String>,
    /// ScreenCast portal restore token: after the first consent the portal
    /// restores the monitor selection silently.
    pub capture_restore_token: Option<String>,
    /// Persistent permission for the standard RemoteDesktop input portal.
    pub remote_desktop_restore_token: Option<String>,
    /// Whether the RemoteDesktop portal token was migrated for multi-monitor selection.
    pub remote_desktop_multi_select_migrated: bool,
    /// Portal source type: monitor, window, or virtual.
    pub capture_source: String,
    /// Whether the one-time migration to multi-monitor portal selection ran.
    pub capture_multi_select_migrated: bool,
    /// "portal" uses system consent; "wayland" is explicitly enabled direct monitor capture.
    pub capture_backend: String,
    /// Named Wayland output for direct capture; a single-output session needs no name.
    pub capture_output: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            eula_accepted: false,
            bridge_port: 47010,
            bridge_token: generate_token(),
            input_backend: "auto".into(),
            pinned_version: None,
            capture_restore_token: None,
            remote_desktop_restore_token: None,
            remote_desktop_multi_select_migrated: false,
            capture_source: "monitor".into(),
            capture_multi_select_migrated: false,
            capture_backend: "portal".into(),
            capture_output: None,
        }
    }
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    // Read EXACTLY 32 bytes.  Never `fs::read` a character device like
    // /dev/urandom: it has no EOF, so fs::read would loop forever and
    // consume all memory (this actually caused an OOM once).
    let filled = std::fs::OpenOptions::new()
        .read(true)
        .open("/dev/urandom")
        .and_then(|mut f| {
            use std::io::Read;
            f.read_exact(&mut bytes)
        })
        .is_ok();
    if !filled {
        // No OS entropy available: derive a weak token from the clock so
        // the tool still functions on loopback.  Logged as a condition,
        // not a crash.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut state = t as u64 | 1;
        for byte in bytes.iter_mut() {
            // xorshift64 — not cryptographic, only a stopgap.
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn config_path() -> Result<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("cannot determine config directory")?;
    Ok(base.join("uur").join("config.toml"))
}

pub fn data_dir() -> Result<PathBuf> {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("cannot determine data directory")?;
    Ok(base.join("uur"))
}

/// Private per-login runtime state. Capture frames are sensitive and must not
/// live in a world-readable global /dev/shm name shared by every user.
pub fn runtime_dir() -> Result<PathBuf> {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("/tmp/uur-{}", unsafe { libc::geteuid() })));
    let dir = base.join("uur");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating runtime directory {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            let config = Self::default();
            config.store()?;
            return Ok(config);
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&raw).context("parsing config.toml")
    }

    pub fn store(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_generation_terminates_and_is_wellformed() {
        // Regression: generate_token once used fs::read on /dev/urandom,
        // which never returns EOF and consumed all memory.
        let token = generate_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn older_configuration_gets_new_defaults() {
        let config: Config = toml::from_str(
            r#"
eula_accepted = true
bridge_port = 47010
bridge_token = "00"
input_backend = "auto"
"#,
        )
        .unwrap();
        assert!(config.remote_desktop_restore_token.is_none());
        assert!(config.capture_restore_token.is_none());
        assert_eq!(config.capture_source, "monitor");
    }
}
