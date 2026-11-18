use super::{APPLICATION_ID, SCHEMA_VERSION, StorageError};
use rusqlite::{Connection, TransactionBehavior};

const V1: &str = "
CREATE TABLE sources (
 id INTEGER PRIMARY KEY,
 provenance INTEGER NOT NULL CHECK(provenance IN (0,1,2)),
 identity TEXT NOT NULL, label TEXT NOT NULL,
 UNIQUE(provenance,identity)
);
CREATE TABLE clips (
 id INTEGER PRIMARY KEY AUTOINCREMENT,
 source_id INTEGER NOT NULL REFERENCES sources(id),
 kind INTEGER NOT NULL CHECK(kind IN (1,2)),
 digest BLOB NOT NULL CHECK(length(digest)=32),
 payload BLOB NOT NULL, thumbnail BLOB NOT NULL,
 byte_count INTEGER NOT NULL CHECK(byte_count>0),
 first_saved_ms INTEGER NOT NULL, last_saved_ms INTEGER NOT NULL,
 UNIQUE(source_id,kind,digest)
);
CREATE INDEX clips_recent ON clips(last_saved_ms DESC,id DESC);
CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
CREATE TABLE clip_tags (
 clip_id INTEGER NOT NULL REFERENCES clips(id) ON DELETE CASCADE,
 tag_id INTEGER NOT NULL REFERENCES tags(id), PRIMARY KEY(clip_id,tag_id)
);
CREATE TABLE vault_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE sensitive_apps (app_id TEXT PRIMARY KEY);
INSERT INTO sensitive_apps VALUES ('x11:wm-class:KeePassXC'), ('x11:wm-class:Bitwarden'), ('x11:wm-class:1Password');
CREATE VIRTUAL TABLE clip_fts USING fts5(search_text);
CREATE TRIGGER clips_delete AFTER DELETE ON clips BEGIN
 DELETE FROM clip_fts WHERE rowid=old.id;
END;
";

/// v2: browse/usage metadata sits after the payload and thumbnail blobs, so
/// reading it from the table walks every encrypted overflow page. This index
/// covers it in browse order; it supersedes `clips_recent`.
const V2: &str = "
CREATE INDEX clips_browse ON clips(last_saved_ms DESC,id DESC,source_id,kind,byte_count,first_saved_ms);
DROP INDEX clips_recent;
";

/// Ordered upgrade steps; each entry produces the listed version.
const STEPS: &[(i64, &str)] = &[(2, V2)];

pub(super) fn initialize(connection: &mut Connection) -> Result<(), StorageError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
    transaction.execute_batch(V1)?;
    for (_, step) in STEPS {
        transaction.execute_batch(step)?;
    }
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

/// Upgrade a recognized older vault in one Immediate transaction; any failed
/// step rolls back every change, including the version. Newer, version-0, and
/// foreign schemas are refused without modification.
pub(super) fn migrate(connection: &mut Connection) -> Result<(), StorageError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(StorageError::UnsupportedSchema);
    }
    if version < 1 {
        return Err(StorageError::UnrecognizedVault);
    }
    let tables: i64 = transaction.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name IN ('sources','clips','tags','clip_tags','vault_settings','sensitive_apps','clip_fts')", [], |r| r.get(0))?;
    if tables != 7 {
        return Err(StorageError::UnrecognizedVault);
    }
    if version == SCHEMA_VERSION {
        if !has_index(&transaction, "clips_browse")? {
            return Err(StorageError::UnrecognizedVault);
        }
        return Ok(());
    }
    for (target, step) in STEPS {
        if *target > version {
            transaction.execute_batch(step)?;
        }
    }
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn has_index(connection: &Connection, name: &str) -> Result<bool, StorageError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='index' AND tbl_name='clips' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}

/// Builds the original version-1 schema so upgrade tests use the real v1 DDL.
#[cfg(all(test, feature = "sqlcipher"))]
pub(super) fn initialize_v1(connection: &mut Connection) -> Result<(), StorageError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
    transaction.execute_batch(V1)?;
    transaction.pragma_update(None, "user_version", 1)?;
    transaction.commit()?;
    Ok(())
}

#[cfg(all(test, feature = "sqlcipher"))]
mod tests {
    use super::*;
    #[test]
    fn failed_schema_step_rolls_back_tables_and_version() {
        let mut connection = Connection::open_in_memory().unwrap();
        // Force a late failure in the real v1 initializer, after earlier DDL ran.
        connection
            .execute_batch("CREATE TABLE sensitive_apps(existing INTEGER)")
            .unwrap();
        assert!(initialize(&mut connection).is_err());
        let count: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name IN ('sources','clips','tags')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .pragma_query_value(None, "application_id", |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn failed_upgrade_step_rolls_back_to_the_intact_v1_schema() {
        let mut connection = Connection::open_in_memory().unwrap();
        initialize_v1(&mut connection).unwrap();
        // Force the v2 step to fail after its first statement has run.
        connection.execute_batch("DROP INDEX clips_recent").unwrap();
        assert!(migrate(&mut connection).is_err());
        assert!(!has_index(&connection, "clips_browse").unwrap());
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(connection.is_autocommit());
    }

    #[test]
    fn v1_upgrades_once_and_newer_or_unversioned_schemas_are_refused() {
        let mut connection = Connection::open_in_memory().unwrap();
        initialize_v1(&mut connection).unwrap();
        migrate(&mut connection).unwrap();
        assert!(has_index(&connection, "clips_browse").unwrap());
        assert!(!has_index(&connection, "clips_recent").unwrap());
        // Already current: a second unlock is a no-op.
        migrate(&mut connection).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        connection
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        assert_eq!(
            migrate(&mut connection),
            Err(StorageError::UnsupportedSchema)
        );
        connection.pragma_update(None, "user_version", 0).unwrap();
        assert_eq!(
            migrate(&mut connection),
            Err(StorageError::UnrecognizedVault)
        );
    }
}
