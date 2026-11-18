use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use snippy_core::source::SourceIdentity;
use zeroize::Zeroizing;

use super::{
    ClipSummary, ContentKind, LoadedClip, Payload, SaveResult, StorageError, Usage, Vault,
};

// Metadata-only queries; the clips_browse index covers them so they never
// read payload or thumbnail overflow pages.
pub(super) const USAGE_SQL: &str = "SELECT count(*),coalesce(sum(byte_count),0) FROM clips";
pub(super) const USED_SQL: &str = "SELECT coalesce(sum(byte_count),0) FROM clips";
pub(super) const SEARCH_SQL: &str = "SELECT c.id,s.provenance,s.identity,s.label,c.kind,c.byte_count,c.first_saved_ms,c.last_saved_ms FROM clips c JOIN sources s ON s.id=c.source_id WHERE ?1='' OR instr(lower(s.label),lower(?1))>0 OR c.id IN (SELECT clip_id FROM clip_tags JOIN tags ON tags.id=tag_id WHERE instr(lower(name),lower(?1))>0) OR c.id IN (SELECT rowid FROM clip_fts WHERE clip_fts MATCH ?2) ORDER BY c.last_saved_ms DESC,c.id DESC LIMIT ?3 OFFSET ?4";

impl Vault {
    pub fn save(
        &mut self,
        source: &SourceIdentity,
        payload: &Payload,
    ) -> Result<SaveResult, StorageError> {
        self.save_guarded(source, payload, || true)
    }

    /// Check cancellation immediately before commit, rolling back all rows,
    /// including source/index/timestamp changes, if authorization was revoked.
    pub fn save_guarded(
        &mut self,
        source: &SourceIdentity,
        payload: &Payload,
        authorized: impl Fn() -> bool,
    ) -> Result<SaveResult, StorageError> {
        let (provenance, identity, label) = source_parts(source)?;
        if payload.stored_bytes() > self.limits.max_clip_bytes {
            return Err(StorageError::ClipTooLarge);
        }
        let quota = self.limits.max_vault_bytes;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&[payload.kind.code() as u8]);
        hasher.update(&payload.bytes);
        let digest = hasher.finalize();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StorageError::Database)?
            .as_millis();
        let now = i64::try_from(now).map_err(|_| StorageError::Database)?;
        let tx = self
            .conn_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO sources(provenance,identity,label) VALUES (?1,?2,?3) ON CONFLICT(provenance,identity) DO UPDATE SET label=excluded.label", params![provenance,identity,label])?;
        let source_id: i64 = tx.query_row(
            "SELECT id FROM sources WHERE provenance=?1 AND identity=?2",
            params![provenance, identity],
            |r| r.get(0),
        )?;
        let existing = tx
            .query_row(
                "SELECT id,payload FROM clips WHERE source_id=?1 AND kind=?2 AND digest=?3",
                params![source_id, payload.kind.code(), digest.as_bytes().as_slice()],
                |r| Ok((r.get::<_, i64>(0)?, Zeroizing::new(r.get::<_, Vec<u8>>(1)?))),
            )
            .optional()?;
        if let Some((id, previous)) = existing {
            if previous.as_slice() != payload.bytes.as_slice() {
                return Err(StorageError::DigestCollision);
            }
            tx.execute(
                "UPDATE clips SET last_saved_ms=max(last_saved_ms,?1) WHERE id=?2",
                params![now, id],
            )?;
            if !authorized() {
                return Err(StorageError::Canceled);
            }
            tx.commit()?;
            return Ok(SaveResult { id, merged: true });
        }
        let used: u64 = tx.query_row(USED_SQL, [], |r| unsigned(r, 0))?;
        if used
            .checked_add(payload.stored_bytes())
            .is_none_or(|total| total > quota)
        {
            return Err(StorageError::QuotaExceeded);
        }
        let byte_count =
            i64::try_from(payload.stored_bytes()).map_err(|_| StorageError::ClipTooLarge)?;
        tx.execute("INSERT INTO clips(source_id,kind,digest,payload,thumbnail,byte_count,first_saved_ms,last_saved_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?7)", params![source_id,payload.kind.code(),digest.as_bytes().as_slice(),payload.bytes.as_slice(),payload.thumbnail.as_slice(),byte_count,now])?;
        let id = tx.last_insert_rowid();
        let search = if payload.kind == ContentKind::Text {
            std::str::from_utf8(&payload.bytes).map_err(|_| StorageError::InvalidContent)?
        } else {
            ""
        };
        tx.execute(
            "INSERT INTO clip_fts(rowid,search_text) VALUES (?1,?2)",
            params![id, search],
        )?;
        if !authorized() {
            return Err(StorageError::Canceled);
        }
        tx.commit()?;
        Ok(SaveResult { id, merged: false })
    }

    pub fn usage(&self) -> Result<Usage, StorageError> {
        Ok(self.conn()?.query_row(USAGE_SQL, [], |r| {
            Ok(Usage {
                items: unsigned(r, 0)?,
                logical_bytes: unsigned(r, 1)?,
            })
        })?)
    }

    /// At most 100 summaries; no payloads or thumbnails are read.
    pub fn list(&self, limit: u32, offset: u64) -> Result<Vec<ClipSummary>, StorageError> {
        self.search("", limit, offset)
    }

    /// Literal FTS phrase for text, substring matching for source labels and tags.
    /// User input is never interpreted as FTS operators or SQL.
    pub fn search(
        &self,
        query: &str,
        limit: u32,
        offset: u64,
    ) -> Result<Vec<ClipSummary>, StorageError> {
        if limit == 0
            || limit > 100
            || offset > i64::MAX as u64
            || query.len() > 1024
            || query.contains('\0')
        {
            return Err(StorageError::InvalidMetadata);
        }
        let connection = self.conn()?;
        let phrase = Zeroizing::new(format!("\"{}\"", query.replace('"', "\"\"")));
        let mut statement = connection.prepare(SEARCH_SQL)?;
        // An empty FTS phrase is legal and matches nothing.
        let mut rows = statement.query(params![query, phrase.as_str(), limit, offset as i64])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            result.push(summary(row)?);
        }
        for item in &mut result {
            item.tags = self.tags(item.id)?;
        }
        Ok(result)
    }

    pub fn get(&self, id: i64) -> Result<LoadedClip, StorageError> {
        let mut item = self.conn()?.query_row("SELECT c.id,s.provenance,s.identity,s.label,c.kind,c.byte_count,c.first_saved_ms,c.last_saved_ms,c.payload,c.thumbnail FROM clips c JOIN sources s ON s.id=c.source_id WHERE c.id=?1", [id], |row| {
            Ok(LoadedClip { summary: summary(row)?, payload: Payload { kind:kind(row.get(4)?)?,bytes:Zeroizing::new(row.get(8)?),thumbnail:Zeroizing::new(row.get(9)?) } })
        }).optional()?.ok_or(StorageError::NotFound)?;
        item.summary.tags = self.tags(id)?;
        Ok(item)
    }

    pub(super) fn tags(&self, id: i64) -> Result<Vec<String>, StorageError> {
        let mut query = self.conn()?.prepare(
            "SELECT name FROM tags JOIN clip_tags ON tags.id=tag_id WHERE clip_id=?1 ORDER BY name",
        )?;
        Ok(query
            .query_map([id], |r| r.get(0))?
            .collect::<Result<_, _>>()?)
    }

    pub fn set_tags(&mut self, id: i64, tags: &[&str]) -> Result<(), StorageError> {
        if tags.len() > 32 || tags.iter().any(|tag| !valid(tag.trim(), 128)) {
            return Err(StorageError::InvalidMetadata);
        }
        let tx = self
            .conn_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM clips WHERE id=?1)",
            [id],
            |r| r.get::<_, bool>(0),
        )? {
            return Err(StorageError::NotFound);
        }
        tx.execute("DELETE FROM clip_tags WHERE clip_id=?1", [id])?;
        for tag in tags {
            tx.execute("INSERT OR IGNORE INTO tags(name) VALUES (?1)", [tag.trim()])?;
            tx.execute("INSERT OR IGNORE INTO clip_tags(clip_id,tag_id) SELECT ?1,id FROM tags WHERE name=?2", params![id,tag.trim()])?;
        }
        tx.execute(
            "DELETE FROM tags WHERE id NOT IN (SELECT tag_id FROM clip_tags)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Add or remove one tag transactionally and return the resulting set.
    pub fn edit_tag(&mut self, id: i64, tag: &str, add: bool) -> Result<Vec<String>, StorageError> {
        let tag = tag.trim();
        if !valid(tag, 128) {
            return Err(StorageError::InvalidMetadata);
        }
        {
            let tx = self
                .conn_mut()?
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            if !tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM clips WHERE id=?1)",
                [id],
                |r| r.get::<_, bool>(0),
            )? {
                return Err(StorageError::NotFound);
            }
            if add {
                let count: i64 = tx.query_row(
                    "SELECT count(*) FROM clip_tags WHERE clip_id=?1",
                    [id],
                    |r| r.get(0),
                )?;
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM clip_tags JOIN tags ON tags.id=tag_id WHERE clip_id=?1 AND name=?2)",
                    params![id, tag],
                    |r| r.get(0),
                )?;
                if !exists && count >= 32 {
                    return Err(StorageError::InvalidMetadata);
                }
                tx.execute("INSERT OR IGNORE INTO tags(name) VALUES (?1)", [tag])?;
                tx.execute("INSERT OR IGNORE INTO clip_tags(clip_id,tag_id) SELECT ?1,id FROM tags WHERE name=?2", params![id, tag])?;
            } else {
                tx.execute("DELETE FROM clip_tags WHERE clip_id=?1 AND tag_id IN (SELECT id FROM tags WHERE name=?2)", params![id, tag])?;
                tx.execute(
                    "DELETE FROM tags WHERE id NOT IN (SELECT tag_id FROM clip_tags)",
                    [],
                )?;
            }
            tx.commit()?;
        }
        self.tags(id)
    }

    pub fn delete(&mut self, id: i64) -> Result<(), StorageError> {
        let tx = self
            .conn_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.execute("DELETE FROM clips WHERE id=?1", [id])? == 0 {
            return Err(StorageError::NotFound);
        }
        tx.execute(
            "DELETE FROM tags WHERE id NOT IN (SELECT tag_id FROM clip_tags)",
            [],
        )?;
        tx.execute(
            "DELETE FROM sources WHERE id NOT IN (SELECT source_id FROM clips)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// One transaction for a bounded manager selection; missing ids are skipped.
    pub fn delete_many(&mut self, ids: &[i64]) -> Result<u64, StorageError> {
        if ids.is_empty() || ids.len() > 100 || ids.iter().any(|id| *id <= 0) {
            return Err(StorageError::InvalidMetadata);
        }
        let tx = self
            .conn_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut deleted = 0u64;
        for id in ids {
            deleted += tx.execute("DELETE FROM clips WHERE id=?1", [id])? as u64;
        }
        if deleted > 0 {
            tx.execute(
                "DELETE FROM tags WHERE id NOT IN (SELECT tag_id FROM clip_tags)",
                [],
            )?;
            tx.execute(
                "DELETE FROM sources WHERE id NOT IN (SELECT source_id FROM clips)",
                [],
            )?;
        }
        tx.commit()?;
        Ok(deleted)
    }

    pub fn is_sensitive(&self, source: &SourceIdentity) -> Result<bool, StorageError> {
        let SourceIdentity::Observed { app_id, .. } = source else {
            return Ok(false);
        };
        // X11 WM_CLASS capitalization varies between packaging/toolkits. Match
        // encrypted identifiers, never display labels or a supplied manual label.
        Ok(self.conn()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM sensitive_apps WHERE app_id=?1 COLLATE NOCASE)",
            [app_id],
            |r| r.get(0),
        )?)
    }

    pub fn sensitive_apps(&self) -> Result<Vec<String>, StorageError> {
        let mut statement = self
            .conn()?
            .prepare("SELECT app_id FROM sensitive_apps ORDER BY app_id")?;
        Ok(statement
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?)
    }

    pub fn set_sensitive_apps(&mut self, app_ids: &[&str]) -> Result<(), StorageError> {
        if app_ids.len() > 128 || app_ids.iter().any(|id| !valid(id, 512)) {
            return Err(StorageError::InvalidMetadata);
        }
        let tx = self
            .conn_mut()?
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM sensitive_apps", [])?;
        for id in app_ids {
            tx.execute(
                "INSERT INTO sensitive_apps(app_id) SELECT ?1 WHERE NOT EXISTS(SELECT 1 FROM sensitive_apps WHERE app_id=?1 COLLATE NOCASE)",
                [id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Add or remove one encrypted rule, returning the sorted current list.
    pub fn edit_sensitive(&mut self, app_id: &str, add: bool) -> Result<Vec<String>, StorageError> {
        let app_id = app_id.trim();
        if !valid(app_id, 512) {
            return Err(StorageError::InvalidMetadata);
        }
        {
            let tx = self
                .conn_mut()?
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            if add {
                let count: i64 =
                    tx.query_row("SELECT count(*) FROM sensitive_apps", [], |r| r.get(0))?;
                let exists: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sensitive_apps WHERE app_id=?1 COLLATE NOCASE)",
                    [app_id],
                    |r| r.get(0),
                )?;
                if !exists {
                    if count >= 128 {
                        return Err(StorageError::InvalidMetadata);
                    }
                    tx.execute("INSERT INTO sensitive_apps(app_id) VALUES (?1)", [app_id])?;
                }
            } else {
                // Matching is case-insensitive, so removal must be too.
                tx.execute(
                    "DELETE FROM sensitive_apps WHERE app_id=?1 COLLATE NOCASE",
                    [app_id],
                )?;
            }
            tx.commit()?;
        }
        self.sensitive_apps()
    }
}

fn valid(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn source_parts(source: &SourceIdentity) -> Result<(i64, &str, &str), StorageError> {
    match source {
        SourceIdentity::Observed {
            app_id,
            display_name,
        } if valid(app_id, 512) && valid(display_name, 256) => Ok((1, app_id, display_name)),
        SourceIdentity::Manual { label } if valid(label, 256) => Ok((2, label, label)),
        SourceIdentity::Unknown => Ok((0, "", "Unknown")),
        _ => Err(StorageError::InvalidMetadata),
    }
}

fn kind(value: i64) -> rusqlite::Result<ContentKind> {
    match value {
        1 => Ok(ContentKind::Text),
        2 => Ok(ContentKind::Png),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<ClipSummary> {
    let identity = row.get(2)?;
    let label = row.get(3)?;
    let source = match row.get::<_, i64>(1)? {
        0 => SourceIdentity::Unknown,
        1 => SourceIdentity::Observed {
            app_id: identity,
            display_name: label,
        },
        2 => SourceIdentity::Manual { label },
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(ClipSummary {
        id: row.get(0)?,
        source,
        kind: kind(row.get(4)?)?,
        byte_count: unsigned(row, 5)?,
        first_saved_ms: row.get(6)?,
        last_saved_ms: row.get(7)?,
        tags: Vec::new(),
    })
}

fn unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}
