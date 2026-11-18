//! On-demand vault lifecycle UI. All IPC and key derivation run off the GTK thread.
mod picker;
use futures_channel::mpsc;
use futures_util::StreamExt;
use gtk::{gio, glib, prelude::*};
use snippy::ipc::{self, Error, Paths, Reply, Request, State, VaultState};
use std::{
    cell::RefCell,
    net::Shutdown,
    os::unix::net::UnixStream,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};
use zeroize::Zeroizing;

#[derive(Default)]
struct Slots {
    state: Option<State>,
    error: Option<Error>,
    present: bool,
    disconnected: bool,
    online: bool,
    done: bool,
    prompt: Option<ipc::Prompt>,
}
#[derive(Clone)]
struct Mailbox {
    slots: Arc<Mutex<Slots>>,
    signal: mpsc::Sender<()>,
}
impl Mailbox {
    fn post(&mut self, change: impl FnOnce(&mut Slots)) {
        if let Ok(mut slots) = self.slots.lock() {
            change(&mut slots);
        }
        let _ = self.signal.try_send(());
    }
    fn state(&mut self, state: State) {
        self.post(|slots| {
            if slots.state.is_none_or(|old| state.revision >= old.revision) {
                slots.state = Some(state)
            }
        })
    }
}
#[derive(Default)]
struct Connections {
    watch: Mutex<Option<UnixStream>>,
    operation: Mutex<Option<UnixStream>>,
    closed: AtomicBool,
    ticket: AtomicU64,
}
impl Connections {
    fn cancel(&self) {
        self.ticket.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut stream) = self.operation.lock()
            && let Some(stream) = stream.take()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.cancel();
        if let Ok(mut stream) = self.watch.lock()
            && let Some(stream) = stream.take()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}
struct Model {
    state: Option<State>,
    connected: bool,
    busy: bool,
    prompt: Option<ipc::Prompt>,
}
struct View {
    window: glib::WeakRef<gtk::ApplicationWindow>,
    heading: gtk::Label,
    description: gtk::Label,
    error: gtk::Label,
    pass: gtk::PasswordEntry,
    confirm: gtk::PasswordEntry,
    submit: gtk::Button,
    cancel: gtk::Button,
    lock: gtk::Button,
    bindings: gtk::Label,
    pending: gtk::Label,
    label: gtk::Entry,
    approve: gtk::Button,
    reject: gtk::Button,
    picker: Rc<picker::Picker>,
    model: RefCell<Model>,
    connections: Arc<Connections>,
    mailbox: Mailbox,
}
impl View {
    fn clear(&self) {
        self.pass.set_text("");
        self.confirm.set_text("");
        self.label.set_text("");
    }
    fn render(&self) {
        let model = self.model.borrow();
        let phase = model.state.map(|s| s.phase).unwrap_or(VaultState::Locked);
        let exists = model.state.is_some_and(|s| s.exists);
        if let Some(window) = self.window.upgrade() {
            window.set_title(Some(if !model.connected {
                "Snippy — Vault — Service unavailable"
            } else {
                match phase {
                    VaultState::Locked => "Snippy — Vault — Locked",
                    VaultState::Unlocking => "Snippy — Vault — Unlocking",
                    VaultState::Unlocked
                        if model.prompt.as_ref().is_some_and(|p| {
                            model.state.is_some_and(|s| {
                                s.capture == p.capture
                                    && s.session == p.session
                                    && s.capture_stage == 3
                            })
                        }) =>
                    {
                        "Snippy — Vault — Confirm Save"
                    }
                    VaultState::Unlocked => "Snippy — Vault — Unlocked",
                }
            }));
        }
        self.heading.set_text(if !model.connected {
            "Service unavailable. Start snippy daemon, then reopen this window."
        } else {
            match phase {
                VaultState::Locked if !exists => "Create your encrypted vault.",
                VaultState::Locked => "Vault locked.",
                VaultState::Unlocking => "Unlocking… You can cancel this operation.",
                VaultState::Unlocked => "Vault unlocked.",
            }
        });
        let input = model.connected && phase == VaultState::Locked;
        if !input
            && (self.pass.has_focus() || self.confirm.has_focus())
            && let Some(window) = self.window.upgrade()
        {
            // Clear text-widget focus before hiding the credential fields so GTK
            // receives a normal focus transition.
            gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
        }
        self.pass.set_visible(input);
        self.confirm.set_visible(input && !exists);
        self.submit.set_visible(input);
        self.submit
            .set_label(if exists { "Unlock" } else { "Create vault" });
        self.submit.set_sensitive(input && !model.busy);
        self.pass.set_sensitive(!model.busy);
        self.confirm.set_sensitive(!model.busy);
        if input && !model.busy {
            self.pass.grab_focus();
        }
        self.cancel
            .set_visible(model.connected && phase != VaultState::Unlocked);
        self.lock
            .set_visible(model.connected && phase == VaultState::Unlocked);
        self.lock.set_sensitive(!model.busy);
        let unlocked = model.connected && phase == VaultState::Unlocked;
        let active = model.state.is_some_and(|s| s.capture != 0);
        let prompt = model.prompt.as_ref().filter(|p| {
            unlocked
                && model.state.is_some_and(|s| {
                    s.capture == p.capture && s.session == p.session && s.capture_stage == 3
                })
        });
        self.approve.set_visible(prompt.is_some());
        self.approve.set_sensitive(!model.busy);
        self.reject.set_visible(model.connected && active);
        self.reject.set_sensitive(!model.busy);
        self.label.set_visible(prompt.is_some_and(|p| !p.sensitive));
        // Unlocked views show the history directly; the explanation is for setup/unlock.
        self.description.set_visible(!unlocked);
        let pending_text = if let Some(p) = prompt {
            format!(
                "{}\n{}",
                if p.sensitive {
                    "Sensitive source: confirm this Save before reading clipboard content."
                } else {
                    "Unknown source: save as Unknown, or enter an optional manual label for this Save."
                },
                source_description(&p.source)
            )
        } else if let Some(s) = model.state {
            match s.capture_stage {
                1 => "Save pending: unlock before any clipboard data is read.".into(),
                2 => "Checking source policy…".into(),
                4 => "Receiving and validating clipboard…".into(),
                5 => "Saving to the encrypted vault…".into(),
                _ => match s.save_outcome {
                    1 => "Saved successfully.".into(),
                    2 => "Saved: duplicate merged.".into(),
                    3 => "Already saved; history unchanged.".into(),
                    0 => String::new(),
                    code => Error::from_code(code)
                        .map(|e| e.to_string())
                        .unwrap_or_default(),
                },
            }
        } else {
            String::new()
        };
        self.pending.set_visible(!pending_text.is_empty());
        self.pending.set_text(&pending_text);
        self.error.set_visible(!self.error.text().is_empty());
        self.bindings.set_visible(false);
        if let Some(state) = model.state {
            let describe = |code| match code {
                0 => "Disabled",
                1 => "Bound",
                _ => "Unavailable/conflicting",
            };
            let text = format!(
                "Save shortcut: {} · Picker shortcut: {}",
                describe(state.save_binding),
                describe(state.picker_binding)
            );
            // Always listed in settings; only a failed binding is shown here.
            self.bindings
                .set_visible(state.save_binding >= 2 || state.picker_binding >= 2);
            self.bindings.set_text(&text);
            self.picker.set_bindings(&text);
        }
        self.picker.state(model.state, model.connected);
    }
}
fn main() -> glib::ExitCode {
    if gtk::init().is_err() {
        eprintln!("snippy-gui: could not initialize GTK; run from a graphical X11 session");
        return glib::ExitCode::FAILURE;
    }
    // Installed by install.sh into the user's hicolor icon theme.
    gtk::Window::set_default_icon_name("snippy");
    let application = gtk::Application::builder()
        .application_id("org.snippy.Snippy")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.connect_activate(present);
    application.run()
}
fn present(application: &gtk::Application) {
    if let Some(window) = application.active_window() {
        window.present();
        return;
    }
    let heading = gtk::Label::new(Some("Connecting to snippy…"));
    heading.add_css_class("title-3");
    heading.set_wrap(true);
    heading.set_xalign(0.0);
    heading.set_hexpand(true);
    let description = gtk::Label::new(Some(
        "Saved clips stay in an encrypted vault. Unlock it to save the current clipboard, browse history, or copy a saved item back.",
    ));
    description.set_wrap(true);
    description.set_xalign(0.0);
    let error = gtk::Label::new(None);
    error.set_wrap(true);
    error.add_css_class("error");
    let pass = gtk::PasswordEntry::builder()
        .placeholder_text("Passphrase")
        .show_peek_icon(false)
        .build();
    let confirm = gtk::PasswordEntry::builder()
        .placeholder_text("Confirm passphrase")
        .show_peek_icon(false)
        .build();
    let submit = gtk::Button::with_label("Unlock");
    let cancel = gtk::Button::with_label("Cancel");
    let lock = gtk::Button::with_label("Lock");
    lock.set_tooltip_text(Some("Lock the vault now and clear this window"));
    let bindings = gtk::Label::new(None);
    bindings.set_wrap(true);
    bindings.set_xalign(0.0);
    bindings.add_css_class("error");
    let pending = gtk::Label::new(None);
    pending.set_wrap(true);
    let label = gtk::Entry::builder()
        .placeholder_text("Optional manual source label")
        .max_length(256)
        .build();
    let approve = gtk::Button::with_mnemonic("Confirm _Save");
    let reject = gtk::Button::with_mnemonic("Cancel Sa_ve");
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.append(&heading);
    header.append(&lock);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    for widget in [
        &header.upcast::<gtk::Widget>(),
        &description.clone().upcast(),
        &pass.clone().upcast(),
        &confirm.clone().upcast(),
        &error.clone().upcast(),
        &submit.clone().upcast(),
        &cancel.clone().upcast(),
        &pending.clone().upcast(),
        &label.clone().upcast(),
        &approve.clone().upcast(),
        &reject.clone().upcast(),
        &bindings.clone().upcast(),
    ] {
        content.append(widget)
    }
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(20);
    content.set_margin_end(20);
    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title("Snippy — Vault")
        .default_width(980)
        .default_height(720)
        .child(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(&content)
                .build(),
        )
        .build();
    let picker = picker::Picker::new(&window);
    content.append(&picker.root);
    let (sender, mut receiver) = mpsc::channel(1);
    let mailbox = Mailbox {
        slots: Arc::new(Mutex::new(Slots::default())),
        signal: sender,
    };
    let view = Rc::new(View {
        window: window.downgrade(),
        heading,
        description,
        error,
        pass,
        confirm,
        submit,
        cancel,
        lock,
        bindings,
        pending,
        label,
        approve,
        reject,
        picker,
        model: RefCell::new(Model {
            state: None,
            connected: false,
            busy: false,
            prompt: None,
        }),
        connections: Arc::new(Connections::default()),
        mailbox,
    });
    let weak = Rc::downgrade(&view);
    view.submit.connect_clicked(move |_| {
        if let Some(view) = weak.upgrade() {
            let passphrase = Zeroizing::new(view.pass.text().to_string());
            let confirmation = Zeroizing::new(view.confirm.text().to_string());
            let exists = view.model.borrow().state.is_some_and(|s| s.exists);
            view.clear();
            if let Some(window) = view.window.upgrade() {
                gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
            }
            operate(
                &view,
                if exists {
                    Request::Unlock(passphrase)
                } else {
                    Request::Create {
                        passphrase,
                        confirmation,
                    }
                },
            );
        }
    });
    let weak = Rc::downgrade(&view);
    view.pass.connect_activate(move |_| {
        if let Some(view) = weak.upgrade() {
            view.submit.emit_clicked();
        }
    });
    let weak = Rc::downgrade(&view);
    view.confirm.connect_activate(move |_| {
        if let Some(view) = weak.upgrade() {
            view.submit.emit_clicked();
        }
    });
    let weak = Rc::downgrade(&view);
    view.cancel.connect_clicked(move |_| {
        if let Some(view) = weak.upgrade() {
            view.clear();
            view.connections.cancel();
            view.model.borrow_mut().busy = false;
            operate(&view, Request::Lock);
        }
    });
    let weak = Rc::downgrade(&view);
    view.lock.connect_clicked(move |_| {
        if let Some(view) = weak.upgrade() {
            view.clear();
            operate(&view, Request::Lock);
        }
    });
    let weak = Rc::downgrade(&view);
    view.approve.connect_clicked(move |_| {
        if let Some(view) = weak.upgrade() {
            let state = view.model.borrow().state;
            if let Some(s) = state {
                let label = Zeroizing::new(view.label.text().to_string());
                view.label.set_text("");
                operate(
                    &view,
                    Request::Confirm {
                        session: s.session,
                        capture: s.capture,
                        label,
                    },
                );
            }
        }
    });
    let weak = Rc::downgrade(&view);
    view.reject.connect_clicked(move |_| {
        if let Some(view) = weak.upgrade() {
            let state = view.model.borrow().state;
            if let Some(s) = state {
                operate(&view, Request::CancelSave { capture: s.capture });
            }
        }
    });
    let connections = view.connections.clone();
    let picker = view.picker.clone();
    window.connect_close_request(move |_| {
        picker.close();
        connections.close();
        glib::Propagation::Proceed
    });
    let weak = window.downgrade();
    let escape = gtk::EventControllerKey::new();
    escape.set_propagation_phase(gtk::PropagationPhase::Capture);
    escape.connect_key_pressed(move |_, key, _, modifiers| {
        if key == gtk::gdk::Key::Escape
            || (key == gtk::gdk::Key::c && modifiers.contains(gtk::gdk::ModifierType::ALT_MASK))
        {
            if let Some(window) = weak.upgrade() {
                window.close();
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(escape);
    view.render();
    window.present();
    let connections = view.connections.clone();
    let mut mailbox = view.mailbox.clone();
    thread::spawn(move || {
        let result = (|| {
            let paths = Paths::discover()?;
            let (mut stream, state) = ipc::subscribe(&paths)?;
            if connections.closed.load(Ordering::SeqCst) {
                return Ok(());
            }
            *connections.watch.lock().map_err(|_| Error::Io)? = Some(stream.try_clone()?);
            mailbox.post(|slots| {
                slots.online = true;
                slots.state = Some(state)
            });
            loop {
                if connections.closed.load(Ordering::SeqCst) {
                    break;
                }
                match ipc::read_reply(&mut stream)? {
                    (0, Reply::State(state)) => mailbox.state(state),
                    (0, Reply::Prompt(prompt)) => mailbox.post(|s| s.prompt = Some(prompt)),
                    (0, Reply::Present) => mailbox.post(|s| s.present = true),
                    _ => return Err(Error::Protocol),
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            mailbox.post(|s| {
                s.error = Some(error);
                s.disconnected = true;
            });
        }
    });
    glib::MainContext::default().spawn_local(async move {
        while receiver.next().await.is_some() {
            let Some(window) = view.window.upgrade() else {
                break;
            };
            let slots = std::mem::take(&mut *view.mailbox.slots.lock().expect("UI mailbox"));
            if slots.present {
                window.present();
            }
            if slots.error == Some(Error::AlreadyOpen) {
                window.close();
                break;
            }
            if slots.online {
                view.model.borrow_mut().connected = true;
            }
            if let Some(state) = slots.state {
                let mut model = view.model.borrow_mut();
                if model.connected && model.state.is_none_or(|old| state.revision >= old.revision) {
                    let reset = model.state.is_some_and(|old| {
                        old.session != state.session || old.phase != state.phase
                    });
                    model.state = Some(state);
                    if reset || state.capture_stage != 3 {
                        model.prompt = None;
                        view.label.set_text("");
                    }
                    if reset || state.phase != VaultState::Locked {
                        view.clear();
                    }
                }
            }
            if slots.disconnected {
                view.connections.cancel();
                view.clear();
                let mut model = view.model.borrow_mut();
                model.connected = false;
                model.state = None;
                model.busy = false;
                model.prompt = None;
            }
            if let Some(prompt) = slots.prompt {
                let mut model = view.model.borrow_mut();
                if model.connected
                    && model.state.is_some_and(|s| {
                        s.session == prompt.session
                            && s.capture == prompt.capture
                            && s.capture_stage == 3
                            && s.phase == VaultState::Unlocked
                    })
                {
                    let unknown = !prompt.sensitive;
                    model.prompt = Some(prompt);
                    if unknown {
                        view.label.set_visible(true);
                        view.label.grab_focus();
                    }
                }
            }
            if slots.done {
                view.model.borrow_mut().busy = false;
            }
            if let Some(error) = slots.error {
                view.error.set_text(&error.to_string());
            }
            view.render();
            if slots.present {
                view.picker.focus();
            }
        }
    });
}
fn operate(view: &Rc<View>, request: Request) {
    {
        let mut model = view.model.borrow_mut();
        if model.busy || !model.connected {
            return;
        }
        model.busy = true;
    }
    view.error.set_text("");
    view.render();
    let connections = view.connections.clone();
    let ticket = connections.ticket.fetch_add(1, Ordering::SeqCst) + 1;
    let mut mailbox = view.mailbox.clone();
    thread::spawn(move || {
        let result = (|| {
            let paths = Paths::discover()?;
            let mut stream = paths.connect()?;
            {
                let mut handle = connections.operation.lock().map_err(|_| Error::Io)?;
                if connections.closed.load(Ordering::SeqCst)
                    || connections.ticket.load(Ordering::SeqCst) != ticket
                {
                    return Err(Error::Canceled);
                }
                *handle = Some(stream.try_clone()?);
            }
            ipc::exchange_reply(&mut stream, request)
        })();
        if connections.ticket.load(Ordering::SeqCst) == ticket {
            if let Ok(mut handle) = connections.operation.lock() {
                handle.take();
            }
            match result {
                Ok(Reply::State(state)) => mailbox.state(state),
                Ok(_) => mailbox.post(|s| s.error = Some(Error::Protocol)),
                Err(error) => mailbox.post(|s| s.error = Some(error)),
            }
            mailbox.post(|s| s.done = true);
        }
    });
}

fn source_description(source: &snippy_core::source::SourceIdentity) -> String {
    match source {
        snippy_core::source::SourceIdentity::Observed {
            app_id,
            display_name,
        } => format!("Observed source: {display_name} ({app_id})"),
        snippy_core::source::SourceIdentity::Manual { label } => {
            format!("Manual source label: {label} (unverified)")
        }
        snippy_core::source::SourceIdentity::Unknown => "Source: Unknown".into(),
    }
}
