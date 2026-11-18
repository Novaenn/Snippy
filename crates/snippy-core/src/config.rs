//! Nonsecret settings. The service persists them in a private plaintext file;
//! secrets and private source rules belong in the encrypted vault.

use std::{error::Error, fmt, time::Duration};

const MIB: u64 = 1024 * 1024;

/// Engineering maximum for one clip. The user setting may only lower it, and
/// stored history saved under an earlier, larger setting stays readable.
pub const MAX_CLIP_BYTES: u64 = 25 * MIB;

/// Plaintext-safe preferences only. Private source rules belong in the vault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppConfig {
    pub shortcuts: ShortcutConfig,
    pub limits: CaptureLimits,
    pub lock_timeout: Duration,
    pub autostart: bool,
    pub notifications: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            shortcuts: ShortcutConfig::default(),
            limits: CaptureLimits::default(),
            lock_timeout: Duration::from_secs(15 * 60),
            autostart: false,
            notifications: false,
        }
    }
}

impl AppConfig {
    /// Reject zero limits instead of allowing them to mean unbounded operation.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let limits = &self.limits;
        for (name, value) in [
            ("maximum vault bytes", limits.max_vault_bytes),
            ("maximum clip bytes", limits.max_clip_bytes),
            ("maximum image pixels", limits.max_image_pixels),
            (
                "maximum decoded image bytes",
                limits.max_decoded_image_bytes,
            ),
        ] {
            if value == 0 {
                return Err(ConfigError::ZeroLimit(name));
            }
        }
        for (name, value) in [
            ("lock timeout", self.lock_timeout),
            ("transfer inactivity timeout", limits.transfer_idle_timeout),
            ("transfer total timeout", limits.transfer_total_timeout),
        ] {
            if value.is_zero() {
                return Err(ConfigError::ZeroLimit(name));
            }
        }
        if limits.max_clip_bytes > MAX_CLIP_BYTES {
            return Err(ConfigError::LimitTooLarge("maximum clip bytes"));
        }
        if limits.transfer_idle_timeout > limits.transfer_total_timeout {
            return Err(ConfigError::InconsistentTransferTimeouts);
        }
        if self.shortcuts.enabled
            && (self.shortcuts.save.trim().is_empty() || self.shortcuts.picker.trim().is_empty())
        {
            return Err(ConfigError::EmptyShortcut);
        }
        Ok(())
    }
}

/// Shortcut strings are preferences; X11 parsing and binding belong to the adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShortcutConfig {
    pub enabled: bool,
    pub save: String,
    pub picker: String,
}

impl Default for ShortcutConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            save: "Ctrl+Alt+G".into(),
            picker: "Ctrl+Alt+V".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureLimits {
    pub max_vault_bytes: u64,
    pub max_clip_bytes: u64,
    pub max_image_pixels: u64,
    pub max_decoded_image_bytes: u64,
    pub transfer_idle_timeout: Duration,
    pub transfer_total_timeout: Duration,
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            max_vault_bytes: 1024 * MIB,
            max_clip_bytes: 25 * MIB,
            max_image_pixels: 16_000_000,
            max_decoded_image_bytes: 64 * MIB,
            transfer_idle_timeout: Duration::from_secs(5),
            transfer_total_timeout: Duration::from_secs(30),
        }
    }
}

/// Safe to display: variants contain field names, never configuration values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    ZeroLimit(&'static str),
    LimitTooLarge(&'static str),
    InconsistentTransferTimeouts,
    EmptyShortcut,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroLimit(name) => write!(f, "{name} must be greater than zero"),
            Self::LimitTooLarge(name) => write!(f, "{name} exceeds the supported maximum"),
            Self::InconsistentTransferTimeouts => {
                f.write_str("transfer inactivity timeout must not exceed the total timeout")
            }
            Self::EmptyShortcut => f.write_str("enabled shortcuts must not be empty"),
        }
    }
}

impl Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_preserve_the_agreed_capture_and_lock_policy() {
        let config = AppConfig::default();
        assert_eq!(config.validate(), Ok(()));
        assert_eq!(config.limits.max_vault_bytes, 1_073_741_824);
        assert_eq!(config.limits.max_clip_bytes, 26_214_400);
        assert_eq!(config.limits.max_image_pixels, 16_000_000);
        assert_eq!(config.limits.max_decoded_image_bytes, 67_108_864);
        assert_eq!(config.limits.transfer_idle_timeout, Duration::from_secs(5));
        assert_eq!(
            config.limits.transfer_total_timeout,
            Duration::from_secs(30)
        );
        assert_eq!(config.lock_timeout, Duration::from_secs(900));
        assert!(config.shortcuts.enabled);
        assert_eq!(config.shortcuts.save, "Ctrl+Alt+G");
        assert_eq!(config.shortcuts.picker, "Ctrl+Alt+V");
        assert!(!config.autostart);
        assert!(!config.notifications);
    }

    #[test]
    fn zero_limits_and_inconsistent_timeouts_are_rejected() {
        let mut config = AppConfig::default();
        config.limits.max_clip_bytes = 0;
        assert_eq!(
            config.validate(),
            Err(ConfigError::ZeroLimit("maximum clip bytes"))
        );
        config.limits.max_clip_bytes = MAX_CLIP_BYTES + 1;
        assert_eq!(
            config.validate(),
            Err(ConfigError::LimitTooLarge("maximum clip bytes"))
        );
        config.limits.max_clip_bytes = 1;
        config.limits.transfer_total_timeout = Duration::from_secs(1);
        assert_eq!(
            config.validate(),
            Err(ConfigError::InconsistentTransferTimeouts)
        );
        config.limits.transfer_total_timeout = Duration::from_secs(30);
        config.lock_timeout = Duration::ZERO;
        assert_eq!(
            config.validate(),
            Err(ConfigError::ZeroLimit("lock timeout"))
        );
    }

    #[test]
    fn empty_shortcuts_require_disabling_builtin_bindings() {
        let mut config = AppConfig::default();
        config.shortcuts.save.clear();
        assert_eq!(config.validate(), Err(ConfigError::EmptyShortcut));
        config.shortcuts.enabled = false;
        assert_eq!(config.validate(), Ok(()));
    }
}
