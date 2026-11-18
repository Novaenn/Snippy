//! Service-owned clipboard. Holds content only, never vault keys or source data.
use super::{X11Error, request};
use crate::storage::{ClipboardContent, ContentKind};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::{
        Event,
        xproto::{
            AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask,
            PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent,
            SelectionRequestEvent, WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};
const MAX_TRANSFERS: usize = 4;
const IDLE: Duration = Duration::from_secs(5);
const TOTAL: Duration = Duration::from_secs(30);
struct Outgoing {
    window: u32,
    property: u32,
    kind: u32,
    content: Arc<ClipboardContent>,
    offset: usize,
    started: Instant,
    progress: Instant,
    finished: bool,
}
pub(super) struct Serving {
    pub window: u32,
    clipboard: u32,
    targets: u32,
    timestamp: u32,
    multiple: u32,
    pair: u32,
    incr: u32,
    utf8: u32,
    mime: u32,
    plain: u32,
    text: u32,
    png: u32,
    acquired: u32,
    acquired_sequence: u16,
    current: Option<Arc<ClipboardContent>>,
    transfers: Vec<Outgoing>,
    masks: HashMap<u32, EventMask>,
    chunk: usize,
}
impl Serving {
    pub fn new(c: &RustConnection, root: u32, clipboard: u32) -> Result<Self, X11Error> {
        let window = c.generate_id().map_err(request)?;
        c.create_window(
            0,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new(),
        )
        .map_err(request)?
        .check()
        .map_err(request)?;
        let atom = |name: &[u8]| {
            c.intern_atom(false, name)
                .map_err(request)?
                .reply()
                .map(|r| r.atom)
                .map_err(request)
        };
        Ok(Self {
            window,
            clipboard,
            targets: atom(b"TARGETS")?,
            timestamp: atom(b"TIMESTAMP")?,
            multiple: atom(b"MULTIPLE")?,
            pair: atom(b"ATOM_PAIR")?,
            incr: atom(b"INCR")?,
            utf8: atom(b"UTF8_STRING")?,
            mime: atom(b"text/plain;charset=utf-8")?,
            plain: atom(b"text/plain")?,
            text: atom(b"TEXT")?,
            png: atom(b"image/png")?,
            acquired: 0,
            acquired_sequence: 0,
            current: None,
            transfers: Vec::new(),
            masks: HashMap::new(),
            chunk: c
                .maximum_request_bytes()
                .saturating_sub(64)
                .clamp(1, 64 * 1024),
        })
    }
    pub fn offer(&mut self, c: &RustConnection, content: ClipboardContent) -> Result<(), X11Error> {
        let cookie = c
            .set_selection_owner(self.window, self.clipboard, x11rb::CURRENT_TIME)
            .map_err(request)?;
        self.acquired_sequence = cookie.sequence_number() as u16;
        cookie.check().map_err(request)?;
        if c.get_selection_owner(self.clipboard)
            .map_err(request)?
            .reply()
            .map_err(request)?
            .owner
            != self.window
        {
            return Err(X11Error::Request);
        }
        self.current = Some(Arc::new(content));
        self.acquired = 0;
        c.flush().map_err(request)
    }
    pub fn owns(&self, c: &RustConnection) -> bool {
        self.current.is_some()
            && c.get_selection_owner(self.clipboard)
                .ok()
                .and_then(|r| r.reply().ok())
                .is_some_and(|r| r.owner == self.window)
    }
    pub fn owner(&mut self, owner: u32) {
        if owner != self.window {
            self.current = None;
        }
    }
    pub fn acquisition(&mut self, sequence: u16, owner: u32, time: u32) {
        if owner == self.window && sequence == self.acquired_sequence {
            self.acquired = time;
        }
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.transfers
            .iter()
            .map(|t| (t.started + TOTAL).min(t.progress + IDLE))
            .min()
    }
    pub fn expire(&mut self, c: &RustConnection) {
        self.expire_at(c, Instant::now());
    }
    fn expire_at(&mut self, c: &RustConnection, now: Instant) {
        self.transfers
            .retain(|t| !t.finished && now < t.started + TOTAL && now < t.progress + IDLE);
        self.release_masks(c);
    }
    fn release_masks(&mut self, c: &RustConnection) {
        if self.masks.is_empty() {
            return;
        }
        self.masks.retain(|window, mask| {
            if self.transfers.iter().any(|t| t.window == *window) {
                true
            } else {
                let _ = c.change_window_attributes(
                    *window,
                    &ChangeWindowAttributesAux::new().event_mask(*mask),
                );
                false
            }
        });
        let _ = c.flush();
    }
    pub fn event(&mut self, c: &RustConnection, event: &Event) {
        // An acknowledgement arriving after its deadline must not revive a lease.
        self.expire(c);
        match event {
            Event::SelectionRequest(e)
                if e.response_type & 0x80 == 0
                    && e.owner == self.window
                    && e.selection == self.clipboard =>
            {
                let property = if e.property == 0 {
                    e.target
                } else {
                    e.property
                };
                let valid = self.current.is_some()
                    && self.acquired != 0
                    && (e.time == 0 || time_after(e.time, self.acquired));
                let success = valid
                    && if e.target == self.multiple {
                        e.property != 0 && self.multiple(c, e).is_ok()
                    } else {
                        self.convert(c, e.requestor, e.target, property).is_ok()
                    };
                let notify = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: e.time,
                    requestor: e.requestor,
                    selection: e.selection,
                    target: e.target,
                    property: if success { property } else { 0 },
                };
                let _ = c.send_event(false, e.requestor, EventMask::NO_EVENT, notify);
                let _ = c.flush();
            }
            Event::PropertyNotify(e)
                if e.response_type & 0x80 == 0 && e.state == Property::DELETE =>
            {
                if let Some(t) = self
                    .transfers
                    .iter_mut()
                    .find(|t| t.window == e.window && t.property == e.atom)
                {
                    let end = (t.offset + self.chunk).min(t.content.bytes.len());
                    let done = t.offset == end;
                    let result = c
                        .change_property8(
                            PropMode::REPLACE,
                            t.window,
                            t.property,
                            t.kind,
                            &t.content.bytes[t.offset..end],
                        )
                        .map_err(request)
                        .and_then(|r| r.check().map_err(request));
                    t.offset = end;
                    t.progress = Instant::now();
                    t.finished = done || result.is_err();
                    let _ = c.flush();
                }
            }
            Event::DestroyNotify(e) if e.response_type & 0x80 == 0 => {
                self.transfers.retain(|t| t.window != e.window);
                self.masks.remove(&e.window);
            }
            _ => (),
        }
        self.expire(c);
    }
    fn convert(
        &mut self,
        c: &RustConnection,
        window: u32,
        target: u32,
        property: u32,
    ) -> Result<(), X11Error> {
        if property == 0
            || self
                .transfers
                .iter()
                .any(|t| t.window == window && t.property == property)
        {
            return Err(X11Error::Request);
        }
        let content = self.current.as_ref().ok_or(X11Error::Request)?.clone();
        if target == self.targets {
            let mut targets = vec![self.targets, self.timestamp, self.multiple];
            if content.kind == ContentKind::Png {
                targets.push(self.png);
            } else {
                targets.extend([self.utf8, self.mime, self.plain, self.text]);
            }
            c.change_property32(
                PropMode::REPLACE,
                window,
                property,
                AtomEnum::ATOM,
                &targets,
            )
            .map_err(request)?
            .check()
            .map_err(request)?;
        } else if target == self.timestamp {
            c.change_property32(
                PropMode::REPLACE,
                window,
                property,
                AtomEnum::INTEGER,
                &[self.acquired],
            )
            .map_err(request)?
            .check()
            .map_err(request)?;
        } else {
            let kind = if content.kind == ContentKind::Png && target == self.png {
                self.png
            } else if content.kind == ContentKind::Text
                && [self.utf8, self.mime, self.plain, self.text].contains(&target)
            {
                if target == self.text {
                    self.utf8
                } else {
                    target
                }
            } else {
                return Err(X11Error::Request);
            };
            if content.bytes.len() <= self.chunk {
                c.change_property8(PropMode::REPLACE, window, property, kind, &content.bytes)
                    .map_err(request)?
                    .check()
                    .map_err(request)?;
            } else {
                if self.transfers.len() >= MAX_TRANSFERS {
                    return Err(X11Error::Request);
                }
                if let std::collections::hash_map::Entry::Vacant(entry) = self.masks.entry(window) {
                    let mask = c
                        .get_window_attributes(window)
                        .map_err(request)?
                        .reply()
                        .map_err(request)?
                        .your_event_mask;
                    c.change_window_attributes(
                        window,
                        &ChangeWindowAttributesAux::new().event_mask(
                            mask | EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY,
                        ),
                    )
                    .map_err(request)?
                    .check()
                    .map_err(request)?;
                    entry.insert(mask);
                }
                c.change_property32(
                    PropMode::REPLACE,
                    window,
                    property,
                    self.incr,
                    &[content.bytes.len() as u32],
                )
                .map_err(request)?
                .check()
                .map_err(request)?;
                let now = Instant::now();
                self.transfers.push(Outgoing {
                    window,
                    property,
                    kind,
                    content,
                    offset: 0,
                    started: now,
                    progress: now,
                    finished: false,
                });
            }
        }
        Ok(())
    }
    fn multiple(&mut self, c: &RustConnection, e: &SelectionRequestEvent) -> Result<(), X11Error> {
        let reply = c
            .get_property(false, e.requestor, e.property, self.pair, 0, 16)
            .map_err(request)?
            .reply()
            .map_err(request)?;
        if reply.format != 32 || reply.type_ != self.pair || reply.bytes_after != 0 {
            return Err(X11Error::Request);
        }
        let mut pairs = reply
            .value32()
            .ok_or(X11Error::Request)?
            .collect::<Vec<_>>();
        if pairs.len() % 2 != 0 {
            return Err(X11Error::Request);
        }
        let mut properties = Vec::new();
        for pair in pairs.as_chunks_mut::<2>().0.iter_mut() {
            if pair[0] == self.multiple
                || pair[1] == 0
                || pair[1] == e.property
                || properties.contains(&pair[1])
                || self.convert(c, e.requestor, pair[0], pair[1]).is_err()
            {
                pair[0] = 0;
            } else {
                properties.push(pair[1]);
            }
        }
        c.change_property32(
            PropMode::REPLACE,
            e.requestor,
            e.property,
            self.pair,
            &pairs,
        )
        .map_err(request)?
        .check()
        .map_err(request)
    }
}
fn time_after(time: u32, acquired: u32) -> bool {
    time.wrapping_sub(acquired) as i32 >= 0
}
#[cfg(test)]
mod tests {
    use super::*;
    fn native() -> (super::super::Observer, RustConnection, u32, u32, u32) {
        assert_eq!(
            std::env::var("SNIPPY_TEST_X11").as_deref(),
            Ok("1"),
            "Use check-picker.sh on private Xvfb"
        );
        let mut observer = super::super::Observer::connect(&snippy_core::config::ShortcutConfig {
            enabled: false,
            ..Default::default()
        })
        .unwrap();
        observer
            .serve(ClipboardContent {
                kind: ContentKind::Text,
                bytes: zeroize::Zeroizing::new(vec![b'R'; 180_000]),
            })
            .unwrap();
        observer.synchronize().unwrap();
        while observer.poll().unwrap().is_some() {}
        let (c, screen) = x11rb::connect(None).unwrap();
        let window = c.generate_id().unwrap();
        c.create_window(
            0,
            window,
            c.setup().roots[screen].root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .unwrap()
        .check()
        .unwrap();
        let clipboard = c
            .intern_atom(false, b"CLIPBOARD")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let utf8 = c
            .intern_atom(false, b"UTF8_STRING")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        (observer, c, window, clipboard, utf8)
    }
    fn notify(observer: &mut super::super::Observer, c: &RustConnection) -> SelectionNotifyEvent {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            while observer.poll().unwrap().is_some() {}
            while let Some(event) = c.poll_for_event().unwrap() {
                if let Event::SelectionNotify(e) = event {
                    return e;
                }
            }
            assert!(Instant::now() < deadline, "synthetic request timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    #[ignore = "private Xvfb required; use check-picker.sh"]
    fn native_slow_receivers_are_bounded_and_expiry_releases_old_payloads() {
        let (mut observer, c, window, clipboard, utf8) = native();
        let weak = Arc::downgrade(observer.serving.current.as_ref().unwrap());
        for n in 0..5 {
            let property = c
                .intern_atom(false, format!("_SYNTHETIC_{n}").as_bytes())
                .unwrap()
                .reply()
                .unwrap()
                .atom;
            c.convert_selection(window, clipboard, utf8, property, x11rb::CURRENT_TIME)
                .unwrap();
            c.flush().unwrap();
            let reply = notify(&mut observer, &c);
            assert_eq!(reply.property, if n < 4 { property } else { 0 });
        }
        assert_eq!(observer.serving.transfers.len(), 4);
        c.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        observer.synchronize().unwrap();
        while observer.poll().unwrap().is_some() {}
        assert!(observer.serving.current.is_none());
        assert!(weak.upgrade().is_some());
        observer.serving.expire_at(
            &observer.connection,
            Instant::now() + IDLE + Duration::from_millis(1),
        );
        assert!(observer.serving.transfers.is_empty());
        assert!(observer.serving.masks.is_empty());
        assert!(weak.upgrade().is_none());
    }
    #[test]
    #[ignore = "private Xvfb required; use check-picker.sh"]
    fn native_incremental_transfer_finishes_after_clipboard_replacement() {
        let (mut observer, c, window, clipboard, utf8) = native();
        let property = c
            .intern_atom(false, b"_SYNTHETIC_TRANSFER")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        c.convert_selection(window, clipboard, utf8, property, x11rb::CURRENT_TIME)
            .unwrap();
        c.flush().unwrap();
        assert_eq!(notify(&mut observer, &c).property, property);
        let header = c
            .get_property(true, window, property, AtomEnum::ANY, 0, 1)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(header.type_, observer.serving.incr);
        c.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        c.flush().unwrap();
        let mut bytes = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            while observer.poll().unwrap().is_some() {}
            let mut finished = false;
            while let Some(event) = c.poll_for_event().unwrap() {
                if matches!(event,Event::PropertyNotify(e) if e.atom==property && e.state==Property::NEW_VALUE)
                {
                    let chunk = c
                        .get_property(true, window, property, AtomEnum::ANY, 0, 32 * 1024)
                        .unwrap()
                        .reply()
                        .unwrap();
                    assert_eq!(chunk.type_, utf8);
                    assert_eq!(chunk.bytes_after, 0);
                    if chunk.value.is_empty() {
                        finished = true;
                        break;
                    }
                    bytes.extend(chunk.value);
                }
            }
            if finished {
                break;
            }
            assert!(Instant::now() < deadline, "incremental transfer timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(bytes, vec![b'R'; 180_000]);
        while observer.poll().unwrap().is_some() {}
        assert!(observer.serving.current.is_none());
        assert!(observer.serving.transfers.is_empty());
    }
    #[test]
    #[ignore = "private Xvfb required; use check-picker.sh"]
    fn native_required_targets_and_multiple_report_independent_failures() {
        let (mut observer, c, window, clipboard, _) = native();
        let property = c
            .intern_atom(false, b"_SYNTHETIC_MULTIPLE")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let a = c
            .intern_atom(false, b"_SYNTHETIC_A")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let b = c
            .intern_atom(false, b"_SYNTHETIC_B")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let pairs = [
            observer.serving.timestamp,
            a,
            u32::from(AtomEnum::PIXMAP),
            b,
        ];
        c.change_property32(
            PropMode::REPLACE,
            window,
            property,
            observer.serving.pair,
            &pairs,
        )
        .unwrap();
        c.convert_selection(
            window,
            clipboard,
            observer.serving.multiple,
            property,
            x11rb::CURRENT_TIME,
        )
        .unwrap();
        c.flush().unwrap();
        assert_eq!(notify(&mut observer, &c).property, property);
        let result = c
            .get_property(true, window, property, AtomEnum::ANY, 0, 4)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(
            result.value32().unwrap().collect::<Vec<_>>(),
            [observer.serving.timestamp, a, 0, b]
        );
        let timestamp = c
            .get_property(true, window, a, AtomEnum::INTEGER, 0, 1)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!(
            timestamp.value32().unwrap().next(),
            Some(observer.serving.acquired)
        );
        assert_ne!(observer.serving.acquired, 0);
        c.convert_selection(
            window,
            clipboard,
            observer.serving.targets,
            property,
            x11rb::CURRENT_TIME,
        )
        .unwrap();
        c.flush().unwrap();
        notify(&mut observer, &c);
        let targets = c
            .get_property(true, window, property, AtomEnum::ATOM, 0, 32)
            .unwrap()
            .reply()
            .unwrap();
        let targets = targets.value32().unwrap().collect::<Vec<_>>();
        for target in [
            observer.serving.timestamp,
            observer.serving.multiple,
            observer.serving.targets,
            observer.serving.utf8,
        ] {
            assert!(targets.contains(&target));
        }
    }
    #[test]
    fn timestamps_compare_across_x11_wrap() {
        assert!(time_after(3, u32::MAX - 5));
        assert!(!time_after(u32::MAX - 5, 3));
        assert!(time_after(10, 10));
    }
}
