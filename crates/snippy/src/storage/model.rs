use snippy_core::{config::CaptureLimits, source::SourceIdentity};
use std::{error::Error, fmt};
use zeroize::Zeroizing;

/// No Debug/Display/serialization; expose only for keying an unlocked connection.
pub struct Passphrase(Zeroizing<String>);
impl Passphrase {
    pub fn new(value: String) -> Result<Self, StorageError> {
        let value = Zeroizing::new(value);
        if value.is_empty() || value.len() > 1024 || value.contains('\0') {
            return Err(StorageError::InvalidPassphrase);
        }
        Ok(Self(value))
    }
    pub(super) fn expose(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentKind {
    Text,
    Png,
}
impl ContentKind {
    pub(super) fn code(self) -> i64 {
        match self {
            Self::Text => 1,
            Self::Png => 2,
        }
    }
}

/// Owned plaintext is wiped on drop. Never implement Debug or log this value.
pub struct Payload {
    pub(super) kind: ContentKind,
    pub(super) bytes: Zeroizing<Vec<u8>>,
    pub(super) thumbnail: Zeroizing<Vec<u8>>,
}
impl Payload {
    pub fn text(text: &str, limits: &CaptureLimits) -> Result<Self, StorageError> {
        if text.is_empty() {
            return Err(StorageError::InvalidContent);
        }
        if text.len() as u64 > limits.max_clip_bytes {
            return Err(StorageError::ClipTooLarge);
        }
        Ok(Self {
            kind: ContentKind::Text,
            bytes: Zeroizing::new(text.as_bytes().to_vec()),
            thumbnail: Zeroizing::new(Vec::new()),
        })
    }
    pub fn png(bytes: &[u8], limits: &CaptureLimits) -> Result<Self, StorageError> {
        super::image::normalize(bytes, limits)
    }
    pub fn kind(&self) -> ContentKind {
        self.kind
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn thumbnail(&self) -> &[u8] {
        &self.thumbnail
    }
    pub fn stored_bytes(&self) -> u64 {
        self.bytes.len() as u64 + self.thumbnail.len() as u64
    }
}

pub struct ClipSummary {
    pub id: i64,
    pub source: SourceIdentity,
    pub kind: ContentKind,
    pub byte_count: u64,
    pub first_saved_ms: i64,
    pub last_saved_ms: i64,
    pub tags: Vec<String>,
}
/// Only the currently served payload may survive vault lock. No private metadata.
pub struct ClipboardContent {
    pub kind: ContentKind,
    pub bytes: Zeroizing<Vec<u8>>,
}
pub struct LoadedClip {
    pub summary: ClipSummary,
    pub payload: Payload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaveResult {
    pub id: i64,
    pub merged: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub items: u64,
    pub logical_bytes: u64,
}

/// Operational settings only. No source labels, content, tags, or keys.
#[derive(Debug)]
pub struct SecurityInfo {
    pub cipher_version: String,
    pub crypto_provider: String,
    pub crypto_provider_version: String,
    pub kdf_iterations: u64,
    pub temp_store: u64,
    pub memory_temp_build: bool,
    pub hmac: bool,
    pub plaintext_header_bytes: u64,
    pub memory_security: bool,
    pub fts5: bool,
}

/// Sanitized failures: never expose SQL parameters, paths, payloads, or keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageError {
    Locked,
    AlreadyUnlocked,
    AlreadyExists,
    PassphraseMismatch,
    InvalidPassphrase,
    InvalidLimits,
    UnsafePath,
    EncryptionUnavailable,
    UnlockFailed,
    UnrecognizedVault,
    UnsupportedSchema,
    InvalidContent,
    ClipTooLarge,
    QuotaExceeded,
    InvalidMetadata,
    NotFound,
    DigestCollision,
    Integrity,
    Busy,
    DiskFull,
    Io,
    Database,
    Canceled,
}
impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Canceled => "capture canceled before commit",
            Self::Locked => "vault is locked",
            Self::AlreadyUnlocked => "vault is already unlocked",
            Self::AlreadyExists => "vault already exists; it was not replaced",
            Self::PassphraseMismatch => "passphrase confirmation does not match",
            Self::InvalidPassphrase => {
                "passphrase must be nonempty, at most 1024 bytes, and contain no NUL"
            }
            Self::InvalidLimits => "vault limits are invalid",
            Self::UnsafePath => {
                "vault directory/file must be private, user-owned, and not a symlink"
            }
            Self::EncryptionUnavailable => {
                "a supported SQLCipher 4 build with memory temporary storage and FTS5 is required"
            }
            Self::UnlockFailed => "could not unlock: incorrect passphrase or damaged vault",
            Self::UnrecognizedVault => "file is not a recognized Snippy vault; it was not reset",
            Self::UnsupportedSchema => "vault schema is newer than this application supports",
            Self::InvalidContent => "content is empty or not a supported valid PNG",
            Self::ClipTooLarge => "clip exceeds the configured size or image decoding limit",
            Self::QuotaExceeded => "vault content quota reached; delete items or raise the limit",
            Self::InvalidMetadata => "source, tags, or query parameters are invalid",
            Self::NotFound => "saved item does not exist",
            Self::DigestCollision => "content digest collision; item was not merged",
            Self::Integrity => "vault integrity verification failed",
            Self::Busy => "vault is busy; retry the operation",
            Self::DiskFull => "storage is full; existing history was preserved",
            Self::Io => "vault filesystem operation failed",
            Self::Database => "vault database operation failed",
        })
    }
}
impl Error for StorageError {}
impl From<rusqlite::Error> for StorageError {
    fn from(error: rusqlite::Error) -> Self {
        match error {
            rusqlite::Error::SqliteFailure(code, _) => match code.code {
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                    Self::Busy
                }
                rusqlite::ErrorCode::DiskFull => Self::DiskFull,
                _ => Self::Database,
            },
            _ => Self::Database,
        }
    }
}
