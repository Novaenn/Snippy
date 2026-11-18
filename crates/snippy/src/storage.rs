//! Encrypted vault adapter owned by the service storage worker.

mod browse;
mod image;
pub use browse::*;
mod model;
mod repository;
mod schema;

pub use model::*;

use rusqlite::{Connection, OpenFlags, limits::Limit};
use snippy_core::config::{AppConfig, CaptureLimits, MAX_CLIP_BYTES};
use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

const APPLICATION_ID: i64 = 0x434c4950;
const SCHEMA_VERSION: i64 = 2;
const FILE_NAME: &str = "vault.db";

/// Owns the decrypted connection only while unlocked. No passphrase is retained.
/// Intentionally does not implement Debug.
pub struct Vault {
    directory: PathBuf,
    connection: Option<Connection>,
    limits: CaptureLimits,
}

impl Vault {
    pub fn at(directory: impl Into<PathBuf>, limits: CaptureLimits) -> Result<Self, StorageError> {
        AppConfig {
            limits: limits.clone(),
            ..AppConfig::default()
        }
        .validate()
        .map_err(|_| StorageError::InvalidLimits)?;
        if limits.max_vault_bytes > i64::MAX as u64 {
            return Err(StorageError::InvalidLimits);
        }
        let directory = directory.into();
        if !directory.is_absolute() {
            return Err(StorageError::UnsafePath);
        }
        Ok(Self {
            directory,
            connection: None,
            limits,
        })
    }

    pub fn is_unlocked(&self) -> bool {
        self.connection.is_some()
    }
    pub fn limits(&self) -> &CaptureLimits {
        &self.limits
    }
    /// Apply validated limits to future saves. Existing history is unaffected:
    /// stored clips stay readable after the clip limit is lowered.
    pub fn set_limits(&mut self, limits: CaptureLimits) -> Result<(), StorageError> {
        AppConfig {
            limits: limits.clone(),
            ..AppConfig::default()
        }
        .validate()
        .map_err(|_| StorageError::InvalidLimits)?;
        if limits.max_vault_bytes > i64::MAX as u64 {
            return Err(StorageError::InvalidLimits);
        }
        self.limits = limits;
        Ok(())
    }
    /// Encrypted database pages currently allocated. Not a quota or a promise
    /// that deletion shrinks the file.
    pub fn physical_bytes(&self) -> Result<u64, StorageError> {
        let connection = self.conn()?;
        let pages = number(connection, "page_count")?;
        let size = number(connection, "page_size")?;
        pages.checked_mul(size).ok_or(StorageError::Integrity)
    }
    pub fn path(&self) -> PathBuf {
        self.directory.join(FILE_NAME)
    }

    /// Exclusive creation: an existing file, including a symlink, is never replaced.
    pub fn create(
        &mut self,
        passphrase: &Passphrase,
        confirmation: &Passphrase,
    ) -> Result<(), StorageError> {
        if self.is_unlocked() {
            return Err(StorageError::AlreadyUnlocked);
        }
        if passphrase.expose() != confirmation.expose() {
            return Err(StorageError::PassphraseMismatch);
        }
        verify_engine()?;
        private_directory(&self.directory, true)?;
        let path = self.path();
        let reserved = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    StorageError::AlreadyExists
                } else {
                    StorageError::Io
                }
            })?;
        drop(reserved);
        let mut creation = NewFile {
            path: path.clone(),
            committed: false,
        };
        let mut connection = encrypted_connection(&path, passphrase)?;
        configure(&connection)?;
        schema::initialize(&mut connection)?;
        let mut header = [0; 16];
        fs::File::open(&path)
            .and_then(|mut file| file.read_exact(&mut header))
            .map_err(|_| StorageError::Io)?;
        if &header == b"SQLite format 3\0" {
            return Err(StorageError::EncryptionUnavailable);
        }
        creation.committed = true;
        self.connection = Some(connection);
        Ok(())
    }

    /// Open only an existing recognized encrypted vault; never initialize/reset it.
    pub fn unlock(&mut self, passphrase: &Passphrase) -> Result<(), StorageError> {
        if self.is_unlocked() {
            return Err(StorageError::AlreadyUnlocked);
        }
        verify_engine()?;
        private_directory(&self.directory, false)?;
        let path = self.path();
        private_file(&path)?;
        let mut connection = encrypted_connection(&path, passphrase)?;
        let app_id: i64 = connection
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .map_err(|_| StorageError::UnlockFailed)?;
        if app_id != APPLICATION_ID {
            return Err(StorageError::UnrecognizedVault);
        }
        // The key was verified by encrypted_connection; enforce the security
        // configuration before any schema write.
        configure(&connection)?;
        schema::migrate(&mut connection)?;
        self.connection = Some(connection);
        Ok(())
    }

    pub fn lock(&mut self) -> Result<(), StorageError> {
        if let Some(connection) = self.connection.take()
            && let Err((connection, error)) = connection.close()
        {
            self.connection = Some(connection);
            return Err(error.into());
        }
        Ok(())
    }

    pub fn security_info(&self) -> Result<SecurityInfo, StorageError> {
        security_info(self.conn()?)
    }

    /// Explicit, potentially expensive diagnostic; normal unlock checks the header/schema.
    pub fn integrity_check(&self) -> Result<(), StorageError> {
        let connection = self.conn()?;
        let mut query = connection.prepare("PRAGMA cipher_integrity_check")?;
        let mut rows = query.query([])?;
        // SQLCipher's successful HMAC check returns no error rows.
        if rows.next()?.is_some() {
            return Err(StorageError::Integrity);
        }
        let check: String = connection.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        if check != "ok" {
            return Err(StorageError::Integrity);
        }
        Ok(())
    }

    fn conn(&self) -> Result<&Connection, StorageError> {
        self.connection.as_ref().ok_or(StorageError::Locked)
    }
    fn conn_mut(&mut self) -> Result<&mut Connection, StorageError> {
        self.connection.as_mut().ok_or(StorageError::Locked)
    }
}

fn verify_engine() -> Result<(), StorageError> {
    let connection = Connection::open_in_memory()?;
    require_cipher(&connection)?;
    // SQLCipher otherwise writes expected wrong-key failures directly to stderr.
    // Expose failures through the sanitized StorageError path only.
    connection.pragma_update(None, "cipher_log_level", "NONE")?;
    Ok(())
}

fn require_cipher(connection: &Connection) -> Result<String, StorageError> {
    let version: String = connection
        .pragma_query_value(None, "cipher_version", |r| r.get(0))
        .map_err(|_| StorageError::EncryptionUnavailable)?;
    if !version.starts_with("4.") {
        return Err(StorageError::EncryptionUnavailable);
    }
    Ok(version)
}

fn encrypted_connection(path: &Path, passphrase: &Passphrase) -> Result<Connection, StorageError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    require_cipher(&connection)?;
    // rusqlite quotes the value safely; never construct SQL from the passphrase.
    // The wrapper zeroizes its owned input; SQLCipher retains its derived key
    // until connection close. Library-internal copies cannot all be guaranteed wiped.
    connection.pragma_update(None, "key", passphrase.expose())?;
    connection.pragma_update(None, "cipher_memory_security", "ON")?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection
        .query_row("SELECT count(*) FROM sqlite_schema", [], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(|_| StorageError::UnlockFailed)?;
    Ok(connection)
}

/// Fixed engineering bound, independent of the user's clip setting, so history
/// saved under a larger earlier setting remains readable.
const MAX_SQL_LENGTH: u64 = MAX_CLIP_BYTES * 2 + 65536;
const _: () = assert!(MAX_SQL_LENGTH <= i32::MAX as u64);

fn configure(connection: &Connection) -> Result<(), StorageError> {
    connection.busy_timeout(Duration::from_secs(1))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "DELETE")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "secure_delete", "ON")?;
    connection.pragma_update(None, "cache_size", -8192)?;
    connection.pragma_update(None, "mmap_size", 0)?;
    connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_SQL_LENGTH as i32)?;
    connection.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    let info = security_info(connection)?;
    if info.temp_store != 2
        || !info.memory_temp_build
        || info.kdf_iterations < 256_000
        || !info.hmac
        || info.plaintext_header_bytes != 0
        || !info.memory_security
        || !info.fts5
    {
        return Err(StorageError::EncryptionUnavailable);
    }
    Ok(())
}

fn number(connection: &Connection, pragma: &str) -> Result<u64, StorageError> {
    use rusqlite::types::Value;
    let value: Value = connection.pragma_query_value(None, pragma, |row| row.get(0))?;
    match value {
        Value::Integer(value) => u64::try_from(value).map_err(|_| StorageError::Database),
        Value::Text(value) => value.parse().map_err(|_| StorageError::Database),
        _ => Err(StorageError::Database),
    }
}

fn security_info(connection: &Connection) -> Result<SecurityInfo, StorageError> {
    let compile_option = |name: &str| {
        connection
            .query_row("SELECT sqlite_compileoption_used(?1)", [name], |row| {
                row.get::<_, bool>(0)
            })
            .map_err(StorageError::from)
    };
    Ok(SecurityInfo {
        cipher_version: require_cipher(connection)?,
        crypto_provider: connection
            .pragma_query_value(None, "cipher_provider", |row| row.get(0))?,
        crypto_provider_version: connection.pragma_query_value(
            None,
            "cipher_provider_version",
            |row| row.get(0),
        )?,
        kdf_iterations: number(connection, "kdf_iter")?,
        temp_store: number(connection, "temp_store")?,
        memory_temp_build: compile_option("TEMP_STORE=2")? || compile_option("TEMP_STORE=3")?,
        hmac: number(connection, "cipher_use_hmac")? == 1,
        plaintext_header_bytes: number(connection, "cipher_plaintext_header_size")?,
        memory_security: number(connection, "cipher_memory_security")? == 1,
        fts5: compile_option("ENABLE_FTS5")?,
    })
}

fn private_directory(path: &Path, create: bool) -> Result<(), StorageError> {
    if create && !path.exists() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|_| StorageError::Io)?;
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| StorageError::Io)?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(StorageError::UnsafePath);
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<(), StorageError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| StorageError::Io)?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(StorageError::UnsafePath);
    }
    if metadata.len() == 0 {
        return Err(StorageError::UnrecognizedVault);
    }
    Ok(())
}

struct NewFile {
    path: PathBuf,
    committed: bool,
}
impl Drop for NewFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
            for suffix in ["-journal", "-wal", "-shm"] {
                let mut sidecar = self.path.as_os_str().to_owned();
                sidecar.push(suffix);
                let _ = fs::remove_file(sidecar);
            }
        }
    }
}

#[cfg(all(test, feature = "sqlcipher"))]
mod tests {
    use super::*;
    use snippy_core::source::SourceIdentity;

    fn key() -> Passphrase {
        Passphrase::new("synthetic journal test key".into()).unwrap()
    }
    fn fixture() -> (tempfile::TempDir, Vault) {
        let temp = tempfile::tempdir().unwrap();
        let mut vault = Vault::at(temp.path().join("vault"), CaptureLimits::default()).unwrap();
        vault.create(&key(), &key()).unwrap();
        (temp, vault)
    }
    fn marker() -> Payload {
        Payload::text(
            &"synthetic-journal-confidential-marker-".repeat(1000),
            &CaptureLimits::default(),
        )
        .unwrap()
    }
    fn assert_encrypted(path: &Path) {
        let bytes = fs::read(path).unwrap();
        assert!(
            !bytes
                .windows(b"synthetic-journal-confidential-marker-".len())
                .any(|w| w == b"synthetic-journal-confidential-marker-")
        );
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn rollback_journal_and_wal_hold_encrypted_pages_and_interruption_preserves_history() {
        let (_temp, mut vault) = fixture();
        let saved = vault.save(&SourceIdentity::Unknown, &marker()).unwrap();
        let db_path = vault.path();
        {
            let tx = vault
                .conn_mut()
                .unwrap()
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            // Spill encrypted dirty pages into a live rollback journal.
            tx.pragma_update(None, "cache_size", 1).unwrap();
            tx.execute(
                "UPDATE clips SET payload=zeroblob(length(payload)) WHERE id=?1",
                [saved.id],
            )
            .unwrap();
            tx.execute(
                "UPDATE clip_fts SET search_text='interrupted' WHERE rowid=?1",
                [saved.id],
            )
            .unwrap();
            let journal = db_path.with_file_name("vault.db-journal");
            assert!(fs::metadata(&journal).unwrap().len() > 512);
            assert_encrypted(&journal);
            // Drop without commit models interruption of a multi-table write.
        }
        assert_eq!(
            vault.get(saved.id).unwrap().payload.bytes(),
            marker().bytes()
        );
        assert_eq!(vault.search("confidential", 10, 0).unwrap().len(), 1);
        vault
            .conn()
            .unwrap()
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        vault
            .conn()
            .unwrap()
            .pragma_update(None, "wal_autocheckpoint", 0)
            .unwrap();
        vault
            .save(
                &SourceIdentity::Manual {
                    label: "Other fixture source".into(),
                },
                &marker(),
            )
            .unwrap();
        let wal = db_path.with_file_name("vault.db-wal");
        assert!(fs::metadata(&wal).unwrap().len() > 32);
        assert_encrypted(&wal);
        assert_encrypted(&db_path);
        vault.integrity_check().unwrap();
        vault.lock().unwrap();
        vault.unlock(&key()).unwrap();
        assert_eq!(vault.usage().unwrap().items, 2);
    }

    #[test]
    fn sqlite_full_rolls_back_content_metadata_and_search_index() {
        let (_temp, mut vault) = fixture();
        let saved = vault
            .save(
                &SourceIdentity::Unknown,
                &Payload::text("existing fixture", vault.limits()).unwrap(),
            )
            .unwrap();
        let before = vault.usage().unwrap();
        let pages: i64 = vault
            .conn()
            .unwrap()
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .unwrap();
        vault
            .conn()
            .unwrap()
            .pragma_update(None, "max_page_count", pages)
            .unwrap();
        let large =
            Payload::text(&"new uncommitted marker ".repeat(30_000), vault.limits()).unwrap();
        assert_eq!(
            vault
                .save(
                    &SourceIdentity::Manual {
                        label: "new-source".into()
                    },
                    &large
                )
                .err(),
            Some(StorageError::DiskFull)
        );
        assert_eq!(vault.usage().unwrap(), before);
        assert_eq!(
            vault.get(saved.id).unwrap().payload.bytes(),
            b"existing fixture"
        );
        assert!(vault.search("uncommitted", 10, 0).unwrap().is_empty());
        let orphan: i64 = vault
            .conn()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM sources WHERE identity='new-source'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphan, 0);
        vault.integrity_check().unwrap();
    }

    fn plan(connection: &Connection, sql: &str) -> String {
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        // Unbound parameters plan as NULL; the plan shape is what matters.
        let mut rows = statement.raw_query();
        let mut details = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            details.push(row.get::<_, String>(3).unwrap());
        }
        details.join("\n")
    }

    #[test]
    fn v1_vault_upgrades_on_unlock_preserving_history() {
        let temp = tempfile::tempdir().unwrap();
        let mut vault = Vault::at(temp.path().join("vault"), CaptureLimits::default()).unwrap();
        private_directory(&vault.directory, true).unwrap();
        drop(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(vault.path())
                .unwrap(),
        );
        let mut connection = encrypted_connection(&vault.path(), &key()).unwrap();
        configure(&connection).unwrap();
        schema::initialize_v1(&mut connection).unwrap();
        vault.connection = Some(connection);
        let saved = vault.save(&SourceIdentity::Unknown, &marker()).unwrap();
        vault.set_tags(saved.id, &["kept"]).unwrap();
        let before = vault.usage().unwrap();
        vault.lock().unwrap();

        vault.unlock(&key()).unwrap();
        let connection = vault.conn().unwrap();
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert!(plan(connection, "SELECT sum(byte_count) FROM clips").contains("clips_browse"));
        assert_eq!(vault.usage().unwrap(), before);
        let loaded = vault.get(saved.id).unwrap();
        assert_eq!(loaded.payload.bytes(), marker().bytes());
        assert_eq!(loaded.summary.tags, ["kept"]);
        assert_eq!(vault.search("confidential", 10, 0).unwrap().len(), 1);
        vault.integrity_check().unwrap();
        vault.lock().unwrap();
        vault.unlock(&key()).unwrap();
        assert_eq!(vault.usage().unwrap(), before);
        assert_encrypted(&vault.path());
    }

    #[test]
    fn usage_and_browse_metadata_use_the_covering_index() {
        let (_temp, mut vault) = fixture();
        vault.save(&SourceIdentity::Unknown, &marker()).unwrap();
        let connection = vault.conn().unwrap();
        for sql in [
            repository::USAGE_SQL,
            repository::USED_SQL,
            repository::SEARCH_SQL,
        ] {
            let plan = plan(connection, sql);
            assert!(plan.contains("COVERING INDEX clips_browse"), "{plan}");
        }
        // Browse filters and orders from the index; the table row (text
        // preview) is read only for rows on the returned page.
        let browse = plan(connection, browse::BROWSE_SQL);
        assert!(browse.contains("USING INDEX clips_browse"), "{browse}");
        assert!(!browse.contains("TEMP B-TREE"), "{browse}");
    }

    #[test]
    fn newer_schema_and_other_app_vault_are_preserved() {
        let (_temp, mut vault) = fixture();
        vault
            .conn()
            .unwrap()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        vault.lock().unwrap();
        let original = fs::read(vault.path()).unwrap();
        assert_eq!(vault.unlock(&key()), Err(StorageError::UnsupportedSchema));
        assert_eq!(fs::read(vault.path()).unwrap(), original);
        let connection = encrypted_connection(&vault.path(), &key()).unwrap();
        connection.pragma_update(None, "application_id", 0).unwrap();
        drop(connection);
        let original = fs::read(vault.path()).unwrap();
        assert_eq!(vault.unlock(&key()), Err(StorageError::UnrecognizedVault));
        assert_eq!(fs::read(vault.path()).unwrap(), original);
    }
}
