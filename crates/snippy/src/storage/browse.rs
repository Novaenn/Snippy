//! Bounded history read models. No full payload in a page or image preview.
use super::{ClipboardContent, ContentKind, StorageError, Vault};
use rusqlite::{OptionalExtension, params};
use snippy_core::config::MAX_CLIP_BYTES;
use snippy_core::source::SourceIdentity;
use zeroize::Zeroizing;

pub const PAGE_SIZE: usize = 10;
pub const TEXT_PREVIEW_BYTES: usize = 384;
pub const FULL_PREVIEW_BYTES: usize = 2048;
pub(super) const BROWSE_SQL: &str = "SELECT c.id,c.source_id,s.provenance,s.identity,s.label,c.kind,c.byte_count,c.first_saved_ms,c.last_saved_ms,CASE WHEN c.kind=1 THEN substr(c.payload,1,?8) ELSE X'' END FROM clips c JOIN sources s ON s.id=c.source_id WHERE (?1='' OR instr(lower(s.label),lower(?1))>0 OR c.id IN (SELECT clip_id FROM clip_tags JOIN tags ON tags.id=tag_id WHERE instr(lower(name),lower(?1))>0) OR c.id IN (SELECT rowid FROM clip_fts WHERE clip_fts MATCH ?2)) AND (?3='' OR instr(lower(s.identity),lower(?3))>0 OR instr(lower(s.label),lower(?3))>0) AND (?4=0 OR c.source_id=?4) AND (?5='' OR c.id IN (SELECT clip_id FROM clip_tags JOIN tags ON tags.id=tag_id WHERE name=?5)) AND (?6=0 OR c.kind=?6) ORDER BY c.last_saved_ms DESC,c.id DESC LIMIT 11 OFFSET ?7";
#[derive(Default, Clone)]
pub struct BrowseQuery {
    pub text: String,
    pub source: String,
    /// Exact `sources.id` filter from the manager's category list.
    pub source_id: Option<i64>,
    pub tag: String,
    pub kind: Option<ContentKind>,
    /// The manager asks for a larger bounded text prefix than the picker.
    pub full: bool,
    pub offset: u64,
}
impl BrowseQuery {
    pub fn validate(&self) -> Result<(), StorageError> {
        if self.text.len() > 1024
            || self.source.len() > 512
            || self.tag.len() > 128
            || self.source_id.is_some_and(|id| id <= 0)
            || self.offset > i64::MAX as u64 - PAGE_SIZE as u64 - 1
            || [&self.text, &self.source, &self.tag]
                .iter()
                .any(|s| s.contains('\0'))
        {
            return Err(StorageError::InvalidMetadata);
        }
        Ok(())
    }
}
pub struct BrowseRow {
    pub id: i64,
    pub source_id: i64,
    pub source: SourceIdentity,
    pub kind: ContentKind,
    pub byte_count: u64,
    pub first_saved_ms: i64,
    pub last_saved_ms: i64,
    pub text: Zeroizing<String>,
    pub tags: Vec<String>,
}
pub struct Page {
    pub session: u64,
    pub offset: u64,
    pub more: bool,
    pub rows: Vec<BrowseRow>,
}
pub struct ImagePreview {
    pub session: u64,
    pub id: i64,
    pub width: u16,
    pub height: u16,
    pub rgba: Zeroizing<Vec<u8>>,
}
pub struct SourceFacet {
    pub id: i64,
    pub source: SourceIdentity,
    pub clips: u64,
}
pub struct TagFacet {
    pub name: String,
    pub clips: u64,
}
pub struct Facets {
    pub sources: Vec<SourceFacet>,
    pub tags: Vec<TagFacet>,
}
impl Vault {
    pub fn browse(&self, query: &BrowseQuery) -> Result<Page, StorageError> {
        query.validate()?;
        let phrase = Zeroizing::new(format!("\"{}\"", query.text.replace('"', "\"\"")));
        let preview = if query.full {
            FULL_PREVIEW_BYTES
        } else {
            TEXT_PREVIEW_BYTES
        };
        let mut statement = self.conn()?.prepare(BROWSE_SQL)?;
        let mut rows = statement.query(params![
            query.text,
            phrase.as_str(),
            query.source,
            query.source_id.unwrap_or(0),
            query.tag,
            query.kind.map_or(0, ContentKind::code),
            query.offset as i64,
            preview as i64
        ])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            let source = match row.get::<_, i64>(2)? {
                0 => SourceIdentity::Unknown,
                1 => SourceIdentity::Observed {
                    app_id: row.get(3)?,
                    display_name: row.get(4)?,
                },
                2 => SourceIdentity::Manual { label: row.get(4)? },
                _ => return Err(StorageError::Integrity),
            };
            let bytes = Zeroizing::new(row.get::<_, Vec<u8>>(9)?);
            let end = match std::str::from_utf8(&bytes) {
                Ok(_) => bytes.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                Err(_) => return Err(StorageError::InvalidContent),
            };
            let text = Zeroizing::new(
                std::str::from_utf8(&bytes[..end])
                    .map_err(|_| StorageError::InvalidContent)?
                    .to_owned(),
            );
            items.push(BrowseRow {
                id: row.get(0)?,
                source_id: row.get(1)?,
                source,
                kind: match row.get::<_, i64>(5)? {
                    1 => ContentKind::Text,
                    2 => ContentKind::Png,
                    _ => return Err(StorageError::Integrity),
                },
                byte_count: u64::try_from(row.get::<_, i64>(6)?)
                    .map_err(|_| StorageError::Integrity)?,
                first_saved_ms: row.get(7)?,
                last_saved_ms: row.get(8)?,
                text,
                tags: Vec::new(),
            });
        }
        let more = items.len() > PAGE_SIZE;
        items.truncate(PAGE_SIZE);
        for item in &mut items {
            item.tags = self.tags(item.id)?;
        }
        Ok(Page {
            session: 0,
            offset: query.offset,
            more,
            rows: items,
        })
    }
    pub fn image_preview(&self, id: i64) -> Result<ImagePreview, StorageError> {
        let bytes = Zeroizing::new(
            self.conn()?
                .query_row(
                    "SELECT substr(thumbnail,1,600001) FROM clips WHERE id=?1 AND kind=2",
                    [id],
                    |r| r.get::<_, Vec<u8>>(0),
                )
                .optional()?
                .ok_or(StorageError::NotFound)?,
        );
        if bytes.len() > 600_000 {
            return Err(StorageError::Integrity);
        }
        let mut decoder = png::Decoder::new(std::io::Cursor::new(&bytes[..]));
        decoder.set_limits(png::Limits { bytes: 1024 * 1024 });
        let mut reader = decoder
            .read_info()
            .map_err(|_| StorageError::InvalidContent)?;
        let info = reader.info();
        if info.width == 0
            || info.height == 0
            || info.width > 256
            || info.height > 256
            || info.color_type != png::ColorType::Rgba
            || !matches!(
                info.bit_depth,
                png::BitDepth::Eight | png::BitDepth::Sixteen
            )
            || info.animation_control.is_some()
        {
            return Err(StorageError::InvalidContent);
        }
        let (width, height, wide) = (
            info.width as u16,
            info.height as u16,
            info.bit_depth == png::BitDepth::Sixteen,
        );
        let size = reader
            .output_buffer_size()
            .filter(|&n| n <= 524288)
            .ok_or(StorageError::ClipTooLarge)?;
        let mut decoded = Zeroizing::new(vec![0; size]);
        reader
            .next_frame(&mut decoded)
            .map_err(|_| StorageError::InvalidContent)?;
        reader.finish().map_err(|_| StorageError::InvalidContent)?;
        let rgba = if wide {
            Zeroizing::new(decoded.iter().step_by(2).copied().collect())
        } else {
            decoded
        };
        Ok(ImagePreview {
            session: 0,
            id,
            width,
            height,
            rgba,
        })
    }
    /// Explicit reuse loads only content, never attribution/tags/thumbnails.
    pub fn clipboard_content(&self, id: i64) -> Result<ClipboardContent, StorageError> {
        let (kind, bytes): (i64, Vec<u8>) = self
            .conn()?
            .query_row("SELECT kind,payload FROM clips WHERE id=?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?
            .ok_or(StorageError::NotFound)?;
        let bytes = Zeroizing::new(bytes);
        // The user's clip limit applies to new saves only; check stored history
        // against the fixed engineering maximum.
        if bytes.is_empty() || bytes.len() as u64 > MAX_CLIP_BYTES {
            return Err(StorageError::ClipTooLarge);
        }
        let kind = match kind {
            1 => {
                std::str::from_utf8(&bytes).map_err(|_| StorageError::InvalidContent)?;
                ContentKind::Text
            }
            2 => ContentKind::Png,
            _ => return Err(StorageError::Integrity),
        };
        Ok(ClipboardContent { kind, bytes })
    }

    /// Bounded category and tag lists for the manager. Metadata only.
    pub fn facets(&self) -> Result<Facets, StorageError> {
        let connection = self.conn()?;
        let mut sources = Vec::new();
        {
            let mut statement = connection.prepare("SELECT s.id,s.provenance,s.identity,s.label,count(c.id) FROM sources s JOIN clips c ON c.source_id=s.id GROUP BY s.id ORDER BY count(c.id) DESC,s.label LIMIT 32")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let source = match row.get::<_, i64>(1)? {
                    0 => SourceIdentity::Unknown,
                    1 => SourceIdentity::Observed {
                        app_id: row.get(2)?,
                        display_name: row.get(3)?,
                    },
                    2 => SourceIdentity::Manual { label: row.get(3)? },
                    _ => return Err(StorageError::Integrity),
                };
                sources.push(SourceFacet {
                    id: row.get(0)?,
                    source,
                    clips: u64::try_from(row.get::<_, i64>(4)?)
                        .map_err(|_| StorageError::Integrity)?,
                });
            }
        }
        let mut tags = Vec::new();
        {
            let mut statement = connection.prepare("SELECT t.name,count(ct.clip_id) FROM tags t JOIN clip_tags ct ON ct.tag_id=t.id GROUP BY t.id ORDER BY count(ct.clip_id) DESC,t.name LIMIT 64")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                tags.push(TagFacet {
                    name: row.get(0)?,
                    clips: u64::try_from(row.get::<_, i64>(1)?)
                        .map_err(|_| StorageError::Integrity)?,
                });
            }
        }
        Ok(Facets { sources, tags })
    }
}
