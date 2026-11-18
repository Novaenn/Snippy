use std::{error::Error, fmt};

use snippy_core::config::ConfigError;

pub(crate) enum AppError {
    Configuration(ConfigError),
    Service(snippy::ipc::Error),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Service(error) => fmt::Display::fmt(error, f),
            Self::Configuration(error) => write!(f, "invalid configuration: {error}"),
        }
    }
}

// Error requires Debug; use the same safe representation as Display.
impl fmt::Debug for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Error for AppError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Configuration(error) => Some(error),
            Self::Service(error) => Some(error),
        }
    }
}

impl From<snippy::ipc::Error> for AppError {
    fn from(error: snippy::ipc::Error) -> Self {
        Self::Service(error)
    }
}

impl From<ConfigError> for AppError {
    fn from(error: ConfigError) -> Self {
        Self::Configuration(error)
    }
}
