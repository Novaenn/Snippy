//! Nonsecret preference persistence. Private source rules remain in the vault.

use crate::ipc::Error;
use snippy_core::config::{AppConfig, MAX_CLIP_BYTES};
use std::{
    ffi::OsString,
    fs::{self, DirBuilder, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

const FILE_NAME: &str = "settings.conf";
const MAX_BYTES: u64 = 64 * 1024;
const MIB: u64 = 1024 * 1024;

/// Absolute private path for the plaintext settings file.
#[derive(Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn discover() -> Result<Self, Error> {
        let root = match std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
            Some(root) => PathBuf::from(root),
            None => {
                PathBuf::from(std::env::var_os("HOME").ok_or(Error::UnsafePath)?).join(".config")
            }
        };
        if !root.is_absolute() {
            return Err(Error::UnsafePath);
        }
        Ok(Self {
            path: root.join("snippy").join(FILE_NAME),
        })
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Missing files use agreed defaults. Invalid or unsafe existing files fail
    /// closed so a damaged configuration cannot silently change behavior.
    pub fn load(&self) -> Result<AppConfig, Error> {
        let directory = self.path.parent().ok_or(Error::UnsafePath)?;
        match fs::symlink_metadata(directory) {
            Ok(metadata) => {
                if !metadata.is_dir()
                    || metadata.file_type().is_symlink()
                    || metadata.uid() != rustix::process::geteuid().as_raw()
                    || metadata.mode() & 0o077 != 0
                {
                    return Err(Error::Configuration);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AppConfig::default());
            }
            Err(_) => return Err(Error::Configuration),
        }
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AppConfig::default());
            }
            Err(_) => return Err(Error::Configuration),
        };
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
            || metadata.len() > MAX_BYTES
        {
            return Err(Error::Configuration);
        }
        let text = fs::read_to_string(&self.path).map_err(|_| Error::Configuration)?;
        parse(&text)
    }

    /// Atomic replacement with private permissions. The service owns writes.
    pub fn save(&self, config: &AppConfig) -> Result<(), Error> {
        validate(config)?;
        let directory = self.path.parent().ok_or(Error::UnsafePath)?;
        match DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
        {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(_) => return Err(Error::Io),
        }
        let metadata = fs::symlink_metadata(directory).map_err(|_| Error::Io)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(Error::UnsafePath);
        }
        let temporary = directory.join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&temporary);
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
                .open(&temporary)
                .map_err(|_| Error::Io)?;
            file.write_all(self.render(config).as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|_| Error::Io)?;
            fs::rename(&temporary, &self.path).map_err(|_| Error::Io)?;
            if let Ok(directory) = fs::File::open(directory) {
                let _ = directory.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn render(&self, config: &AppConfig) -> String {
        format!(
            "# snippy settings. Nonsecret preferences only.\n\
             shortcuts_enabled={}\n\
             save_shortcut={}\n\
             picker_shortcut={}\n\
             max_clip_bytes={}\n\
             max_vault_bytes={}\n\
             lock_timeout_seconds={}\n\
             autostart={}\n\
             notifications={}\n",
            config.shortcuts.enabled,
            config.shortcuts.save,
            config.shortcuts.picker,
            config.limits.max_clip_bytes,
            config.limits.max_vault_bytes,
            config.lock_timeout.as_secs(),
            config.autostart,
            config.notifications,
        )
    }
}

fn parse(text: &str) -> Result<AppConfig, Error> {
    if text.len() as u64 > MAX_BYTES {
        return Err(Error::Configuration);
    }
    let mut config = AppConfig::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.len() > 1024 || line.chars().any(char::is_control) {
            return Err(Error::Configuration);
        }
        let (key, value) = line.split_once('=').ok_or(Error::Configuration)?;
        let key = key.trim();
        let value = value.trim();
        match key {
            "shortcuts_enabled" => config.shortcuts.enabled = boolean(value)?,
            "save_shortcut" => config.shortcuts.save = shortcut(value)?,
            "picker_shortcut" => config.shortcuts.picker = shortcut(value)?,
            "max_clip_bytes" => {
                config.limits.max_clip_bytes = bounded(value, 1, MAX_CLIP_BYTES)?;
            }
            "max_vault_bytes" => {
                config.limits.max_vault_bytes = bounded(value, 1, 1024 * MIB)?;
            }
            "lock_timeout_seconds" => {
                config.lock_timeout = Duration::from_secs(bounded(value, 1, 86_400)?);
            }
            "autostart" => config.autostart = boolean(value)?,
            "notifications" => config.notifications = boolean(value)?,
            // Forward-compatible: unknown keys do not alter agreed behavior.
            _ => (),
        }
    }
    validate(&config)?;
    Ok(config)
}

/// The one validator for loaded, saved, overridden, and IPC-edited settings.
pub fn validate(config: &AppConfig) -> Result<(), Error> {
    config.validate().map_err(|_| Error::Configuration)?;
    let limits = &config.limits;
    if !(1..=MAX_CLIP_BYTES).contains(&limits.max_clip_bytes)
        || !(1..=1024 * MIB).contains(&limits.max_vault_bytes)
        || !(1..=86_400).contains(&config.lock_timeout.as_secs())
        || config.lock_timeout.subsec_nanos() != 0
    {
        return Err(Error::Configuration);
    }
    for value in [&config.shortcuts.save, &config.shortcuts.picker] {
        shortcut(value)?;
    }
    Ok(())
}

fn boolean(value: &str) -> Result<bool, Error> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(Error::Configuration),
    }
}

fn bounded(value: &str, minimum: u64, maximum: u64) -> Result<u64, Error> {
    let value: u64 = value.parse().map_err(|_| Error::Configuration)?;
    if (minimum..=maximum).contains(&value) {
        Ok(value)
    } else {
        Err(Error::Configuration)
    }
}

fn shortcut(value: &str) -> Result<String, Error> {
    if value.len() > 64 || value.chars().any(char::is_control) {
        return Err(Error::Configuration);
    }
    Ok(value.to_owned())
}

/// One-run daemon overrides. They are never persisted: the service keeps the
/// saved base configuration separately and recomputes the runtime view from it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overrides {
    lock_timeout_seconds: Option<u64>,
    no_shortcuts: bool,
    max_clip_bytes: Option<u64>,
    max_vault_bytes: Option<u64>,
    picker_shortcut: Option<String>,
    save_shortcut: Option<String>,
}

impl Overrides {
    pub fn new(
        lock_timeout_seconds: Option<u64>,
        no_shortcuts: bool,
        max_clip_bytes: Option<u64>,
        max_vault_bytes: Option<u64>,
        picker_shortcut: Option<OsString>,
        save_shortcut: Option<OsString>,
    ) -> Result<Self, Error> {
        let text = |value: Option<OsString>| {
            value
                .map(|value| {
                    let value = value.into_string().map_err(|_| Error::Configuration)?;
                    shortcut(&value)
                })
                .transpose()
        };
        Ok(Self {
            lock_timeout_seconds,
            no_shortcuts,
            max_clip_bytes,
            max_vault_bytes,
            picker_shortcut: text(picker_shortcut)?,
            save_shortcut: text(save_shortcut)?,
        })
    }

    /// Runtime configuration for this run: the saved base with overrides on top.
    pub fn apply(&self, base: &AppConfig) -> Result<AppConfig, Error> {
        let mut config = base.clone();
        if let Some(value) = self.lock_timeout_seconds {
            config.lock_timeout = Duration::from_secs(value);
        }
        if self.no_shortcuts {
            config.shortcuts.enabled = false;
        }
        if let Some(value) = self.max_clip_bytes {
            config.limits.max_clip_bytes = value;
        }
        if let Some(value) = self.max_vault_bytes {
            config.limits.max_vault_bytes = value;
        }
        if let Some(value) = &self.picker_shortcut {
            config.shortcuts.picker = value.clone();
        }
        if let Some(value) = &self.save_shortcut {
            config.shortcuts.save = value.clone();
        }
        validate(&config)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn at(directory: &Path) -> Store {
        Store::at(directory.join("snippy").join(FILE_NAME))
    }

    #[test]
    fn missing_file_uses_defaults_and_round_trip_preserves_values() {
        let temp = tempfile::tempdir().unwrap();
        let store = at(temp.path());
        assert_eq!(store.load().unwrap(), AppConfig::default());
        let mut config = AppConfig::default();
        config.shortcuts.save = "Super+F9".into();
        config.shortcuts.picker = "Ctrl+Alt+Y".into();
        config.shortcuts.enabled = false;
        config.limits.max_clip_bytes = 4096;
        config.limits.max_vault_bytes = 8190;
        config.lock_timeout = Duration::from_secs(45);
        config.autostart = true;
        config.notifications = true;
        store.save(&config).unwrap();
        assert_eq!(store.load().unwrap(), config);
        let mode = fs::metadata(store.path()).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let directory = store.path().parent().unwrap();
        assert_eq!(fs::metadata(directory).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn invalid_or_unsafe_files_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = at(temp.path());
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::set_permissions(
            store.path().parent().unwrap(),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        for value in [
            "shortcuts_enabled=maybe\n",
            "max_clip_bytes=0\n",
            "max_vault_bytes=0\n",
            "lock_timeout_seconds=0\n",
            "lock_timeout_seconds=999999\n",
            "save_shortcut=\n",
        ] {
            fs::write(store.path(), value).unwrap();
            fs::set_permissions(store.path(), fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(store.load().err(), Some(Error::Configuration));
        }
        let public = temp.path().join("public");
        fs::create_dir(&public).unwrap();
        fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            Store::at(public.join(FILE_NAME)).load().err(),
            Some(Error::Configuration)
        );
    }

    #[test]
    fn unknown_keys_are_ignored_and_overrides_are_validated() {
        let text = "unknown_future_key=value\nmax_clip_bytes=4096\n";
        let config = parse(text).unwrap();
        assert_eq!(config.limits.max_clip_bytes, 4096);
        let disabled =
            parse("shortcuts_enabled=false\nsave_shortcut=\npicker_shortcut=\n").unwrap();
        assert!(!disabled.shortcuts.enabled);
        assert!(disabled.shortcuts.save.is_empty());
        let overrides = Overrides::new(Some(30), true, Some(4096), None, None, None).unwrap();
        let config = overrides.apply(&AppConfig::default()).unwrap();
        assert_eq!(config.lock_timeout, Duration::from_secs(30));
        assert!(!config.shortcuts.enabled);
        assert_eq!(config.limits.max_clip_bytes, 4096);
        let zero = Overrides::new(Some(0), false, None, None, None, None).unwrap();
        assert_eq!(
            zero.apply(&AppConfig::default()).err(),
            Some(Error::Configuration)
        );
    }

    #[test]
    fn override_shortcuts_share_the_settings_bounds() {
        for value in ["x".repeat(65), "Ctrl+\u{7}".into(), "Ctrl\n+Alt+V".into()] {
            assert_eq!(
                Overrides::new(None, false, None, None, Some(value.clone().into()), None).err(),
                Some(Error::Configuration)
            );
            assert_eq!(
                Overrides::new(None, false, None, None, None, Some(value.into())).err(),
                Some(Error::Configuration)
            );
        }
        let mut config = AppConfig::default();
        config.shortcuts.save = "x".repeat(65);
        assert_eq!(validate(&config).err(), Some(Error::Configuration));
        config.shortcuts.save = "Ctrl+Alt+G".into();
        config.limits.max_vault_bytes = 1024 * MIB + 1;
        assert_eq!(validate(&config).err(), Some(Error::Configuration));
        let max = "x".repeat(64);
        let overrides = Overrides::new(None, false, None, None, Some(max.clone().into()), None);
        assert_eq!(
            overrides
                .unwrap()
                .apply(&AppConfig::default())
                .unwrap()
                .shortcuts
                .picker,
            max
        );
    }

    #[test]
    fn overrides_win_at_runtime_but_are_never_persisted() {
        let temp = tempfile::tempdir().unwrap();
        let store = at(temp.path());
        let overrides = Overrides::new(
            Some(30),
            true,
            Some(4096),
            Some(8192),
            Some("Super+P".into()),
            Some("Super+S".into()),
        )
        .unwrap();
        // A settings edit changes and saves only the base configuration.
        let base = AppConfig {
            notifications: true,
            lock_timeout: Duration::from_secs(600),
            ..AppConfig::default()
        };
        assert!(base.shortcuts.enabled);
        store.save(&base).unwrap();
        assert_eq!(store.load().unwrap(), base);
        let text = fs::read_to_string(store.path()).unwrap();
        for value in ["Super+P", "Super+S", "4096", "8192", "=30\n"] {
            assert!(!text.contains(value), "override was persisted");
        }
        let runtime = overrides.apply(&store.load().unwrap()).unwrap();
        assert!(!runtime.shortcuts.enabled);
        assert!(runtime.notifications);
        assert_eq!(runtime.lock_timeout, Duration::from_secs(30));
        assert_eq!(runtime.limits.max_clip_bytes, 4096);
        assert_eq!(runtime.limits.max_vault_bytes, 8192);
        assert_eq!(runtime.shortcuts.save, "Super+S");
        assert_eq!(runtime.shortcuts.picker, "Super+P");
        assert_eq!(
            Overrides::default().apply(&base).unwrap(),
            base,
            "no overrides leaves the saved configuration unchanged"
        );
    }
}
