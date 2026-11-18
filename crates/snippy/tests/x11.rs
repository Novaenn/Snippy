//! Run only through scripts/check-x11.sh on a private Xvfb display.
use snippy::x11::{Action, BindingState, Observer, UnknownReason, Update};
use snippy_core::{config::ShortcutConfig, source::SourceIdentity};
use std::{
    thread,
    time::{Duration, Instant},
};
use x11rb::{
    CURRENT_TIME,
    connection::Connection,
    protocol::{
        Event,
        xfixes::{SelectionEvent, SelectionNotifyEvent},
        xproto::{
            AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, InputFocus, KEY_PRESS_EVENT,
            KEY_RELEASE_EVENT, MappingStatus, PropMode, WindowClass,
        },
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

fn isolated() {
    assert_eq!(
        std::env::var("SNIPPY_TEST_X11").as_deref(),
        Ok("1"),
        "Use scripts/check-x11.sh; never run fixture tests on a live clipboard"
    );
}

struct Fixture {
    connection: RustConnection,
    root: u32,
    clipboard: u32,
}
impl Fixture {
    fn new() -> Self {
        let (connection, screen) = x11rb::connect(None).unwrap();
        let root = connection.setup().roots[screen].root;
        let clipboard = connection
            .intern_atom(false, b"CLIPBOARD")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        Self {
            connection,
            root,
            clipboard,
        }
    }
    fn window(&self, class: Option<&str>) -> u32 {
        let window = self.connection.generate_id().unwrap();
        self.connection
            .create_window(
                0,
                window,
                self.root,
                0,
                0,
                100,
                100,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
        if let Some(class) = class {
            self.connection
                .change_property8(
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
        window
    }
    fn copy(&self, window: u32) {
        self.connection
            .set_selection_owner(window, self.clipboard, CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        self.connection.flush().unwrap();
    }
    fn assert_no_content_requests(&self) {
        self.connection.get_input_focus().unwrap().reply().unwrap();
        while let Some(event) = self.connection.poll_for_event().unwrap() {
            assert!(
                !matches!(event, Event::SelectionRequest(_)),
                "Observer requested clipboard content or TARGETS"
            );
        }
    }
    fn code(&self, symbol: u32) -> u8 {
        let setup = self.connection.setup();
        let mapping = self
            .connection
            .get_keyboard_mapping(setup.min_keycode, setup.max_keycode - setup.min_keycode + 1)
            .unwrap()
            .reply()
            .unwrap();
        let index = mapping
            .keysyms
            .chunks(usize::from(mapping.keysyms_per_keycode))
            .position(|row| row.contains(&symbol))
            .expect("fixture keysym available");
        setup.min_keycode + u8::try_from(index).unwrap()
    }
    fn key(&self, code: u8, pressed: bool) {
        self.connection
            .xtest_fake_input(
                if pressed {
                    KEY_PRESS_EVENT
                } else {
                    KEY_RELEASE_EVENT
                },
                code,
                CURRENT_TIME,
                self.root,
                0,
                0,
                0,
            )
            .unwrap()
            .check()
            .unwrap();
    }
    fn activate(&self, symbol: u32) {
        let control = self.code(0xffe3);
        let alt = self.code(0xffe9);
        let key = self.code(symbol);
        self.key(control, true);
        self.key(alt, true);
        self.key(key, true);
        self.key(key, false);
        self.key(alt, false);
        self.key(control, false);
        self.connection.flush().unwrap();
    }
    fn toggle(&self, symbol: u32) {
        let code = self.code(symbol);
        self.key(code, true);
        self.key(code, false);
    }
}

fn disabled() -> ShortcutConfig {
    ShortcutConfig {
        enabled: false,
        ..ShortcutConfig::default()
    }
}
fn until(observer: &mut Observer, mut condition: impl FnMut(&Observer, Update) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match observer.poll().unwrap() {
            Some(update) if condition(observer, update) => return,
            Some(_) => (),
            None => thread::sleep(Duration::from_millis(2)),
        }
    }
    panic!("Timed out waiting for synthetic X11 event");
}
fn drain(observer: &mut Observer) {
    while observer.poll().unwrap().is_some() {}
}
fn observed(observer: &Observer, class: &str) -> bool {
    matches!(&observer.snapshot().source, SourceIdentity::Observed { display_name, .. } if display_name == class)
}

#[test]
#[ignore = "requires private Xvfb; use scripts/check-x11.sh"]
fn ownership_survives_focus_changes_and_never_requests_content() {
    isolated();
    let fixture = Fixture::new();
    let copied = fixture.window(Some("FixturePasswordManager"));
    fixture.copy(copied);
    let mut observer = Observer::connect(&disabled()).unwrap();
    assert!(observed(&observer, "FixturePasswordManager")); // Existing owner at startup.
    drain(&mut observer);
    let other = Fixture::new();
    let focused = other.window(Some("UnrelatedEditor"));
    other
        .connection
        .map_window(focused)
        .unwrap()
        .check()
        .unwrap();
    other
        .connection
        .set_input_focus(InputFocus::PARENT, focused, CURRENT_TIME)
        .unwrap()
        .check()
        .unwrap();
    assert!(observed(&observer, "FixturePasswordManager"));
    let before = observer.snapshot().generation;
    thread::sleep(Duration::from_millis(2));
    fixture.copy(copied);
    until(&mut observer, |observer, update| {
        update == Update::Ownership && observer.snapshot().generation > before
    });
    assert!(observed(&observer, "FixturePasswordManager"));
    let another = fixture.window(Some("AnotherApp"));
    fixture.copy(another);
    until(&mut observer, |observer, _| {
        observed(observer, "AnotherApp")
    });
    fixture.assert_no_content_requests();
    other.assert_no_content_requests();
    drop(fixture);
    until(&mut observer, |observer, _| observer.snapshot().owner == 0);
    assert_eq!(
        observer.snapshot().unknown_reason,
        Some(UnknownReason::Empty)
    );
}

#[test]
#[ignore = "requires private Xvfb; use scripts/check-x11.sh"]
fn hidden_helpers_unknown_ambiguous_and_proxy_sources_are_honest() {
    isolated();
    let fixture = Fixture::new();
    let app = fixture.window(Some("FixtureBrowser"));
    let helper = fixture.window(None);
    let mut observer = Observer::connect(&disabled()).unwrap();
    fixture.copy(helper);
    until(&mut observer, |observer, _| {
        observer.snapshot().owner == helper
    });
    assert!(observed(&observer, "FixtureBrowser"));
    let _different_app_same_client = fixture.window(Some("DifferentIdentity"));
    thread::sleep(Duration::from_millis(2));
    fixture.copy(helper);
    until(&mut observer, |observer, _| {
        observer.snapshot().unknown_reason == Some(UnknownReason::AmbiguousClient)
    });
    let proxy = fixture.window(Some("CopyQ"));
    fixture.copy(proxy);
    until(&mut observer, |observer, _| {
        observer.snapshot().unknown_reason == Some(UnknownReason::ClipboardProxy)
    });
    let unknown_client = Fixture::new();
    let unknown = unknown_client.window(None);
    unknown_client.copy(unknown);
    until(&mut observer, |observer, _| {
        observer.snapshot().owner == unknown
    });
    assert!(observer.snapshot().source == SourceIdentity::Unknown);
    fixture.assert_no_content_requests();
    unknown_client.assert_no_content_requests();
    fixture
        .connection
        .destroy_window(app)
        .unwrap()
        .check()
        .unwrap();
}

#[test]
#[ignore = "requires private Xvfb; use scripts/check-x11.sh"]
fn separate_clipboard_connections_resolve_via_server_pid_not_pid_properties() {
    isolated();
    let application = Fixture::new();
    let terminal = application.window(Some("Alacritty"));
    let clipboard = Fixture::new();
    let helper = clipboard.window(None);
    assert_ne!(
        application.connection.setup().resource_id_base,
        clipboard.connection.setup().resource_id_base
    );

    // These deliberately disagree. Only XRes's authoritative peer PID may
    // establish the relationship between the two X11 connections.
    let pid_atom = application
        .connection
        .intern_atom(false, b"_NET_WM_PID")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    application
        .connection
        .change_property32(
            PropMode::REPLACE,
            terminal,
            pid_atom,
            AtomEnum::CARDINAL,
            &[42],
        )
        .unwrap()
        .check()
        .unwrap();
    clipboard
        .connection
        .change_property32(
            PropMode::REPLACE,
            helper,
            pid_atom,
            AtomEnum::CARDINAL,
            &[84],
        )
        .unwrap()
        .check()
        .unwrap();
    clipboard.copy(helper);
    let mut observer = Observer::connect(&disabled()).unwrap();
    assert!(observed(&observer, "Alacritty"));
    drain(&mut observer);

    // If the same process exposes more than one application identity, the
    // helper alone cannot say which one supplied its data.
    application.window(Some("AnotherIdentity"));
    thread::sleep(Duration::from_millis(2));
    clipboard.copy(helper);
    until(&mut observer, |observer, _| {
        observer.snapshot().unknown_reason == Some(UnknownReason::AmbiguousClient)
    });
    assert!(observer.snapshot().source == SourceIdentity::Unknown);
    application.assert_no_content_requests();
    clipboard.assert_no_content_requests();
}

#[test]
#[ignore = "requires private Xvfb; use scripts/check-x11.sh"]
fn primary_and_forged_notifications_do_not_change_the_snapshot() {
    isolated();
    let fixture = Fixture::new();
    let window = fixture.window(Some("FixtureApp"));
    let mut observer = Observer::connect(&disabled()).unwrap();
    fixture.copy(window);
    until(&mut observer, |observer, _| {
        observer.snapshot().owner == window
    });
    drain(&mut observer);
    let generation = observer.snapshot().generation;
    fixture
        .connection
        .set_selection_owner(window, AtomEnum::PRIMARY.into(), CURRENT_TIME)
        .unwrap()
        .check()
        .unwrap();
    // A server round trip ensures the PRIMARY change has been processed.
    fixture
        .connection
        .get_input_focus()
        .unwrap()
        .reply()
        .unwrap();
    thread::sleep(Duration::from_millis(20));
    drain(&mut observer);
    assert_eq!(observer.snapshot().generation, generation);

    // Identify this fixture's sole InputOnly helper without exposing the
    // observer's private connection/window as a public application interface.
    let helper = fixture
        .connection
        .query_tree(fixture.root)
        .unwrap()
        .reply()
        .unwrap()
        .children
        .into_iter()
        .find(|window| {
            fixture
                .connection
                .get_window_attributes(*window)
                .unwrap()
                .reply()
                .is_ok_and(|attributes| attributes.class == WindowClass::INPUT_ONLY)
        })
        .expect("observer notification window exists");
    let extension = fixture
        .connection
        .query_extension(b"XFIXES")
        .unwrap()
        .reply()
        .unwrap();
    let forged = SelectionNotifyEvent {
        response_type: extension.first_event,
        subtype: SelectionEvent::SET_SELECTION_OWNER,
        window: helper,
        owner: window,
        selection: fixture.clipboard,
        ..SelectionNotifyEvent::default()
    };
    fixture
        .connection
        .send_event(false, helper, EventMask::NO_EVENT, forged)
        .unwrap()
        .check()
        .unwrap();
    thread::sleep(Duration::from_millis(20));
    drain(&mut observer);
    assert_eq!(observer.snapshot().generation, generation);
    fixture.assert_no_content_requests();
}

#[test]
#[ignore = "requires private Xvfb; use scripts/check-x11.sh"]
fn shortcuts_handle_locks_conflicts_disabled_bindings_and_remapping() {
    isolated();
    let fixture = Fixture::new();
    let mut first = Observer::connect(&ShortcutConfig::default()).unwrap();
    assert!(
        first
            .bindings()
            .iter()
            .all(|b| b.state == BindingState::Bound)
    );
    drain(&mut first);
    for toggle in [None, Some(0xffe5), Some(0xff7f), Some(0xffe5), Some(0xff7f)] {
        if let Some(symbol) = toggle {
            fixture.toggle(symbol);
        }
        fixture.activate(u32::from(b'g'));
        until(&mut first, |_, update| {
            update == Update::Shortcut(Action::Save)
        });
    }
    fixture.activate(u32::from(b'v'));
    until(&mut first, |_, update| {
        update == Update::Shortcut(Action::Picker)
    });

    let second = Observer::connect(&ShortcutConfig {
        picker: "Ctrl+Alt+Y".into(),
        ..ShortcutConfig::default()
    })
    .unwrap();
    assert_eq!(second.bindings()[0].state, BindingState::Unavailable);
    assert_eq!(second.bindings()[1].state, BindingState::Bound);
    let mut no_keys = Observer::connect(&disabled()).unwrap();
    assert!(
        no_keys
            .bindings()
            .iter()
            .all(|b| b.state == BindingState::Disabled)
    );
    drain(&mut no_keys);
    fixture.activate(u32::from(b'g'));
    until(&mut first, |_, update| {
        update == Update::Shortcut(Action::Save)
    });
    assert!(no_keys.poll().unwrap().is_none());
    drop(second);

    // Remapping the same row still emits MappingNotify; bindings must recover.
    let code = fixture.code(u32::from(b'g'));
    let row = fixture
        .connection
        .get_keyboard_mapping(code, 1)
        .unwrap()
        .reply()
        .unwrap();
    fixture
        .connection
        .change_keyboard_mapping(1, code, row.keysyms_per_keycode, &row.keysyms)
        .unwrap()
        .check()
        .unwrap();
    until(&mut first, |_, update| update == Update::Bindings);
    assert!(
        first
            .bindings()
            .iter()
            .all(|b| b.state == BindingState::Bound)
    );
    fixture.activate(u32::from(b'g'));
    until(&mut first, |_, update| {
        update == Update::Shortcut(Action::Save)
    });

    // Pointer remapping also emits MappingNotify but must not regrab keys.
    drain(&mut first);
    let buttons = fixture
        .connection
        .get_pointer_mapping()
        .unwrap()
        .reply()
        .unwrap()
        .map;
    let status = fixture
        .connection
        .set_pointer_mapping(&buttons)
        .unwrap()
        .reply()
        .unwrap()
        .status;
    assert_eq!(status, MappingStatus::SUCCESS);
    fixture
        .connection
        .change_keyboard_mapping(1, code, row.keysyms_per_keycode, &row.keysyms)
        .unwrap()
        .check()
        .unwrap();
    let mut rebinds = 0;
    until(&mut first, |_, update| {
        rebinds += usize::from(update == Update::Bindings);
        update == Update::Bindings
    });
    first.synchronize().unwrap();
    while let Some(update) = first.poll().unwrap() {
        rebinds += usize::from(update == Update::Bindings);
    }
    assert_eq!(rebinds, 1, "only the keyboard remap rebinds shortcuts");
    fixture.assert_no_content_requests();
}

#[test]
#[ignore = "requires Xvfb with XFixes disabled; use scripts/check-x11.sh"]
fn missing_xfixes_fails_instead_of_polling() {
    isolated();
    assert!(matches!(
        Observer::connect(&disabled()),
        Err(snippy::x11::X11Error::MissingXfixes)
    ));
}

#[test]
#[ignore = "requires private Xvfb; use scripts/check-x11.sh"]
fn owner_destruction_and_metadata_budgets_do_not_invent_a_source() {
    isolated();
    let fixture = Fixture::new();
    let owner = fixture.window(Some("FixtureApp"));
    let mut observer = Observer::connect(&disabled()).unwrap();
    fixture.copy(owner);
    until(&mut observer, |observer, _| {
        observed(observer, "FixtureApp")
    });
    fixture
        .connection
        .destroy_window(owner)
        .unwrap()
        .check()
        .unwrap();
    until(&mut observer, |observer, _| observer.snapshot().owner == 0);
    for _ in 0..130 {
        fixture.window(Some("FixtureApp"));
    }
    let hidden = fixture.window(None);
    fixture.copy(hidden);
    until(&mut observer, |observer, _| {
        observer.snapshot().owner == hidden
    });
    assert_eq!(
        observer.snapshot().unknown_reason,
        Some(UnknownReason::ResolutionBudget)
    );
    fixture.assert_no_content_requests();
}

#[test]
#[ignore = "requires Xvfb with X-Resource disabled; use scripts/check-x11.sh"]
fn missing_optional_xres_keeps_direct_sources_and_marks_helpers_unknown() {
    isolated();
    let fixture = Fixture::new();
    assert!(
        !fixture
            .connection
            .query_extension(b"X-Resource")
            .unwrap()
            .reply()
            .unwrap()
            .present
    );
    let direct = fixture.window(Some("FixtureApp"));
    fixture.copy(direct);
    let mut observer = Observer::connect(&disabled()).unwrap();
    assert!(observed(&observer, "FixtureApp"));
    drain(&mut observer);
    let helper = fixture.window(None);
    fixture.copy(helper);
    until(&mut observer, |observer, _| {
        observer.snapshot().owner == helper
    });
    assert_eq!(
        observer.snapshot().unknown_reason,
        Some(UnknownReason::MissingMetadata)
    );
    fixture.assert_no_content_requests();
}
