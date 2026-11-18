//! Synthetic owner shared by isolated tests and the explicit manual demo.
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask,
            PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, WindowClass,
        },
    },
    wrapper::ConnectionExt as _,
};

#[derive(Default)]
pub struct Offers {
    pub class: Option<String>,
    pub text: Vec<u8>,
    pub png: Option<Vec<u8>>,
    pub incr: bool,
    pub hung: bool,
    pub unsupported: bool,
    pub latin1: bool,
    pub bad_incr_type: bool,
    pub oversized_hint: bool,
    pub truncated: bool,
}
pub struct Owner {
    pub window: u32,
    pub requests: Arc<AtomicUsize>,
    pub payloads: Arc<AtomicUsize>,
    command: SyncSender<Option<SyncSender<()>>>,
    join: Option<JoinHandle<()>>,
}
impl Owner {
    pub fn new(offers: Offers) -> Self {
        let (command, receive) = mpsc::sync_channel::<Option<SyncSender<()>>>(4);
        let (ready, created) = mpsc::sync_channel(1);
        let requests = Arc::new(AtomicUsize::new(0));
        let payloads = Arc::new(AtomicUsize::new(0));
        let counts = requests.clone();
        let data_counts = payloads.clone();
        let join = thread::spawn(move || {
            let (c, screen) = x11rb::connect(None).unwrap();
            let root = c.setup().roots[screen].root;
            let window = c.generate_id().unwrap();
            c.create_window(
                0,
                window,
                root,
                0,
                0,
                10,
                10,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
            if let Some(class) = &offers.class {
                c.change_property8(
                    PropMode::REPLACE,
                    window,
                    AtomEnum::WM_CLASS,
                    AtomEnum::STRING,
                    format!("fixture\0{class}\0").as_bytes(),
                )
                .unwrap()
                .check()
                .unwrap();
            }
            let atom = |name: &[u8]| c.intern_atom(false, name).unwrap().reply().unwrap().atom;
            let clipboard = atom(b"CLIPBOARD");
            let targets = atom(b"TARGETS");
            let utf8 = atom(b"UTF8_STRING");
            let png = atom(b"image/png");
            let incr = atom(b"INCR");
            c.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME)
                .unwrap()
                .check()
                .unwrap();
            c.flush().unwrap();
            ready.send(window).unwrap();
            struct Pending {
                data: Vec<u8>,
                offset: usize,
                target: u32,
            }
            let mut pending = HashMap::<(u32, u32), Pending>::new();
            loop {
                match receive.try_recv() {
                    Ok(Some(done)) => {
                        c.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME)
                            .unwrap()
                            .check()
                            .unwrap();
                        c.flush().unwrap();
                        let _ = done.send(());
                    }
                    Ok(None) | Err(mpsc::TryRecvError::Disconnected) => break,
                    Err(mpsc::TryRecvError::Empty) => (),
                }
                while let Some(event) = c.poll_for_event().unwrap() {
                    match event {
                        Event::SelectionRequest(e) => {
                            counts.fetch_add(1, Ordering::SeqCst);
                            if e.target != targets {
                                data_counts.fetch_add(1, Ordering::SeqCst);
                            }
                            if offers.hung {
                                continue;
                            }
                            let property = if e.property == 0 {
                                e.target
                            } else {
                                e.property
                            };
                            let mut success = true;
                            if e.target == targets {
                                let mut types = Vec::new();
                                if !offers.unsupported {
                                    types.push(if offers.latin1 {
                                        u32::from(AtomEnum::STRING)
                                    } else {
                                        utf8
                                    });
                                    if offers.png.is_some() {
                                        types.push(png);
                                    }
                                }
                                let _ = c.change_property32(
                                    PropMode::REPLACE,
                                    e.requestor,
                                    property,
                                    AtomEnum::ATOM,
                                    &types,
                                );
                            } else {
                                let data = if e.target == png {
                                    offers.png.as_deref()
                                } else if e.target == utf8
                                    || e.target == u32::from(AtomEnum::STRING)
                                {
                                    Some(offers.text.as_slice())
                                } else {
                                    None
                                };
                                if let Some(data) = data {
                                    if offers.incr {
                                        let _ = c.change_window_attributes(
                                            e.requestor,
                                            &ChangeWindowAttributesAux::new()
                                                .event_mask(EventMask::PROPERTY_CHANGE),
                                        );
                                        let hint = if offers.oversized_hint {
                                            u32::MAX
                                        } else {
                                            data.len() as u32
                                        };
                                        let _ = c.change_property32(
                                            PropMode::REPLACE,
                                            e.requestor,
                                            property,
                                            incr,
                                            &[hint],
                                        );
                                        pending.insert(
                                            (e.requestor, property),
                                            Pending {
                                                data: data.to_vec(),
                                                offset: 0,
                                                target: e.target,
                                            },
                                        );
                                    } else {
                                        let _ = c.change_property8(
                                            PropMode::REPLACE,
                                            e.requestor,
                                            property,
                                            e.target,
                                            data,
                                        );
                                    }
                                } else {
                                    success = false;
                                }
                            }
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
                        }
                        Event::PropertyNotify(e) if e.state == Property::DELETE => {
                            if let Some(p) = pending.get_mut(&(e.window, e.atom)) {
                                if offers.truncated && p.offset != 0 {
                                    continue;
                                }
                                let end = (p.offset + 4096).min(p.data.len());
                                let kind = if offers.bad_incr_type && p.offset != 0 {
                                    u32::from(AtomEnum::STRING)
                                } else {
                                    p.target
                                };
                                let _ = c.change_property8(
                                    PropMode::REPLACE,
                                    e.window,
                                    e.atom,
                                    kind,
                                    &p.data[p.offset..end],
                                );
                                if end == p.offset {
                                    pending.remove(&(e.window, e.atom));
                                } else {
                                    p.offset = end;
                                }
                            }
                        }
                        _ => (),
                    }
                }
                c.flush().unwrap();
                thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            window: created.recv().unwrap(),
            requests,
            payloads,
            command,
            join: Some(join),
        }
    }
    pub fn copy_again(&self) {
        let (send, done) = mpsc::sync_channel(1);
        self.command.send(Some(send)).unwrap();
        done.recv().unwrap();
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.command.send(None);
        if let Some(join) = self.join.take() {
            join.join().unwrap();
        }
    }
}
