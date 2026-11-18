//! Passive ownership metadata/shortcuts and a separately authorized capture adapter.

pub mod capture;
mod placement;
mod resolver;
mod serving;
mod shortcuts;

pub use shortcuts::{Action, BindingState, BindingStatus};
use snippy_core::{config::ShortcutConfig, source::SourceIdentity};
use std::{error::Error, fmt};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        res::ConnectionExt as _,
        xfixes::{ConnectionExt as _, SelectionEventMask},
        xproto::{ConnectionExt as _, CreateWindowAux, Mapping, WindowClass},
    },
    rust_connection::RustConnection,
};

/// Current private metadata only; deliberately has no Debug implementation.
#[derive(Clone)]
pub struct Snapshot {
    pub owner: u32,
    pub generation: u64,
    pub selection_timestamp: Option<u32>,
    pub source: SourceIdentity,
    pub unknown_reason: Option<UnknownReason>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnknownReason {
    Empty,
    MissingMetadata,
    ClipboardProxy,
    AmbiguousClient,
    ResolutionBudget,
}

impl UnknownReason {
    pub fn description(self) -> &'static str {
        match self {
            Self::Empty => "Clipboard has no owner",
            Self::MissingMetadata => "Owner has no reliable app metadata",
            Self::ClipboardProxy => "Clipboard proxy obscures the original app",
            Self::AmbiguousClient => "Process or client has multiple app identities",
            Self::ResolutionBudget => "Source lookup exceeded its metadata budget",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Update {
    Ownership,
    Shortcut(Action),
    Bindings,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum X11Error {
    Connect,
    MissingXfixes,
    Request,
    KeyboardMap,
}

impl fmt::Display for X11Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Connect => "could not connect to the X11 display",
            Self::MissingXfixes => "XFixes 1.0 or newer is required; polling is not supported",
            Self::Request => "X11 metadata request or connection failed",
            Self::KeyboardMap => "could not read a supported X11 keyboard mapping",
        })
    }
}
impl Error for X11Error {}
pub(crate) fn request<E>(_: E) -> X11Error {
    X11Error::Request
}

/// One connection, one helper window, and one current snapshot. No history.
pub struct Observer {
    connection: RustConnection,
    root: u32,
    window: u32,
    clipboard: u32,
    client_list: u32,
    has_res: bool,
    has_res_pids: bool,
    snapshot: Snapshot,
    shortcuts: shortcuts::Shortcuts,
    serving: serving::Serving,
}

impl Observer {
    /// Readiness integration; use poll() to consume buffered protocol events.
    pub fn event_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.connection.stream().as_fd()
    }
    pub fn connect(config: &ShortcutConfig) -> Result<Self, X11Error> {
        let (connection, screen) = x11rb::connect(None).map_err(|_| X11Error::Connect)?;
        let root = connection.setup().roots[screen].root;
        if !connection
            .query_extension(b"XFIXES")
            .map_err(request)?
            .reply()
            .map_err(request)?
            .present
        {
            return Err(X11Error::MissingXfixes);
        }
        let version = connection
            .xfixes_query_version(5, 0)
            .map_err(request)?
            .reply()
            .map_err(request)?;
        if version.major_version < 1 {
            return Err(X11Error::MissingXfixes);
        }
        let res_present = connection
            .query_extension(b"X-Resource")
            .map_err(request)?
            .reply()
            .map_err(request)?
            .present;
        let res_version = if res_present {
            connection
                .res_query_version(1, 2)
                .map_err(request)?
                .reply()
                .ok()
        } else {
            None
        };
        let has_res = res_version.is_some();
        let has_res_pids = res_version
            .is_some_and(|version| (version.server_major, version.server_minor) >= (1, 2));
        let window = connection.generate_id().map_err(request)?;
        connection
            .create_window(
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
        let clipboard = connection
            .intern_atom(false, b"CLIPBOARD")
            .map_err(request)?
            .reply()
            .map_err(request)?
            .atom;
        let client_list = connection
            .intern_atom(false, b"_NET_CLIENT_LIST")
            .map_err(request)?
            .reply()
            .map_err(request)?
            .atom;
        connection
            .xfixes_select_selection_input(
                window,
                clipboard,
                SelectionEventMask::SET_SELECTION_OWNER
                    | SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | SelectionEventMask::SELECTION_CLIENT_CLOSE,
            )
            .map_err(request)?
            .check()
            .map_err(request)?;
        let serving = serving::Serving::new(&connection, root, clipboard)?;
        let mut observer = Self {
            connection,
            root,
            window,
            clipboard,
            client_list,
            has_res,
            has_res_pids,
            snapshot: Snapshot {
                owner: 0,
                generation: 0,
                selection_timestamp: None,
                source: SourceIdentity::Unknown,
                unknown_reason: Some(UnknownReason::Empty),
            },
            shortcuts: shortcuts::Shortcuts::new(config.clone()),
            serving,
        };
        observer.refresh(None)?;
        observer
            .shortcuts
            .bind(&observer.connection, observer.root)?;
        Ok(observer)
    }

    /// A server round trip orders earlier ownership notifications before a
    /// capture checkpoint. This requests no selection representation.
    pub fn synchronize(&self) -> Result<(), X11Error> {
        self.connection
            .get_selection_owner(self.clipboard)
            .map_err(request)?
            .reply()
            .map_err(request)?;
        Ok(())
    }

    /// Rebind built-in shortcuts after a settings change on the same connection.
    pub fn set_shortcuts(
        &mut self,
        config: &snippy_core::config::ShortcutConfig,
    ) -> Result<(), X11Error> {
        self.shortcuts.set_config(config.clone());
        self.shortcuts.bind(&self.connection, self.root)
    }
    pub fn serve(&mut self, content: crate::storage::ClipboardContent) -> Result<(), X11Error> {
        self.serving.offer(&self.connection, content)
    }
    pub fn serving_current(&self) -> bool {
        self.serving.owns(&self.connection)
    }
    pub fn serving_deadline(&self) -> Option<std::time::Instant> {
        self.serving.deadline()
    }
    pub fn expire_serving(&mut self) {
        self.serving.expire(&self.connection)
    }
    pub fn focused_window(&self) -> Option<u32> {
        self.connection
            .get_input_focus()
            .ok()?
            .reply()
            .ok()
            .map(|r| r.focus)
            .filter(|&w| w > 1)
    }
    pub fn position_gui(&self, pid: u32) {
        placement::position(
            &self.connection,
            self.root,
            self.client_list,
            pid,
            self.has_res_pids,
        );
    }
    pub fn restore_focus(&self, window: u32) {
        use x11rb::protocol::xproto::{InputFocus, MapState};
        if self
            .connection
            .get_window_attributes(window)
            .ok()
            .and_then(|r| r.reply().ok())
            .is_some_and(|a| a.map_state == MapState::VIEWABLE)
        {
            let _ =
                self.connection
                    .set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME);
            let _ = self.connection.flush();
        }
    }
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
    pub fn bindings(&self) -> &[BindingStatus] {
        self.shortcuts.statuses()
    }

    /// Block on server events, never on a periodic ownership poll.
    pub fn wait(&mut self) -> Result<Update, X11Error> {
        let event = self.connection.wait_for_event().map_err(request)?;
        self.handle(event)
    }

    /// Nonblocking drain for tests or an external event-loop readiness callback.
    pub fn poll(&mut self) -> Result<Option<Update>, X11Error> {
        self.connection
            .poll_for_event()
            .map_err(request)?
            .map(|event| self.handle(event))
            .transpose()
    }

    fn handle(&mut self, event: Event) -> Result<Update, X11Error> {
        self.serving.event(&self.connection, &event);
        match event {
            Event::XfixesSelectionNotify(event)
                if event.response_type & 0x80 == 0
                    && event.window == self.window
                    && event.selection == self.clipboard =>
            {
                self.serving
                    .acquisition(event.sequence, event.owner, event.selection_timestamp);
                Ok(
                    if self.refresh(Some((event.owner, event.selection_timestamp)))? {
                        Update::Ownership
                    } else {
                        Update::Other
                    },
                )
            }
            Event::KeyPress(event) if event.response_type & 0x80 == 0 => Ok(self
                .shortcuts
                .action(event.detail, event.state.into())
                .map(Update::Shortcut)
                .unwrap_or(Update::Other)),
            Event::MappingNotify(event)
                if event.response_type & 0x80 == 0 && event.request != Mapping::POINTER =>
            {
                // A failed rebind is reported through the binding statuses; a
                // broken connection surfaces on the next wait or poll.
                let _ = self.shortcuts.bind(&self.connection, self.root);
                Ok(Update::Bindings)
            }
            _ => Ok(Update::Other),
        }
    }

    fn refresh(&mut self, event: Option<(u32, u32)>) -> Result<bool, X11Error> {
        let owner = self
            .connection
            .get_selection_owner(self.clipboard)
            .map_err(request)?
            .reply()
            .map_err(request)?
            .owner;
        if event.is_some_and(|(event_owner, _)| event_owner != owner) {
            return Ok(false);
        }
        let (source, reason) = resolver::resolve(
            &self.connection,
            self.root,
            self.client_list,
            owner,
            self.has_res,
            self.has_res_pids,
        );
        let after = self
            .connection
            .get_selection_owner(self.clipboard)
            .map_err(request)?
            .reply()
            .map_err(request)?
            .owner;
        if after != owner {
            return Ok(false);
        }
        self.serving.owner(owner);
        self.snapshot = Snapshot {
            owner,
            generation: self.snapshot.generation.saturating_add(1),
            selection_timestamp: event.map(|(_, timestamp)| timestamp),
            source,
            unknown_reason: reason,
        };
        Ok(true)
    }
}
