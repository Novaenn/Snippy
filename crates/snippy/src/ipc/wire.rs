use super::Error;
use zeroize::Zeroizing;
pub const VERSION: u16 = 5;
pub const MAX_FRAME: usize = 4096;
pub const MAX_REPLY: usize = 300_000;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VaultState {
    Locked,
    Unlocking,
    Unlocked,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub revision: u64,
    pub session: u64,
    pub phase: VaultState,
    pub exists: bool,
    pub save_binding: u8,
    pub picker_binding: u8,
    pub capture: u64,
    pub capture_stage: u8,
    pub save_outcome: u8,
    pub gui_mode: u8,
    pub history_revision: u64,
}
/// Requests deliberately have no Debug implementation or diagnostic serialization.
pub enum Request {
    Status,
    Lock,
    Create {
        passphrase: Zeroizing<String>,
        confirmation: Zeroizing<String>,
    },
    Unlock(Zeroizing<String>),
    Watch,
    Activity {
        session: u64,
    },
    OpenGui,
    OpenPicker,
    Browse {
        session: u64,
        query: crate::storage::BrowseQuery,
    },
    Preview {
        session: u64,
        id: i64,
    },
    Copy {
        session: u64,
        id: i64,
    },
    Save,
    Confirm {
        session: u64,
        capture: u64,
        label: Zeroizing<String>,
    },
    CancelSave {
        capture: u64,
    },
    Verify {
        session: u64,
    },
    Config {
        session: u64,
    },
    SetConfig {
        session: u64,
        enabled: bool,
        autostart: bool,
        notifications: bool,
        save: String,
        picker: String,
        max_clip_bytes: u64,
        max_vault_bytes: u64,
        lock_timeout_seconds: u64,
    },
    SensitiveApps {
        session: u64,
    },
    AddSensitive {
        session: u64,
        app_id: String,
    },
    RemoveSensitive {
        session: u64,
        app_id: String,
    },
    AddTag {
        session: u64,
        id: i64,
        tag: String,
    },
    RemoveTag {
        session: u64,
        id: i64,
        tag: String,
    },
    Delete {
        session: u64,
        id: i64,
    },
    DeleteMany {
        session: u64,
        ids: Vec<i64>,
    },
    ManagerInfo {
        session: u64,
    },
}
pub enum Reply {
    State(State),
    Error(Error),
    Present,
    Saved { id: i64, merged: bool },
    Prompt(Prompt),
    Verification(Verification),
    Page(crate::storage::Page),
    Image(crate::storage::ImagePreview),
    AlreadySaved,
    Config(ConfigView),
    SensitiveApps(Vec<String>),
    Tags { id: i64, tags: Vec<String> },
    Deleted { count: u64 },
    ManagerInfo(ManagerInfo),
}
/// Private metadata: never format these structures in logs.
pub struct Prompt {
    pub session: u64,
    pub capture: u64,
    pub sensitive: bool,
    pub source: snippy_core::source::SourceIdentity,
}
pub struct Verification {
    pub session: u64,
    pub items: u64,
    pub logical_bytes: u64,
    pub recent: Option<crate::storage::ClipSummary>,
}
/// Nonsecret preferences safe to show while unlocked.
pub struct ConfigView {
    pub session: u64,
    pub enabled: bool,
    pub autostart: bool,
    pub notifications: bool,
    pub save: String,
    pub picker: String,
    pub max_clip_bytes: u64,
    pub max_vault_bytes: u64,
    pub lock_timeout_seconds: u64,
}
/// Bounded manager metadata; no payloads.
pub struct ManagerInfo {
    pub session: u64,
    pub items: u64,
    pub logical_bytes: u64,
    pub physical_bytes: u64,
    pub sources: Vec<crate::storage::SourceFacet>,
    pub tags: Vec<crate::storage::TagFacet>,
}
impl Request {
    pub fn encode(&self, id: u64) -> Result<Zeroizing<Vec<u8>>, Error> {
        if id == 0 {
            return Err(Error::Protocol);
        }
        let tag = match self {
            Self::Status => 1,
            Self::Lock => 2,
            Self::Create { .. } => 3,
            Self::Unlock(_) => 4,
            Self::Watch => 5,
            Self::Activity { .. } => 6,
            Self::OpenGui => 7,
            Self::Save => 8,
            Self::Confirm { .. } => 9,
            Self::CancelSave { .. } => 10,
            Self::Verify { .. } => 11,
            Self::OpenPicker => 12,
            Self::Browse { .. } => 13,
            Self::Preview { .. } => 14,
            Self::Copy { .. } => 15,
            Self::Config { .. } => 16,
            Self::SetConfig { .. } => 17,
            Self::SensitiveApps { .. } => 18,
            Self::AddSensitive { .. } => 19,
            Self::RemoveSensitive { .. } => 20,
            Self::AddTag { .. } => 21,
            Self::RemoveTag { .. } => 22,
            Self::Delete { .. } => 23,
            Self::DeleteMany { .. } => 24,
            Self::ManagerInfo { .. } => 25,
        };
        // Every request, including credentials, fits one MAX_FRAME allocation.
        let mut out = header(id, tag, MAX_FRAME);
        match self {
            Self::Create {
                passphrase,
                confirmation,
            } => {
                put_secret(&mut out, passphrase)?;
                put_secret(&mut out, confirmation)?
            }
            Self::Unlock(pass) => put_secret(&mut out, pass)?,
            Self::Activity { session }
            | Self::Verify { session }
            | Self::Config { session }
            | Self::SensitiveApps { session }
            | Self::ManagerInfo { session } => out.extend(session.to_be_bytes()),
            Self::SetConfig {
                session,
                enabled,
                autostart,
                notifications,
                save,
                picker,
                max_clip_bytes,
                max_vault_bytes,
                lock_timeout_seconds,
            } => {
                if !(1..=86_400).contains(lock_timeout_seconds) {
                    return Err(Error::Protocol);
                }
                out.extend(session.to_be_bytes());
                out.push(u8::from(*enabled));
                out.push(u8::from(*autostart));
                out.push(u8::from(*notifications));
                put_text(&mut out, save, 64)?;
                put_text(&mut out, picker, 64)?;
                out.extend(max_clip_bytes.to_be_bytes());
                out.extend(max_vault_bytes.to_be_bytes());
                out.extend(lock_timeout_seconds.to_be_bytes());
            }
            Self::AddSensitive { session, app_id } | Self::RemoveSensitive { session, app_id } => {
                out.extend(session.to_be_bytes());
                put_text(&mut out, app_id, 512)?;
            }
            Self::AddTag { session, id, tag } | Self::RemoveTag { session, id, tag } => {
                out.extend(session.to_be_bytes());
                out.extend(id.to_be_bytes());
                put_text(&mut out, tag, 128)?;
            }
            Self::Delete { session, id } => {
                out.extend(session.to_be_bytes());
                out.extend(id.to_be_bytes());
            }
            Self::DeleteMany { session, ids } => {
                if ids.is_empty() || ids.len() > 100 || ids.iter().any(|id| *id <= 0) {
                    return Err(Error::Protocol);
                }
                out.extend(session.to_be_bytes());
                out.push(ids.len() as u8);
                for id in ids {
                    out.extend(id.to_be_bytes());
                }
            }
            Self::Confirm {
                session,
                capture,
                label,
            } => {
                out.extend(session.to_be_bytes());
                out.extend(capture.to_be_bytes());
                put_text(&mut out, label, 256)?;
            }
            Self::Browse { session, query } => {
                query.validate().map_err(|_| Error::Protocol)?;
                out.extend(session.to_be_bytes());
                put_text(&mut out, &query.text, 1024)?;
                put_text(&mut out, &query.source, 512)?;
                out.extend(query.source_id.unwrap_or(0).to_be_bytes());
                put_text(&mut out, &query.tag, 128)?;
                out.push(kind_code(query.kind));
                out.push(u8::from(query.full));
                out.extend(query.offset.to_be_bytes());
            }
            Self::Preview { session, id } | Self::Copy { session, id } => {
                out.extend(session.to_be_bytes());
                out.extend(id.to_be_bytes());
            }
            Self::CancelSave { capture } => out.extend(capture.to_be_bytes()),
            _ => (),
        }
        finish(out)
    }
    pub(crate) fn decode(bytes: &[u8]) -> Result<(u64, Self), Error> {
        if bytes.len() > MAX_FRAME {
            return Err(Error::Protocol);
        }
        let mut c = Cursor { bytes, index: 0 };
        let (id, tag) = c.header()?;
        if id == 0 {
            return Err(Error::Protocol);
        }
        let result = match tag {
            1 => Self::Status,
            2 => Self::Lock,
            3 => Self::Create {
                passphrase: c.secret()?,
                confirmation: c.secret()?,
            },
            4 => Self::Unlock(c.secret()?),
            5 => Self::Watch,
            6 => Self::Activity { session: c.u64()? },
            7 => Self::OpenGui,
            8 => Self::Save,
            9 => Self::Confirm {
                session: c.u64()?,
                capture: c.u64()?,
                label: Zeroizing::new(c.text(256)?),
            },
            10 => Self::CancelSave { capture: c.u64()? },
            11 => Self::Verify { session: c.u64()? },
            12 => Self::OpenPicker,
            13 => {
                let session = c.u64()?;
                let query = crate::storage::BrowseQuery {
                    text: c.text(1024)?,
                    source: c.text(512)?,
                    source_id: match c.u64()? {
                        0 => None,
                        id => Some(i64::try_from(id).map_err(|_| Error::Protocol)?),
                    },
                    tag: c.text(128)?,
                    kind: parse_kind(c.byte()?)?,
                    full: c.boolean()?,
                    offset: c.u64()?,
                };
                query.validate().map_err(|_| Error::Protocol)?;
                Self::Browse { session, query }
            }
            14 => Self::Preview {
                session: c.u64()?,
                id: c.u64()? as i64,
            },
            15 => Self::Copy {
                session: c.u64()?,
                id: c.u64()? as i64,
            },
            16 => Self::Config { session: c.u64()? },
            17 => {
                let session = c.u64()?;
                let enabled = c.boolean()?;
                let autostart = c.boolean()?;
                let notifications = c.boolean()?;
                let save = c.text(64)?;
                let picker = c.text(64)?;
                let max_clip_bytes = c.u64()?;
                let max_vault_bytes = c.u64()?;
                let lock_timeout_seconds = c.u64()?;
                if !(1..=86_400).contains(&lock_timeout_seconds) {
                    return Err(Error::Protocol);
                }
                Self::SetConfig {
                    session,
                    enabled,
                    autostart,
                    notifications,
                    save,
                    picker,
                    max_clip_bytes,
                    max_vault_bytes,
                    lock_timeout_seconds,
                }
            }
            18 => Self::SensitiveApps { session: c.u64()? },
            19 => Self::AddSensitive {
                session: c.u64()?,
                app_id: c.text(512)?,
            },
            20 => Self::RemoveSensitive {
                session: c.u64()?,
                app_id: c.text(512)?,
            },
            21 => Self::AddTag {
                session: c.u64()?,
                id: c.u64()? as i64,
                tag: c.text(128)?,
            },
            22 => Self::RemoveTag {
                session: c.u64()?,
                id: c.u64()? as i64,
                tag: c.text(128)?,
            },
            23 => Self::Delete {
                session: c.u64()?,
                id: c.u64()? as i64,
            },
            24 => {
                let session = c.u64()?;
                let count = c.byte()? as usize;
                if count == 0 || count > 100 {
                    return Err(Error::Protocol);
                }
                let mut ids = Vec::with_capacity(count);
                for _ in 0..count {
                    let id = c.u64()? as i64;
                    if id <= 0 {
                        return Err(Error::Protocol);
                    }
                    ids.push(id);
                }
                Self::DeleteMany { session, ids }
            }
            25 => Self::ManagerInfo { session: c.u64()? },
            _ => return Err(Error::Protocol),
        };
        c.end()?;
        Ok((id, result))
    }
}
impl Reply {
    /// Upper bound of the encoded body, so private replies never reallocate
    /// (which would free unzeroed partial copies).
    fn capacity(&self) -> usize {
        const FIXED: usize = 64;
        let text = |value: &str| 2 + value.len();
        let source = |source: &snippy_core::source::SourceIdentity| {
            use snippy_core::source::SourceIdentity as S;
            1 + match source {
                S::Unknown => 0,
                S::Observed {
                    app_id,
                    display_name,
                } => text(app_id) + text(display_name),
                S::Manual { label } => text(label),
            }
        };
        FIXED
            + match self {
                Self::Prompt(p) => source(&p.source),
                Self::Verification(v) => v.recent.as_ref().map_or(0, |s| FIXED + source(&s.source)),
                Self::Page(page) => page
                    .rows
                    .iter()
                    .map(|row| {
                        FIXED
                            + source(&row.source)
                            + text(&row.text)
                            + row.tags.iter().map(|tag| text(tag)).sum::<usize>()
                    })
                    .sum(),
                Self::Image(preview) => preview.rgba.len(),
                Self::Config(view) => text(&view.save) + text(&view.picker),
                Self::SensitiveApps(values) | Self::Tags { tags: values, .. } => {
                    values.iter().map(|value| text(value)).sum()
                }
                Self::ManagerInfo(info) => {
                    info.sources
                        .iter()
                        .map(|facet| 16 + source(&facet.source))
                        .sum::<usize>()
                        + info
                            .tags
                            .iter()
                            .map(|tag| 8 + text(&tag.name))
                            .sum::<usize>()
                }
                Self::State(_)
                | Self::Error(_)
                | Self::Present
                | Self::Saved { .. }
                | Self::AlreadySaved
                | Self::Deleted { .. } => 0,
            }
    }
    pub(crate) fn encode(&self, id: u64) -> Zeroizing<Vec<u8>> {
        let capacity = self.capacity();
        let mut out = header(
            id,
            match self {
                Self::State(_) => 1,
                Self::Error(_) => 2,
                Self::Present => 3,
                Self::Saved { .. } => 4,
                Self::Prompt(_) => 5,
                Self::Verification(_) => 6,
                Self::Page(_) => 7,
                Self::Image(_) => 8,
                Self::AlreadySaved => 9,
                Self::Config(_) => 10,
                Self::SensitiveApps(_) => 11,
                Self::Tags { .. } => 12,
                Self::Deleted { .. } => 13,
                Self::ManagerInfo(_) => 14,
            },
            capacity,
        );
        match self {
            Self::State(s) => {
                out.extend(s.revision.to_be_bytes());
                out.extend(s.session.to_be_bytes());
                out.extend([
                    match s.phase {
                        VaultState::Locked => 0,
                        VaultState::Unlocking => 1,
                        VaultState::Unlocked => 2,
                    },
                    u8::from(s.exists),
                    s.save_binding,
                    s.picker_binding,
                ]);
                out.extend(s.capture.to_be_bytes());
                out.extend([s.capture_stage, s.save_outcome, s.gui_mode]);
                out.extend(s.history_revision.to_be_bytes())
            }
            Self::Error(e) => out.push(*e as u8),
            Self::Present => (),
            Self::Saved { id, merged } => {
                out.extend(id.to_be_bytes());
                out.push(u8::from(*merged));
            }
            Self::Prompt(p) => {
                out.extend(p.session.to_be_bytes());
                out.extend(p.capture.to_be_bytes());
                out.push(u8::from(p.sensitive));
                put_source(&mut out, &p.source);
            }
            Self::Verification(v) => {
                out.extend(v.session.to_be_bytes());
                out.extend(v.items.to_be_bytes());
                out.extend(v.logical_bytes.to_be_bytes());
                out.push(u8::from(v.recent.is_some()));
                if let Some(s) = &v.recent {
                    out.extend(s.id.to_be_bytes());
                    put_source(&mut out, &s.source);
                    out.push(if s.kind == crate::storage::ContentKind::Text {
                        1
                    } else {
                        2
                    });
                    out.extend(s.byte_count.to_be_bytes());
                    out.extend(s.first_saved_ms.to_be_bytes());
                    out.extend(s.last_saved_ms.to_be_bytes());
                }
            }
            Self::Page(page) => {
                out.extend(page.session.to_be_bytes());
                out.extend(page.offset.to_be_bytes());
                out.push(u8::from(page.more));
                out.push(page.rows.len() as u8);
                for row in &page.rows {
                    out.extend(row.id.to_be_bytes());
                    out.extend(row.source_id.to_be_bytes());
                    put_source(&mut out, &row.source);
                    out.push(kind_code(Some(row.kind)));
                    out.extend(row.byte_count.to_be_bytes());
                    out.extend(row.first_saved_ms.to_be_bytes());
                    out.extend(row.last_saved_ms.to_be_bytes());
                    put_text(&mut out, &row.text, 2048).expect("bounded preview");
                    out.push(row.tags.len() as u8);
                    for tag in &row.tags {
                        put_text(&mut out, tag, 128).expect("bounded tag");
                    }
                }
            }
            Self::Image(preview) => {
                out.extend(preview.session.to_be_bytes());
                out.extend(preview.id.to_be_bytes());
                out.extend(preview.width.to_be_bytes());
                out.extend(preview.height.to_be_bytes());
                out.extend(&*preview.rgba);
            }
            Self::AlreadySaved => (),
            Self::Config(view) => {
                out.extend(view.session.to_be_bytes());
                out.push(u8::from(view.enabled));
                out.push(u8::from(view.autostart));
                out.push(u8::from(view.notifications));
                put_text(&mut out, &view.save, 64).expect("bounded shortcut");
                put_text(&mut out, &view.picker, 64).expect("bounded shortcut");
                out.extend(view.max_clip_bytes.to_be_bytes());
                out.extend(view.max_vault_bytes.to_be_bytes());
                out.extend(view.lock_timeout_seconds.to_be_bytes());
            }
            Self::SensitiveApps(apps) => {
                put_list(&mut out, apps, 128, 512);
            }
            Self::Tags { id, tags } => {
                out.extend(id.to_be_bytes());
                put_list(&mut out, tags, 32, 128);
            }
            Self::Deleted { count } => out.extend(count.to_be_bytes()),
            Self::ManagerInfo(info) => {
                out.extend(info.session.to_be_bytes());
                out.extend(info.items.to_be_bytes());
                out.extend(info.logical_bytes.to_be_bytes());
                out.extend(info.physical_bytes.to_be_bytes());
                out.push(info.sources.len() as u8);
                for source in &info.sources {
                    out.extend(source.id.to_be_bytes());
                    put_source(&mut out, &source.source);
                    out.extend(source.clips.to_be_bytes());
                }
                out.push(info.tags.len() as u8);
                for tag in &info.tags {
                    put_text(&mut out, &tag.name, 128).expect("bounded tag");
                    out.extend(tag.clips.to_be_bytes());
                }
            }
        }
        debug_assert!(out.len() <= capacity, "reply capacity bound");
        finish_limit(out, MAX_REPLY).expect("bounded reply fits protocol")
    }
    pub(crate) fn decode(bytes: &[u8]) -> Result<(u64, Self), Error> {
        let mut c = Cursor { bytes, index: 0 };
        let (id, tag) = c.header()?;
        let reply = match tag {
            1 => {
                let revision = c.u64()?;
                let session = c.u64()?;
                let phase = match c.byte()? {
                    0 => VaultState::Locked,
                    1 => VaultState::Unlocking,
                    2 => VaultState::Unlocked,
                    _ => return Err(Error::Protocol),
                };
                let exists = match c.byte()? {
                    0 => false,
                    1 => true,
                    _ => return Err(Error::Protocol),
                };
                let save_binding = c.byte()?;
                let picker_binding = c.byte()?;
                if save_binding > 2 || picker_binding > 2 {
                    return Err(Error::Protocol);
                }
                Self::State(State {
                    revision,
                    session,
                    phase,
                    exists,
                    save_binding,
                    picker_binding,
                    capture: c.u64()?,
                    capture_stage: c.byte()?,
                    save_outcome: c.byte()?,
                    gui_mode: c.byte()?,
                    history_revision: c.u64()?,
                })
            }
            2 => Self::Error(Error::from_code(c.byte()?).ok_or(Error::Protocol)?),
            3 => Self::Present,
            4 => Self::Saved {
                id: c.u64()? as i64,
                merged: c.boolean()?,
            },
            5 => Self::Prompt(Prompt {
                session: c.u64()?,
                capture: c.u64()?,
                sensitive: c.boolean()?,
                source: c.source()?,
            }),
            6 => {
                let session = c.u64()?;
                let items = c.u64()?;
                let logical_bytes = c.u64()?;
                let recent = if c.boolean()? {
                    Some(crate::storage::ClipSummary {
                        id: c.u64()? as i64,
                        source: c.source()?,
                        kind: match c.byte()? {
                            1 => crate::storage::ContentKind::Text,
                            2 => crate::storage::ContentKind::Png,
                            _ => return Err(Error::Protocol),
                        },
                        byte_count: c.u64()?,
                        first_saved_ms: c.u64()? as i64,
                        last_saved_ms: c.u64()? as i64,
                        tags: Vec::new(),
                    })
                } else {
                    None
                };
                Self::Verification(Verification {
                    session,
                    items,
                    logical_bytes,
                    recent,
                })
            }
            7 => {
                let session = c.u64()?;
                let offset = c.u64()?;
                let more = c.boolean()?;
                let count = c.byte()?;
                if count as usize > crate::storage::PAGE_SIZE {
                    return Err(Error::Protocol);
                }
                let mut rows = Vec::new();
                for _ in 0..count {
                    let id = c.u64()? as i64;
                    let source_id = c.u64()? as i64;
                    let source = c.source()?;
                    let kind = parse_kind(c.byte()?)?.ok_or(Error::Protocol)?;
                    let byte_count = c.u64()?;
                    let first_saved_ms = c.u64()? as i64;
                    let last_saved_ms = c.u64()? as i64;
                    let text = Zeroizing::new(c.text(2048)?);
                    let tags_count = c.byte()?;
                    if tags_count > 32 {
                        return Err(Error::Protocol);
                    }
                    let mut tags = Vec::new();
                    for _ in 0..tags_count {
                        tags.push(c.text(128)?);
                    }
                    rows.push(crate::storage::BrowseRow {
                        id,
                        source_id,
                        source,
                        kind,
                        byte_count,
                        first_saved_ms,
                        last_saved_ms,
                        text,
                        tags,
                    });
                }
                Self::Page(crate::storage::Page {
                    session,
                    offset,
                    more,
                    rows,
                })
            }
            8 => {
                let session = c.u64()?;
                let id = c.u64()? as i64;
                let width = u16::from_be_bytes(c.take(2)?.try_into().map_err(|_| Error::Protocol)?);
                let height =
                    u16::from_be_bytes(c.take(2)?.try_into().map_err(|_| Error::Protocol)?);
                if width == 0 || height == 0 || width > 256 || height > 256 {
                    return Err(Error::Protocol);
                }
                let rgba = Zeroizing::new(c.take(width as usize * height as usize * 4)?.to_vec());
                Self::Image(crate::storage::ImagePreview {
                    session,
                    id,
                    width,
                    height,
                    rgba,
                })
            }
            9 => Self::AlreadySaved,
            10 => {
                let session = c.u64()?;
                let enabled = c.boolean()?;
                let autostart = c.boolean()?;
                let notifications = c.boolean()?;
                let save = c.text(64)?;
                let picker = c.text(64)?;
                let max_clip_bytes = c.u64()?;
                let max_vault_bytes = c.u64()?;
                let lock_timeout_seconds = c.u64()?;
                if !(1..=86_400).contains(&lock_timeout_seconds) {
                    return Err(Error::Protocol);
                }
                Self::Config(ConfigView {
                    session,
                    enabled,
                    autostart,
                    notifications,
                    save,
                    picker,
                    max_clip_bytes,
                    max_vault_bytes,
                    lock_timeout_seconds,
                })
            }
            11 => {
                let count = c.byte()?;
                if count > 128 {
                    return Err(Error::Protocol);
                }
                let mut apps = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    apps.push(c.text(512)?);
                }
                Self::SensitiveApps(apps)
            }
            12 => {
                let id = c.u64()? as i64;
                let count = c.byte()?;
                if count > 32 {
                    return Err(Error::Protocol);
                }
                let mut tags = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    tags.push(c.text(128)?);
                }
                Self::Tags { id, tags }
            }
            13 => Self::Deleted { count: c.u64()? },
            14 => {
                let session = c.u64()?;
                let items = c.u64()?;
                let logical_bytes = c.u64()?;
                let physical_bytes = c.u64()?;
                let source_count = c.byte()?;
                if source_count > 32 {
                    return Err(Error::Protocol);
                }
                let mut sources = Vec::with_capacity(source_count as usize);
                for _ in 0..source_count {
                    sources.push(crate::storage::SourceFacet {
                        id: c.u64()? as i64,
                        source: c.source()?,
                        clips: c.u64()?,
                    });
                }
                let tag_count = c.byte()?;
                if tag_count > 64 {
                    return Err(Error::Protocol);
                }
                let mut tags = Vec::with_capacity(tag_count as usize);
                for _ in 0..tag_count {
                    tags.push(crate::storage::TagFacet {
                        name: c.text(128)?,
                        clips: c.u64()?,
                    });
                }
                Self::ManagerInfo(ManagerInfo {
                    session,
                    items,
                    logical_bytes,
                    physical_bytes,
                    sources,
                    tags,
                })
            }
            _ => return Err(Error::Protocol),
        };
        c.end()?;
        Ok((id, reply))
    }
}
fn header(id: u64, tag: u8, capacity: usize) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::with_capacity(capacity.max(16)));
    out.extend(VERSION.to_be_bytes());
    out.extend(id.to_be_bytes());
    out.push(tag);
    out
}
fn finish(body: Zeroizing<Vec<u8>>) -> Result<Zeroizing<Vec<u8>>, Error> {
    finish_limit(body, MAX_FRAME)
}
fn finish_limit(body: Zeroizing<Vec<u8>>, limit: usize) -> Result<Zeroizing<Vec<u8>>, Error> {
    if body.len() > limit {
        return Err(Error::Protocol);
    }
    let mut out = Zeroizing::new(Vec::with_capacity(body.len() + 4));
    out.extend((body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}
fn kind_code(kind: Option<crate::storage::ContentKind>) -> u8 {
    match kind {
        None => 0,
        Some(crate::storage::ContentKind::Text) => 1,
        Some(crate::storage::ContentKind::Png) => 2,
    }
}
fn parse_kind(code: u8) -> Result<Option<crate::storage::ContentKind>, Error> {
    match code {
        0 => Ok(None),
        1 => Ok(Some(crate::storage::ContentKind::Text)),
        2 => Ok(Some(crate::storage::ContentKind::Png)),
        _ => Err(Error::Protocol),
    }
}
fn put_text(out: &mut Vec<u8>, value: &str, max: usize) -> Result<(), Error> {
    if value.len() > max {
        return Err(Error::InvalidLabel);
    }
    out.extend((value.len() as u16).to_be_bytes());
    out.extend(value.as_bytes());
    Ok(())
}
fn put_list(out: &mut Vec<u8>, values: &[String], max_items: usize, max_bytes: usize) {
    let count = values.len().min(max_items);
    out.push(count as u8);
    for value in &values[..count] {
        put_text(out, value, max_bytes).expect("bounded list entry");
    }
}
fn put_source(out: &mut Vec<u8>, source: &snippy_core::source::SourceIdentity) {
    use snippy_core::source::SourceIdentity as S;
    match source {
        S::Unknown => out.push(0),
        S::Observed {
            app_id,
            display_name,
        } => {
            out.push(1);
            put_text(out, app_id, 512).expect("validated identity");
            put_text(out, display_name, 256).expect("validated label");
        }
        S::Manual { label } => {
            out.push(2);
            put_text(out, label, 256).expect("validated label");
        }
    }
}
fn put_secret(out: &mut Vec<u8>, value: &str) -> Result<(), Error> {
    if value.is_empty() || value.len() > 1024 || value.contains('\0') {
        return Err(Error::InvalidPass);
    }
    out.extend((value.len() as u16).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
    Ok(())
}
struct Cursor<'a> {
    bytes: &'a [u8],
    index: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.index.checked_add(n).ok_or(Error::Protocol)?;
        let value = self.bytes.get(self.index..end).ok_or(Error::Protocol)?;
        self.index = end;
        Ok(value)
    }
    fn boolean(&mut self) -> Result<bool, Error> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::Protocol),
        }
    }
    fn text(&mut self, max: usize) -> Result<String, Error> {
        let n = u16::from_be_bytes(self.take(2)?.try_into().map_err(|_| Error::Protocol)?) as usize;
        if n > max {
            return Err(Error::Protocol);
        }
        Ok(std::str::from_utf8(self.take(n)?)
            .map_err(|_| Error::Protocol)?
            .to_owned())
    }
    fn source(&mut self) -> Result<snippy_core::source::SourceIdentity, Error> {
        use snippy_core::source::SourceIdentity as S;
        match self.byte()? {
            0 => Ok(S::Unknown),
            1 => Ok(S::Observed {
                app_id: self.text(512)?,
                display_name: self.text(256)?,
            }),
            2 => Ok(S::Manual {
                label: self.text(256)?,
            }),
            _ => Err(Error::Protocol),
        }
    }
    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().map_err(|_| Error::Protocol)?,
        ))
    }
    fn header(&mut self) -> Result<(u64, u8), Error> {
        if self.bytes.len() > MAX_REPLY {
            return Err(Error::Protocol);
        }
        let version = u16::from_be_bytes(self.take(2)?.try_into().map_err(|_| Error::Protocol)?);
        if version != VERSION {
            return Err(Error::Version);
        }
        Ok((self.u64()?, self.byte()?))
    }
    fn secret(&mut self) -> Result<Zeroizing<String>, Error> {
        let n = u16::from_be_bytes(self.take(2)?.try_into().map_err(|_| Error::Protocol)?) as usize;
        if n == 0 || n > 1024 {
            return Err(Error::InvalidPass);
        }
        let value = std::str::from_utf8(self.take(n)?).map_err(|_| Error::Protocol)?;
        if value.contains('\0') {
            return Err(Error::InvalidPass);
        }
        Ok(Zeroizing::new(value.to_owned()))
    }
    fn end(&self) -> Result<(), Error> {
        if self.index == self.bytes.len() {
            Ok(())
        } else {
            Err(Error::Protocol)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_framing_version_and_credentials_fail_closed() {
        let valid = Request::Unlock(Zeroizing::new("synthetic".into()))
            .encode(17)
            .unwrap();
        for end in 4..valid.len() {
            assert!(Request::decode(&valid[4..end]).is_err())
        }
        let mut wrong = valid.to_vec();
        wrong[5] = 1;
        assert_eq!(Request::decode(&wrong[4..]).err(), Some(Error::Version));
        let mut extra = valid.to_vec();
        extra.push(0);
        assert_eq!(Request::decode(&extra[4..]).err(), Some(Error::Protocol));
        assert_eq!(
            Request::Unlock(Zeroizing::new("x".repeat(1025)))
                .encode(1)
                .err(),
            Some(Error::InvalidPass)
        );
        let (id, request) = Request::decode(&valid[4..]).unwrap();
        assert_eq!(id, 17);
        assert!(matches!(request, Request::Unlock(_)));
    }
    #[test]
    fn private_prompts_and_bounded_verification_round_trip_without_payloads() {
        use snippy_core::source::SourceIdentity;
        let encoded = Reply::Prompt(Prompt {
            session: 3,
            capture: 7,
            sensitive: true,
            source: SourceIdentity::Observed {
                app_id: "x11:wm-class:KeePassXC".into(),
                display_name: "KeePassXC".into(),
            },
        })
        .encode(0);
        assert!(matches!(
            Reply::decode(&encoded[4..]).unwrap(),
            (
                0,
                Reply::Prompt(Prompt {
                    session: 3,
                    capture: 7,
                    sensitive: true,
                    ..
                })
            )
        ));
        for end in 4..encoded.len() {
            assert!(Reply::decode(&encoded[4..end]).is_err());
        }
        let encoded = Reply::Verification(Verification {
            session: 3,
            items: 4,
            logical_bytes: 100,
            recent: Some(crate::storage::ClipSummary {
                id: 6,
                source: SourceIdentity::Manual {
                    label: "fixture".into(),
                },
                kind: crate::storage::ContentKind::Text,
                byte_count: 25,
                first_saved_ms: 1,
                last_saved_ms: 2,
                tags: Vec::new(),
            }),
        })
        .encode(15);
        assert!(encoded.len() < MAX_FRAME);
        assert!(
            matches!(Reply::decode(&encoded[4..]).unwrap(),(15,Reply::Verification(v)) if v.session==3 && v.items==4 && v.recent.as_ref().is_some_and(|s| s.id==6 && s.byte_count==25))
        );
        assert_eq!(
            Request::Confirm {
                session: 3,
                capture: 7,
                label: Zeroizing::new("x".repeat(257))
            }
            .encode(1)
            .err(),
            Some(Error::InvalidLabel)
        );
    }
    #[test]
    fn picker_frames_bound_pages_pixels_queries_and_sessions() {
        use crate::storage::{BrowseQuery, BrowseRow, ContentKind, ImagePreview, Page};
        use snippy_core::source::SourceIdentity;
        let encoded = Request::Browse {
            session: 7,
            query: BrowseQuery {
                text: "literal OR phrase".into(),
                source: "fixture:editor".into(),
                source_id: Some(9),
                tag: "demo".into(),
                kind: Some(ContentKind::Text),
                full: true,
                offset: 10,
            },
        }
        .encode(8)
        .unwrap();
        assert!(
            matches!(Request::decode(&encoded[4..]).unwrap(),(8,Request::Browse{session:7,query}) if query.offset==10 && query.tag=="demo")
        );
        assert_eq!(
            Request::Browse {
                session: 7,
                query: BrowseQuery {
                    offset: u64::MAX,
                    ..Default::default()
                }
            }
            .encode(8)
            .err(),
            Some(Error::Protocol)
        );
        assert_eq!(
            Request::decode(&vec![0; MAX_FRAME + 1]).err(),
            Some(Error::Protocol)
        );
        let encoded = Reply::Page(Page {
            session: 7,
            offset: 10,
            more: true,
            rows: vec![BrowseRow {
                id: 3,
                source_id: 9,
                source: SourceIdentity::Unknown,
                kind: ContentKind::Text,
                byte_count: 100,
                first_saved_ms: 1,
                last_saved_ms: 2,
                text: Zeroizing::new("synthetic preview".into()),
                tags: vec!["demo".into()],
            }],
        })
        .encode(8);
        assert!(
            matches!(Reply::decode(&encoded[4..]).unwrap(),(8,Reply::Page(page)) if page.session==7 && page.more && page.rows.len()==1 && page.rows[0].tags==["demo"])
        );
        let encoded = Reply::Image(ImagePreview {
            session: 7,
            id: 3,
            width: 256,
            height: 256,
            rgba: Zeroizing::new(vec![42; 256 * 256 * 4]),
        })
        .encode(9);
        assert!(encoded.len() > MAX_FRAME && encoded.len() <= MAX_REPLY + 4);
        assert!(
            matches!(Reply::decode(&encoded[4..]).unwrap(),(9,Reply::Image(image)) if image.rgba.len()==256*256*4)
        );
        assert_eq!(
            Reply::decode(&encoded[4..encoded.len() - 1]).err(),
            Some(Error::Protocol)
        );
        let mut invalid = encoded.to_vec();
        invalid[31..33].copy_from_slice(&257u16.to_be_bytes());
        assert_eq!(Reply::decode(&invalid[4..]).err(), Some(Error::Protocol));
    }
    #[test]
    fn manager_frames_bound_settings_tags_deletion_and_metadata() {
        let encoded = Request::SetConfig {
            session: 9,
            enabled: false,
            autostart: true,
            notifications: true,
            save: "Super+F9".into(),
            picker: "Ctrl+Alt+Y".into(),
            max_clip_bytes: 4096,
            max_vault_bytes: 8190,
            lock_timeout_seconds: 45,
        }
        .encode(3)
        .unwrap();
        assert!(matches!(
            Request::decode(&encoded[4..]).unwrap(),
            (
                3,
                Request::SetConfig {
                    session: 9,
                    enabled: false,
                    autostart: true,
                    notifications: true,
                    lock_timeout_seconds: 45,
                    ..
                }
            )
        ));
        assert_eq!(
            Request::SetConfig {
                session: 9,
                enabled: true,
                autostart: false,
                notifications: false,
                save: "x".repeat(65),
                picker: "Ctrl+Alt+V".into(),
                max_clip_bytes: 4096,
                max_vault_bytes: 8190,
                lock_timeout_seconds: 45,
            }
            .encode(3)
            .err(),
            Some(Error::InvalidLabel)
        );
        let encoded = Request::DeleteMany {
            session: 9,
            ids: vec![1, 2, 3],
        }
        .encode(4)
        .unwrap();
        assert!(matches!(
            Request::decode(&encoded[4..]).unwrap(),
            (4, Request::DeleteMany { session: 9, ids }) if ids == [1, 2, 3]
        ));
        assert_eq!(
            Request::DeleteMany {
                session: 9,
                ids: vec![1; 101]
            }
            .encode(4)
            .err(),
            Some(Error::Protocol)
        );
        let encoded = Request::ManagerInfo { session: 9 }.encode(5).unwrap();
        assert!(matches!(
            Request::decode(&encoded[4..]).unwrap(),
            (5, Request::ManagerInfo { session: 9 })
        ));
        let encoded = Reply::Config(ConfigView {
            session: 9,
            enabled: true,
            autostart: true,
            notifications: false,
            save: "Ctrl+Alt+G".into(),
            picker: "Ctrl+Alt+V".into(),
            max_clip_bytes: 4096,
            max_vault_bytes: 8190,
            lock_timeout_seconds: 45,
        })
        .encode(6);
        assert!(matches!(
            Reply::decode(&encoded[4..]).unwrap(),
            (6, Reply::Config(view))
                if view.session == 9 && view.autostart && view.max_clip_bytes == 4096
        ));
        let encoded = Reply::SensitiveApps(vec!["x11:wm-class:KeePassXC".into()]).encode(7);
        assert!(matches!(
            Reply::decode(&encoded[4..]).unwrap(),
            (7, Reply::SensitiveApps(apps)) if apps.len() == 1
        ));
        let encoded = Reply::Tags {
            id: 4,
            tags: vec!["demo".into(), "even".into()],
        }
        .encode(8);
        assert!(matches!(
            Reply::decode(&encoded[4..]).unwrap(),
            (8, Reply::Tags { id: 4, tags }) if tags == ["demo", "even"]
        ));
        let encoded = Reply::Deleted { count: 2 }.encode(9);
        assert!(matches!(
            Reply::decode(&encoded[4..]).unwrap(),
            (9, Reply::Deleted { count: 2 })
        ));
        let encoded = Reply::ManagerInfo(ManagerInfo {
            session: 9,
            items: 25,
            logical_bytes: 100,
            physical_bytes: 200,
            sources: vec![crate::storage::SourceFacet {
                id: 1,
                source: snippy_core::source::SourceIdentity::Unknown,
                clips: 25,
            }],
            tags: vec![crate::storage::TagFacet {
                name: "demo".into(),
                clips: 25,
            }],
        })
        .encode(10);
        assert!(matches!(
            Reply::decode(&encoded[4..]).unwrap(),
            (10, Reply::ManagerInfo(info))
                if info.physical_bytes == 200 && info.sources.len() == 1 && info.tags[0].name == "demo"
        ));
        let mut invalid = encoded.to_vec();
        invalid[4 + 43] = 33;
        assert_eq!(Reply::decode(&invalid[4..]).err(), Some(Error::Protocol));
    }

    #[test]
    fn response_preserves_request_identity_and_session() {
        let state = State {
            revision: 9,
            session: 3,
            phase: VaultState::Unlocked,
            exists: true,
            save_binding: 1,
            picker_binding: 0,
            capture: 0,
            capture_stage: 0,
            save_outcome: 0,
            gui_mode: 0,
            history_revision: 0,
        };
        let encoded = Reply::State(state).encode(8);
        assert!(
            matches!(Reply::decode(&encoded[4..]).unwrap(), (8, Reply::State(got)) if got == state)
        );
    }
}
