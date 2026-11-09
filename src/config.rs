use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// Portal source type: monitor, window, or virtual.
    pub capture_source: String,
    /// Multiple consented monitors are opt-in; existing grants stay intact.
    pub capture_multiple: bool,
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
            capture_source: "monitor".into(),
            capture_multiple: false,
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
        match std::fs::read_to_string(&path) {
            Ok(raw) => toml::from_str(&raw).context("parsing config.toml"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::update(|_| {}),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn store(&self) -> Result<()> {
        let (path, _lock) = Self::write_lock()?;
        self.store_locked(&path)
    }

    /// Serialize read/modify/write across the capture, input and frontend
    /// processes. Background token saves must not overwrite other fields.
    pub fn update(change: impl FnOnce(&mut Self)) -> Result<Self> {
        let (path, _lock) = Self::write_lock()?;
        let mut config = match std::fs::read_to_string(&path) {
            Ok(raw) => toml::from_str(&raw).context("parsing config.toml")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => return Err(error).context("reading configuration for update"),
        };
        change(&mut config);
        config.store_locked(&path)?;
        Ok(config)
    }

    fn write_lock() -> Result<(PathBuf, std::fs::File)> {
        let mut path = config_path()?;
        // Do not replace a user-created symlink to declarative configuration.
        if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            path = std::fs::canonicalize(&path).context("resolving configuration symlink")?;
        }
        let parent = path.parent().context("configuration has no parent")?;
        std::fs::create_dir_all(parent)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path.with_extension("toml.lock"))?;
        loop {
            if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("locking configuration");
            }
        }
        Ok((path, lock))
    }

    fn store_locked(&self, path: &std::path::Path) -> Result<()> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = path.parent().context("configuration has no parent")?;
        let mut document = match std::fs::read_to_string(path) {
            Ok(raw) => {
                toml::from_str::<toml::Table>(&raw).context("parsing existing configuration")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
            Err(error) => return Err(error).context("reading existing configuration"),
        };
        // Keep keys belonging to newer versions. JSON includes None fields,
        // allowing explicitly cleared optional fields to remove old values.
        for name in serde_json::to_value(self)?.as_object().unwrap().keys() {
            document.remove(name);
        }
        document.extend(toml::Value::try_from(self)?.as_table().unwrap().clone());
        let text = toml::to_string_pretty(&document)?;
        for _ in 0..32 {
            let temporary = parent.join(format!(
                ".uur-config-{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = match std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("creating configuration temporary file"),
            };
            let result = (|| -> Result<()> {
                file.write_all(text.as_bytes())?;
                file.sync_all()?;
                std::fs::rename(&temporary, path)?;
                std::fs::File::open(parent)?.sync_all()?;
                Ok(())
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temporary);
            }
            return result.with_context(|| format!("writing {}", path.display()));
        }
        anyhow::bail!("could not create a configuration temporary file")
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
        assert!(!config.capture_multiple);
    }
}
