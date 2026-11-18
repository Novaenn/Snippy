//! History window (search, browse, tags, deletion, settings) with one coalescing IPC worker.
//! `snippy picker` and `snippy manager` open the same view; the service only uses the
//! picker request for pointer-monitor placement and focus restoration after copying.
//! This module never reads live clipboard content; reuse goes through the service.
use futures_channel::mpsc;
use futures_util::StreamExt;
use gtk::{glib, prelude::*};
use snippy::{
    ipc::{self, ConfigView, Error, ManagerInfo, Paths, Reply, Request, State, VaultState},
    storage::{BrowseQuery, ContentKind, PAGE_SIZE, Page},
};
use std::{
    cell::{Cell, RefCell},
    net::Shutdown,
    os::unix::net::UnixStream,
    rc::{Rc, Weak},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self as sync, SyncSender},
    },
    thread,
    time::Duration,
};
#[derive(Clone, Copy)]
enum Action {
    Page,
    Preview,
    Copy { close: bool },
    CopyDeleteCopy(i64),
    CopyDeleteDelete,
    Info,
    Tags,
    Deleted,
    Config,
    ApplyConfig,
    Sensitive,
}
struct Job {
    ticket: u64,
    request: Request,
    action: Action,
}
struct Done {
    ticket: u64,
    action: Action,
    result: Result<Reply, Error>,
}
struct Runner {
    latest: Mutex<Option<Job>>,
    active: Mutex<Option<UnixStream>>,
    wake: SyncSender<()>,
    ticket: AtomicU64,
    closed: AtomicBool,
}
impl Runner {
    fn cancel(&self) {
        self.ticket.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut job) = self.latest.lock() {
            job.take();
        }
        if let Ok(mut stream) = self.active.lock()
            && let Some(stream) = stream.take()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
    fn submit(&self, request: Request, action: Action) {
        self.cancel();
        let ticket = self.ticket.load(Ordering::SeqCst);
        *self.latest.lock().expect("picker queue") = Some(Job {
            ticket,
            request,
            action,
        });
        let _ = self.wake.try_send(());
    }
    fn idle(&self) -> bool {
        self.latest.lock().is_ok_and(|job| job.is_none())
            && self.active.lock().is_ok_and(|stream| stream.is_none())
    }
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.cancel();
        let _ = self.wake.try_send(());
    }
}
// Editing invalidates rows immediately; defer only the next storage request.
#[derive(Default)]
struct QueryDelay(RefCell<Option<glib::SourceId>>);
impl QueryDelay {
    fn cancel(&self) {
        if let Some(source) = self.0.borrow_mut().take() {
            source.remove();
        }
    }
    fn schedule(self: &Rc<Self>, callback: impl FnOnce() + 'static) {
        self.cancel();
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(Duration::from_millis(150), move || {
            if let Some(delay) = weak.upgrade() {
                delay.0.borrow_mut().take();
                callback();
            }
        });
        *self.0.borrow_mut() = Some(source);
    }
}
impl Drop for QueryDelay {
    fn drop(&mut self) {
        self.cancel();
    }
}
struct Model {
    state: Option<State>,
    active: bool,
    page: Option<Page>,
    selected: Option<i64>,
    busy: bool,
}
pub(super) struct Picker {
    pub root: gtk::Box,
    stack: gtk::Stack,
    search: gtk::SearchEntry,
    source: gtk::Entry,
    tag: gtk::Entry,
    kind: gtk::ComboBoxText,
    categories: gtk::FlowBox,
    list: gtk::ListBox,
    status: gtk::Label,
    usage: gtk::Label,
    picture: gtk::Picture,
    details: gtk::Label,
    tags_box: gtk::FlowBox,
    tag_entry: gtk::Entry,
    add_tag: gtk::Button,
    copy: gtk::Button,
    copy_delete: gtk::Button,
    copy_keep: gtk::Button,
    previous: gtk::Button,
    next: gtk::Button,
    delete: gtk::Button,
    delete_selected: gtk::Button,
    select_page: gtk::CheckButton,
    settings: gtk::Button,
    refresh: gtk::Button,
    save_entry: gtk::Entry,
    picker_entry: gtk::Entry,
    enabled: gtk::Switch,
    autostart: gtk::Switch,
    notifications: gtk::Switch,
    clip_entry: gtk::Entry,
    vault_entry: gtk::Entry,
    timeout_entry: gtk::Entry,
    settings_status: gtk::Label,
    shortcut_status: gtk::Label,
    sensitive_list: gtk::ListBox,
    sensitive_entry: gtk::Entry,
    sensitive_add: gtk::Button,
    settings_back: gtk::Button,
    window: glib::WeakRef<gtk::ApplicationWindow>,
    me: RefCell<Weak<Picker>>,
    model: RefCell<Model>,
    updating: Cell<bool>,
    source_id: Cell<Option<i64>>,
    pending_select: Cell<Option<i64>>,
    pending_offset: Cell<u64>,
    checks: RefCell<Vec<(i64, gtk::CheckButton)>>,
    category_buttons: RefCell<Vec<(Option<i64>, gtk::Button)>>,
    stale: Cell<bool>,
    config_loaded: Cell<bool>,
    runner: Arc<Runner>,
    query_delay: Rc<QueryDelay>,
}
impl Picker {
    pub fn new(window: &gtk::ApplicationWindow) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let stack = gtk::Stack::new();
        // Each page sizes itself; the tall settings page must not stretch history.
        stack.set_vhomogeneous(false);
        stack.set_vexpand(true);
        let history_page = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let settings_page = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search saved text, sources, or tags")
            .build();
        search.set_widget_name("snippy-search");
        let source = gtk::Entry::builder()
            .placeholder_text("Source filter: app identity or label")
            .max_length(512)
            .build();
        source.set_widget_name("snippy-source");
        let tag = gtk::Entry::builder()
            .placeholder_text("Tag filter: exact name")
            .max_length(128)
            .build();
        tag.set_widget_name("snippy-tag");
        let kind = gtk::ComboBoxText::new();
        kind.append(Some("all"), "All types");
        kind.append(Some("text"), "Text");
        kind.append(Some("png"), "Images");
        kind.set_active_id(Some("all"));
        kind.set_widget_name("snippy-kind");
        let search_label = gtk::Label::with_mnemonic("_Search");
        search_label.set_mnemonic_widget(Some(&search));
        search_label.set_xalign(0.0);
        let source_label = gtk::Label::with_mnemonic("_App/source");
        source_label.set_mnemonic_widget(Some(&source));
        source_label.set_xalign(0.0);
        let tag_label = gtk::Label::with_mnemonic("_Tag");
        tag_label.set_mnemonic_widget(Some(&tag));
        tag_label.set_xalign(0.0);
        let type_label = gtk::Label::with_mnemonic("T_ype");
        type_label.set_mnemonic_widget(Some(&kind));
        let search_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        search_row.append(&search_label);
        search_row.append(&search);
        search_row.append(&type_label);
        search_row.append(&kind);
        search.set_hexpand(true);
        let filters = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        filters.append(&source_label);
        filters.append(&source);
        filters.append(&tag_label);
        filters.append(&tag);
        source.set_hexpand(true);
        tag.set_hexpand(true);
        let categories = gtk::FlowBox::new();
        categories.set_selection_mode(gtk::SelectionMode::None);
        categories.set_max_children_per_line(6);
        categories.set_column_spacing(4);
        categories.set_row_spacing(4);
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::Single);
        list.set_activate_on_single_click(false);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(220)
            .min_content_width(280)
            .vexpand(true)
            .child(&list)
            .build();
        let status = gtk::Label::new(None);
        status.set_wrap(true);
        status.set_hexpand(true);
        let usage = gtk::Label::new(None);
        usage.set_wrap(true);
        usage.set_xalign(0.0);
        usage.add_css_class("dim-label");
        let manager_bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let refresh = gtk::Button::with_mnemonic("_Refresh");
        refresh.set_tooltip_text(Some("Reload categories, usage, and the current page"));
        let settings = gtk::Button::with_mnemonic("Sett_ings");
        settings.set_tooltip_text(Some(
            "Shortcuts, limits, startup, and sensitive apps (Ctrl+,)",
        ));
        manager_bar.append(&usage);
        manager_bar.append(&refresh);
        manager_bar.append(&settings);
        usage.set_hexpand(true);
        let picture = gtk::Picture::new();
        picture.set_can_shrink(true);
        picture.set_height_request(160);
        picture.set_visible(false);
        let details = gtk::Label::new(None);
        details.set_wrap(true);
        details.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        details.set_xalign(0.0);
        details.set_yalign(0.0);
        details.set_selectable(true);
        let details_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_width(240)
            .min_content_height(90)
            .vexpand(true)
            .child(&details)
            .build();
        let tags_box = gtk::FlowBox::new();
        tags_box.set_selection_mode(gtk::SelectionMode::None);
        tags_box.set_max_children_per_line(8);
        tags_box.set_column_spacing(4);
        let tag_entry = gtk::Entry::builder()
            .placeholder_text("New tag (128 bytes max)")
            .max_length(128)
            .build();
        tag_entry.set_widget_name("snippy-tag-entry");
        let add_tag = gtk::Button::with_mnemonic("Add ta_g");
        let tag_editor = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        tag_editor.append(&tag_entry);
        tag_editor.append(&add_tag);
        tag_entry.set_hexpand(true);
        let copy = gtk::Button::with_mnemonic("C_opy and close");
        copy.set_tooltip_text(Some(
            "Copy the selected item to the clipboard and close this window (Enter)",
        ));
        let copy_keep = gtk::Button::with_label("Copy");
        copy_keep.set_tooltip_text(Some(
            "Copy the selected item to the clipboard and keep this window open (Ctrl+Enter)",
        ));
        copy.add_css_class("suggested-action");
        let copy_delete = gtk::Button::with_label("Copy and delete");
        copy_delete.set_tooltip_text(Some(
            "Copy the selected item to the clipboard, then delete it from history (Ctrl+X). \
             If copying fails, the item remains.",
        ));
        let previous = gtk::Button::with_mnemonic("_Previous");
        let next = gtk::Button::with_mnemonic("_Next");
        let delete = gtk::Button::with_mnemonic("_Delete");
        delete.set_tooltip_text(Some("Delete the selected item after confirmation (Ctrl+D)"));
        delete.add_css_class("destructive-action");
        let delete_selected = gtk::Button::with_mnemonic("Delete c_hecked");
        delete_selected.set_tooltip_text(Some(
            "Delete the checked items on this page after confirmation (Ctrl+Shift+D)",
        ));
        let select_page = gtk::CheckButton::with_mnemonic("Chec_k page");
        select_page.set_tooltip_text(Some("Check or uncheck every item on this page (Ctrl+K)"));
        select_page.set_widget_name("snippy-select-page");
        let item_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        item_actions.append(&copy);
        item_actions.append(&copy_keep);
        item_actions.append(&copy_delete);
        item_actions.append(&delete);
        let detail = gtk::Box::new(gtk::Orientation::Vertical, 8);
        detail.set_margin_start(10);
        for widget in [
            &picture.clone().upcast::<gtk::Widget>(),
            &details_scroll.upcast(),
            &tags_box.clone().upcast(),
            &tag_editor.clone().upcast(),
            &item_actions.upcast(),
        ] {
            detail.append(widget);
        }
        let body = gtk::Paned::new(gtk::Orientation::Horizontal);
        body.set_start_child(Some(&scroll));
        body.set_end_child(Some(&detail));
        body.set_shrink_start_child(false);
        body.set_shrink_end_child(false);
        body.set_position(470);
        body.set_vexpand(true);
        let navigation = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        navigation.append(&select_page);
        navigation.append(&delete_selected);
        navigation.append(&status);
        navigation.append(&previous);
        navigation.append(&next);
        for widget in [
            &manager_bar.clone().upcast::<gtk::Widget>(),
            &search_row.upcast(),
            &filters.upcast(),
            &categories.clone().upcast(),
            &body.upcast(),
            &navigation.upcast(),
        ] {
            history_page.append(widget);
        }
        let settings_title = gtk::Label::new(Some("Settings"));
        settings_title.add_css_class("title-3");
        settings_title.set_xalign(0.0);
        let settings_description = gtk::Label::new(Some(
            "Changes take effect when you press Apply. Nonsecret preferences are stored \
             in your private config directory; sensitive-app identities stay inside the \
             encrypted vault.",
        ));
        settings_description.set_wrap(true);
        settings_description.set_xalign(0.0);
        let grid = gtk::Grid::new();
        grid.set_row_spacing(6);
        grid.set_column_spacing(12);
        let save_entry = entry("Ctrl+Alt+G", 64);
        let picker_entry = entry("Ctrl+Alt+V", 64);
        let enabled = gtk::Switch::new();
        let autostart = gtk::Switch::new();
        let notifications = gtk::Switch::new();
        for switch in [&enabled, &autostart, &notifications] {
            switch.set_halign(gtk::Align::Start);
            switch.set_valign(gtk::Align::Center);
        }
        let clip_entry = entry("25 MiB", 32);
        let vault_entry = entry("1 GiB", 32);
        let timeout_entry = entry("900", 20);
        timeout_entry.set_input_purpose(gtk::InputPurpose::Digits);
        for size in [&clip_entry, &vault_entry] {
            size.set_tooltip_text(Some(
                "A byte count, or a size such as 512 KiB, 25 MiB, or 1 GiB",
            ));
        }
        let shortcut_status = gtk::Label::new(None);
        shortcut_status.set_xalign(0.0);
        shortcut_status.set_wrap(true);
        shortcut_status.add_css_class("dim-label");
        attach_setting(&grid, 0, "_Save shortcut", &save_entry);
        attach_setting(&grid, 1, "_Picker shortcut", &picker_entry);
        attach_setting(&grid, 2, "_Enable built-in shortcuts", &enabled);
        grid.attach(&shortcut_status, 1, 3, 1, 1);
        attach_setting(&grid, 4, "Start with the desktop session", &autostart);
        attach_setting(&grid, 5, "Desktop notifications", &notifications);
        attach_setting(&grid, 6, "_Max clip size", &clip_entry);
        attach_setting(&grid, 7, "Ma_x vault size", &vault_entry);
        attach_setting(&grid, 8, "_Lock timeout (seconds)", &timeout_entry);
        let apply = gtk::Button::with_mnemonic("_Apply");
        apply.add_css_class("suggested-action");
        let settings_back = gtk::Button::with_mnemonic("Back to _history");
        let settings_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        settings_actions.append(&apply);
        settings_actions.append(&settings_back);
        let settings_status = gtk::Label::new(None);
        settings_status.set_wrap(true);
        settings_status.set_xalign(0.0);
        let sensitive_title = gtk::Label::new(Some("Sensitive app identities"));
        sensitive_title.set_xalign(0.0);
        let sensitive_description = gtk::Label::new(Some(
            "Observed identities such as x11:wm-class:KeePassXC. Matching is \
             case-insensitive and stays encrypted. Manual labels never bypass warnings.",
        ));
        sensitive_description.set_wrap(true);
        sensitive_description.set_xalign(0.0);
        let sensitive_list = gtk::ListBox::new();
        sensitive_list.set_selection_mode(gtk::SelectionMode::None);
        let sensitive_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(120)
            .max_content_height(220)
            .child(&sensitive_list)
            .build();
        let sensitive_entry = entry("x11:wm-class:NewApp", 512);
        let sensitive_add = gtk::Button::with_mnemonic("Add _rule");
        let sensitive_label = gtk::Label::with_mnemonic("Add observed app _identity");
        sensitive_label.set_mnemonic_widget(Some(&sensitive_entry));
        sensitive_label.set_xalign(0.0);
        let sensitive_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        sensitive_actions.append(&sensitive_entry);
        sensitive_actions.append(&sensitive_add);
        sensitive_entry.set_hexpand(true);
        for widget in [
            &settings_title.clone().upcast::<gtk::Widget>(),
            &settings_description.clone().upcast(),
            &grid.upcast(),
            &settings_actions.clone().upcast(),
            &settings_status.clone().upcast(),
            &sensitive_title.clone().upcast(),
            &sensitive_description.clone().upcast(),
            &sensitive_scroll.clone().upcast(),
            &sensitive_label.clone().upcast(),
            &sensitive_actions.clone().upcast(),
        ] {
            settings_page.append(widget);
        }
        stack.add_named(&history_page, Some("history"));
        let settings_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&settings_page)
            .build();
        stack.add_named(&settings_scroll, Some("settings"));
        stack.set_visible_child_name("history");
        root.append(&stack);
        let (send, receive) = sync::sync_channel(1);
        let runner = Arc::new(Runner {
            latest: Mutex::new(None),
            active: Mutex::new(None),
            wake: send,
            ticket: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        });
        let picker = Rc::new(Self {
            root,
            stack,
            search,
            source,
            tag,
            kind,
            categories,
            list,
            status,
            usage,
            picture,
            details,
            tags_box,
            tag_entry,
            add_tag,
            copy,
            copy_delete,
            copy_keep,
            previous,
            next,
            delete,
            delete_selected,
            select_page,
            settings,
            refresh,
            save_entry,
            picker_entry,
            enabled,
            autostart,
            notifications,
            clip_entry,
            vault_entry,
            timeout_entry,
            settings_status,
            shortcut_status,
            sensitive_list,
            sensitive_entry,
            sensitive_add,
            settings_back,
            window: window.downgrade(),
            me: RefCell::new(Weak::new()),
            model: RefCell::new(Model {
                state: None,
                active: false,
                page: None,
                selected: None,
                busy: false,
            }),
            updating: Cell::new(false),
            source_id: Cell::new(None),
            pending_select: Cell::new(None),
            pending_offset: Cell::new(0),
            checks: RefCell::new(Vec::new()),
            category_buttons: RefCell::new(Vec::new()),
            stale: Cell::new(false),
            config_loaded: Cell::new(false),
            runner: runner.clone(),
            query_delay: Rc::new(QueryDelay::default()),
        });
        *picker.me.borrow_mut() = Rc::downgrade(&picker);
        let (mut signal, mut receiver) = mpsc::channel(1);
        let slot = Arc::new(Mutex::new(None::<Done>));
        let worker_slot = slot.clone();
        thread::spawn(move || {
            while receive.recv().is_ok() {
                if runner.closed.load(Ordering::SeqCst) {
                    break;
                }
                let Some(job) = runner.latest.lock().expect("picker queue").take() else {
                    continue;
                };
                let result = (|| {
                    let mut stream = Paths::discover()?.connect()?;
                    {
                        let mut active = runner.active.lock().map_err(|_| Error::Io)?;
                        if runner.closed.load(Ordering::SeqCst)
                            || runner.ticket.load(Ordering::SeqCst) != job.ticket
                        {
                            return Err(Error::Canceled);
                        }
                        *active = Some(stream.try_clone()?);
                    }
                    ipc::exchange_reply(&mut stream, job.request)
                })();
                if runner.ticket.load(Ordering::SeqCst) == job.ticket
                    && !runner.closed.load(Ordering::SeqCst)
                {
                    runner.active.lock().expect("picker connection").take();
                    *worker_slot.lock().expect("picker result") = Some(Done {
                        ticket: job.ticket,
                        action: job.action,
                        result,
                    });
                    let _ = signal.try_send(());
                }
            }
        });
        let weak = Rc::downgrade(&picker);
        glib::MainContext::default().spawn_local(async move {
            while receiver.next().await.is_some() {
                let Some(picker) = weak.upgrade() else {
                    break;
                };
                let done = slot.lock().expect("picker result").take();
                if let Some(done) = done {
                    picker.complete(done);
                }
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.search.connect_changed(move |_| {
            if let Some(p) = weak.upgrade() {
                p.queue_query();
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.source.connect_changed(move |_| {
            if let Some(p) = weak.upgrade() {
                p.source_id.set(None);
                p.queue_query();
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.tag.connect_changed(move |_| {
            if let Some(p) = weak.upgrade() {
                p.queue_query();
            }
        });
        for entry in [&picker.source, &picker.tag] {
            let weak = Rc::downgrade(&picker);
            entry.connect_activate(move |_| {
                if let Some(p) = weak.upgrade() {
                    p.copy(true);
                }
            });
        }
        let weak = Rc::downgrade(&picker);
        picker.kind.connect_changed(move |_| {
            if let Some(p) = weak.upgrade() {
                p.query(0);
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.list.connect_row_selected(move |_, row| {
            if let Some(p) = weak.upgrade() {
                p.selected(row.map(|r| r.index() as usize));
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.list.connect_row_activated(move |_, _| {
            if let Some(p) = weak.upgrade() {
                p.copy(true);
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.copy.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.copy(true);
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.copy_keep.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.copy(false);
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.copy_delete.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.copy_delete();
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.previous.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                let offset = p
                    .model
                    .borrow()
                    .page
                    .as_ref()
                    .map_or(0, |v| v.offset.saturating_sub(PAGE_SIZE as u64));
                p.query(offset);
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.next.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                let offset = p
                    .model
                    .borrow()
                    .page
                    .as_ref()
                    .map_or(0, |v| v.offset + PAGE_SIZE as u64);
                p.query(offset);
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.refresh.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.refresh();
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.settings.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.open_settings();
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.add_tag.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.add_tag();
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.tag_entry.connect_activate(move |_| {
            if let Some(p) = weak.upgrade() {
                p.add_tag();
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.delete.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.delete_current();
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.delete_selected.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.delete_checked();
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.select_page.connect_toggled(move |button| {
            if let Some(p) = weak.upgrade() {
                p.toggle_page(button.is_active());
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.settings_back.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.stack.set_visible_child_name("history");
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.sensitive_add.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.add_sensitive();
            }
        });
        let weak = Rc::downgrade(&picker);
        picker.sensitive_entry.connect_activate(move |_| {
            if let Some(p) = weak.upgrade() {
                p.add_sensitive();
            }
        });
        let weak = Rc::downgrade(&picker);
        apply.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.apply_settings();
            }
        });
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&picker);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let Some(p) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let control = modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK);
            if control
                && !modifiers.intersects(
                    gtk::gdk::ModifierType::ALT_MASK | gtk::gdk::ModifierType::SUPER_MASK,
                )
            {
                if p.editing_entry()
                    && matches!(
                        key,
                        gtk::gdk::Key::t
                            | gtk::gdk::Key::d
                            | gtk::gdk::Key::D
                            | gtk::gdk::Key::k
                            | gtk::gdk::Key::x
                    )
                {
                    return glib::Propagation::Proceed;
                }
                if !p.on_history()
                    && matches!(
                        key,
                        gtk::gdk::Key::Return
                            | gtk::gdk::Key::KP_Enter
                            | gtk::gdk::Key::f
                            | gtk::gdk::Key::t
                            | gtk::gdk::Key::d
                            | gtk::gdk::Key::D
                            | gtk::gdk::Key::k
                            | gtk::gdk::Key::x
                    )
                {
                    return glib::Propagation::Proceed;
                }
                match key {
                    gtk::gdk::Key::f => schedule(p.weak(), |p| {
                        p.search.grab_focus();
                    }),
                    gtk::gdk::Key::t => {
                        schedule(p.weak(), |p| {
                            p.tag_entry.grab_focus();
                        });
                    }
                    gtk::gdk::Key::d | gtk::gdk::Key::D => {
                        let shift = modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK);
                        schedule(p.weak(), move |p| {
                            if shift {
                                p.delete_checked();
                            } else {
                                p.delete_current();
                            }
                        });
                    }
                    gtk::gdk::Key::k => {
                        schedule(p.weak(), |p| {
                            p.select_page.set_active(!p.select_page.is_active());
                        });
                    }
                    gtk::gdk::Key::comma => {
                        schedule(p.weak(), |p| p.open_settings());
                    }
                    gtk::gdk::Key::x => {
                        schedule(p.weak(), |p| p.copy_delete());
                    }
                    gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter => {
                        schedule(p.weak(), |p| p.copy(false));
                    }
                    _ => return glib::Propagation::Proceed,
                };
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        picker.root.add_controller(keys);
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(&picker);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let Some(p) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if modifiers.intersects(
                gtk::gdk::ModifierType::CONTROL_MASK
                    | gtk::gdk::ModifierType::ALT_MASK
                    | gtk::gdk::ModifierType::SUPER_MASK,
            ) {
                return glib::Propagation::Proceed;
            }
            match key {
                gtk::gdk::Key::Up => p.move_selection(-1),
                gtk::gdk::Key::Down => p.move_selection(1),
                gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter => p.copy(true),
                _ => return glib::Propagation::Proceed,
            };
            glib::Propagation::Stop
        });
        picker.search.add_controller(keys);
        picker.root.set_visible(false);
        picker
    }
    pub fn set_bindings(&self, text: &str) {
        self.shortcut_status.set_text(text);
    }
    pub fn close(&self) {
        self.runner.close();
        self.clear();
    }
    pub fn state(&self, state: Option<State>, connected: bool) {
        let want = connected
            && state.is_some_and(|s| s.phase == VaultState::Unlocked && s.capture_stage != 3);
        let (was, changed) = {
            let mut model = self.model.borrow_mut();
            let was = model.active;
            let changed = model.state.zip(state).is_some_and(|(a, b)| {
                a.session != b.session || a.history_revision != b.history_revision
            });
            model.state = state;
            model.active = want;
            (was, changed)
        };
        if !want
            && was
            && let Some(window) = self.window.upgrade()
        {
            gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
        }
        self.root.set_visible(want);
        if !want {
            if was {
                self.runner.cancel();
                self.clear();
            }
            return;
        }
        if !was {
            self.stack.set_visible_child_name("history");
            self.reset_view_state();
            self.refresh();
            schedule(self.weak(), |p| {
                p.search.grab_focus();
            });
        } else if changed && self.stack.visible_child_name().as_deref() == Some("history") {
            self.refresh();
        }
    }
    pub fn focus(&self) {
        if self.model.borrow().active {
            self.search.grab_focus();
        }
    }
    fn clear_rows(&self) {
        self.updating.set(true);
        while let Some(widget) = self.list.first_child() {
            self.list.remove(&widget);
        }
        self.updating.set(false);
        self.checks.borrow_mut().clear();
        self.select_page.set_active(false);
        self.update_bulk();
        self.picture.set_paintable(None::<&gtk::gdk::Paintable>);
        self.picture.set_visible(false);
        self.details.set_text("");
        self.tags_box.set_visible(false);
        self.clear_flow(&self.tags_box);
        for button in [&self.copy, &self.copy_keep, &self.copy_delete, &self.delete] {
            button.set_sensitive(false);
        }
        self.previous.set_sensitive(false);
        self.next.set_sensitive(false);
    }
    fn clear_flow(&self, flow: &gtk::FlowBox) {
        while let Some(child) = flow.first_child() {
            flow.remove(&child);
        }
    }
    fn reset_view_state(&self) {
        self.source_id.set(None);
        self.clear_rows();
        self.clear_settings();
    }
    fn clear_settings(&self) {
        self.config_loaded.set(false);
        self.stack.set_visible_child_name("history");
        self.save_entry.set_text("");
        self.picker_entry.set_text("");
        self.enabled.set_active(false);
        self.autostart.set_active(false);
        self.notifications.set_active(false);
        self.clip_entry.set_text("");
        self.vault_entry.set_text("");
        self.timeout_entry.set_text("");
        self.settings_status.set_text("");
        self.sensitive_entry.set_text("");
        while let Some(child) = self.sensitive_list.first_child() {
            self.sensitive_list.remove(&child);
        }
    }
    fn clear(&self) {
        self.query_delay.cancel();
        self.root.set_sensitive(true);
        self.updating.set(true);
        self.search.set_text("");
        self.source.set_text("");
        self.tag.set_text("");
        self.kind.set_active_id(Some("all"));
        self.updating.set(false);
        self.source_id.set(None);
        self.pending_select.set(None);
        self.stack.set_visible_child_name("history");
        self.clear_rows();
        self.clear_settings();
        self.status.set_text("");
        self.usage.set_text("");
        self.clear_flow(&self.categories);
        self.category_buttons.borrow_mut().clear();
        let mut model = self.model.borrow_mut();
        model.page = None;
        model.selected = None;
        model.busy = false;
        self.stale.set(false);
    }
    fn queue_query(self: &Rc<Self>) {
        if self.updating.get() || !self.model.borrow().active {
            return;
        }
        if self.model.borrow().busy {
            // Never cancel an in-flight edit; reload once it completes.
            self.stale.set(true);
            return;
        }
        self.runner.cancel();
        self.clear_rows();
        {
            let mut model = self.model.borrow_mut();
            model.page = None;
            model.selected = None;
        }
        self.status.set_text("Loading saved clips…");
        let weak = Rc::downgrade(self);
        self.query_delay.schedule(move || {
            if let Some(picker) = weak.upgrade() {
                picker.query(0);
            }
        });
    }
    fn query(&self, offset: u64) {
        self.query_delay.cancel();
        if self.updating.get() || !self.model.borrow().active {
            return;
        }
        if self.model.borrow().busy {
            self.stale.set(true);
            return;
        }
        let session = self.model.borrow().state.expect("active session").session;
        let query = BrowseQuery {
            text: self.search.text().into(),
            source: self.source.text().into(),
            source_id: self.source_id.get(),
            tag: self.tag.text().into(),
            kind: match self.kind.active_id().as_deref() {
                Some("text") => Some(ContentKind::Text),
                Some("png") => Some(ContentKind::Png),
                _ => None,
            },
            full: true,
            offset,
        };
        if query.validate().is_err() {
            self.runner.cancel();
            self.clear_rows();
            {
                let mut model = self.model.borrow_mut();
                model.page = None;
                model.selected = None;
            }
            self.status.set_text("Search/filter input is too long.");
            return;
        }
        self.clear_rows();
        {
            let mut model = self.model.borrow_mut();
            model.page = None;
            model.selected = None;
        }
        self.mark_category(query.source_id);
        self.status.set_text("Loading saved clips…");
        self.runner
            .submit(Request::Browse { session, query }, Action::Page);
    }
    fn mark_category(&self, active: Option<i64>) {
        for (id, button) in self.category_buttons.borrow().iter() {
            if *id == active {
                button.add_css_class("suggested-action");
            } else {
                button.remove_css_class("suggested-action");
            }
        }
    }
    fn current_offset(&self) -> u64 {
        self.model
            .borrow()
            .page
            .as_ref()
            .map_or(0, |page| page.offset)
    }
    fn refresh(&self) {
        self.load_manager(self.current_offset());
    }
    fn on_history(&self) -> bool {
        self.stack.visible_child_name().as_deref() == Some("history")
    }
    fn editing_entry(&self) -> bool {
        let Some(window) = self.window.upgrade() else {
            return false;
        };
        gtk::prelude::RootExt::focus(&window)
            .is_some_and(|focus| focus.ancestor(gtk::Entry::static_type()).is_some())
    }
    fn load_manager(&self, offset: u64) {
        let session = match self.model.borrow().state {
            Some(state) => state.session,
            _ => return,
        };
        if self.model.borrow().busy {
            self.stale.set(true);
            return;
        }
        self.pending_offset.set(offset);
        self.status.set_text("Loading saved clips…");
        self.runner
            .submit(Request::ManagerInfo { session }, Action::Info);
    }
    fn selected(&self, index: Option<usize>) {
        if self.updating.get() {
            return;
        }
        self.picture.set_paintable(None::<&gtk::gdk::Paintable>);
        self.picture.set_visible(false);
        self.tags_box.set_visible(false);
        self.clear_flow(&self.tags_box);
        let choice = {
            let mut model = self.model.borrow_mut();
            let row = index.and_then(|i| model.page.as_ref().and_then(|p| p.rows.get(i)));
            let choice = row.map(|r| {
                (
                    r.id,
                    r.kind,
                    r.byte_count,
                    r.text.clone(),
                    r.tags.clone(),
                    r.source.clone(),
                    r.first_saved_ms,
                    r.last_saved_ms,
                )
            });
            model.selected = choice.as_ref().map(|c| c.0);
            choice
        };
        let busy = self.model.borrow().busy;
        if busy {
            // The edit in flight must finish; reload afterwards to fetch any preview.
            self.stale.set(true);
        } else {
            self.runner.cancel();
        }
        self.copy.set_sensitive(!busy && choice.is_some());
        self.copy_keep.set_sensitive(!busy && choice.is_some());
        self.copy_delete.set_sensitive(!busy && choice.is_some());
        self.delete.set_sensitive(!busy && choice.is_some());
        if let Some((id, kind, bytes, text, tags, source, first, last)) = choice {
            let mut details = format!(
                "{} · {} bytes\nFirst saved: {}\nLast saved: {}",
                super::source_description(&source),
                bytes,
                timestamp(first),
                timestamp(last)
            );
            if !tags.is_empty() {
                details.push_str(&format!("\nTags: {}", tags.join(", ")));
            }
            if kind == ContentKind::Text && !text.is_empty() {
                details.push_str("\n\n");
                details.push_str(&text.replace('\0', "␀"));
            }
            self.details.set_text(&details);
            self.populate_tags(id, &tags);
            self.tags_box.set_visible(true);
            if kind == ContentKind::Png && !busy {
                let session = self.model.borrow().state.expect("session").session;
                self.runner
                    .submit(Request::Preview { session, id }, Action::Preview);
            }
        } else {
            self.details.set_text("");
        }
    }
    fn populate_tags(&self, id: i64, tags: &[String]) {
        self.clear_flow(&self.tags_box);
        for tag in tags {
            let button = gtk::Button::with_label(&format!("× {tag}"));
            button.set_tooltip_text(Some("Remove this tag from the selected item"));
            let weak = self.weak();
            let tag = tag.clone();
            button.connect_clicked(move |_| {
                if let Some(p) = weak.upgrade() {
                    p.remove_tag(id, &tag);
                }
            });
            self.tags_box.insert(&button, -1);
        }
    }
    fn move_selection(&self, delta: i32) {
        if self.model.borrow().busy {
            return;
        }
        let len = self
            .model
            .borrow()
            .page
            .as_ref()
            .map_or(0, |p| p.rows.len());
        if len == 0 {
            return;
        }
        let next = match self.list.selected_row() {
            Some(row) => (row.index() + delta).clamp(0, len as i32 - 1),
            None => 0,
        };
        self.list.select_row(self.list.row_at_index(next).as_ref());
    }
    fn copy(&self, close: bool) {
        let model = self.model.borrow();
        if !model.active || model.busy {
            return;
        }
        let Some(id) = model.selected else {
            return;
        };
        let session = model.state.expect("active").session;
        drop(model);
        self.status.set_text("Copying saved clip…");
        self.mutate(Request::Copy { session, id }, Action::Copy { close });
    }
    fn copy_delete(&self) {
        let model = self.model.borrow();
        if !model.active || model.busy {
            return;
        }
        let Some(id) = model.selected else {
            return;
        };
        let session = model.state.expect("active").session;
        drop(model);
        let weak = self.weak();
        self.confirm(
            "Copy the selected item, then delete it from history?\nThis cannot be undone.",
            Rc::new(move || {
                if let Some(p) = weak.upgrade()
                    && p.mutate(Request::Copy { session, id }, Action::CopyDeleteCopy(id))
                {
                    p.status
                        .set_text("Copying the selected item before deleting it from history…");
                }
            }),
        );
    }
    fn complete(&self, done: Done) {
        if done.ticket != self.runner.ticket.load(Ordering::SeqCst) || !self.model.borrow().active {
            return;
        }
        self.model.borrow_mut().busy = false;
        self.root.set_sensitive(true);
        let ticket = done.ticket;
        self.handle(done);
        // Reads deferred during an edit run now, unless the handler already
        // started newer work (which reloads the same widgets anyway).
        if self.stale.get()
            && !self.model.borrow().busy
            && self.runner.ticket.load(Ordering::SeqCst) == ticket
            && self.model.borrow().active
        {
            self.stale.set(false);
            self.refresh();
        }
    }
    fn handle(&self, done: Done) {
        match done.result {
            Ok(Reply::Page(page)) => {
                if !self
                    .model
                    .borrow()
                    .state
                    .is_some_and(|s| s.session == page.session)
                {
                    return;
                }
                self.show_page(page);
            }
            Ok(Reply::Image(image)) => {
                if !self
                    .model
                    .borrow()
                    .state
                    .is_some_and(|s| s.session == image.session)
                    || self.model.borrow().selected != Some(image.id)
                {
                    return;
                }
                let bytes = glib::Bytes::from_owned(image.rgba);
                let texture = gtk::gdk::MemoryTexture::new(
                    image.width.into(),
                    image.height.into(),
                    gtk::gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    image.width as usize * 4,
                );
                self.picture.set_paintable(Some(&texture));
                self.picture.set_visible(true);
            }
            Ok(Reply::State(_)) if matches!(done.action, Action::Copy { .. }) => {
                if matches!(done.action, Action::Copy { close: false }) {
                    self.status
                        .set_text("Copied the selected item. Paste manually into the destination.");
                    self.restore_item_actions();
                    self.search.grab_focus();
                } else if let Some(window) = self.window.upgrade() {
                    window.close();
                }
            }
            Ok(Reply::State(_)) if matches!(done.action, Action::CopyDeleteCopy(_)) => {
                let Action::CopyDeleteCopy(id) = done.action else {
                    return;
                };
                let Some(session) = self.model.borrow().state.map(|state| state.session) else {
                    return;
                };
                self.status
                    .set_text("Copied. Deleting the item from history…");
                self.mutate(Request::Delete { session, id }, Action::CopyDeleteDelete);
            }
            Ok(Reply::ManagerInfo(info)) => self.show_info(info),
            Ok(Reply::Config(config)) => self.show_config(config, done.action),
            Ok(Reply::SensitiveApps(apps)) => self.show_sensitive(apps),
            Ok(Reply::Tags { id, tags }) => {
                self.status.set_text("Tags updated.");
                self.pending_select.set(Some(id));
                self.tags_box.set_visible(true);
                self.populate_tags(id, &tags);
                self.search.grab_focus();
                self.refresh();
            }
            Ok(Reply::Deleted { count }) => {
                let one_time = matches!(done.action, Action::CopyDeleteDelete);
                self.status.set_text(&if one_time {
                    format!("Copied and deleted {count} saved item(s).")
                } else {
                    format!("Deleted {count} saved item(s).")
                });
                if one_time {
                    self.search.grab_focus();
                }
                self.pending_select.set(None);
                let offset = self.current_offset();
                let next = if count > 0
                    && self
                        .model
                        .borrow()
                        .page
                        .as_ref()
                        .is_some_and(|page| page.rows.len() as u64 <= count && offset > 0)
                {
                    offset.saturating_sub(PAGE_SIZE as u64)
                } else {
                    offset
                };
                self.refresh_at(next);
            }
            Err(error) => {
                self.status.set_text(&error.to_string());
                if matches!(done.action, Action::Config | Action::ApplyConfig) {
                    self.settings_status.set_text(&error.to_string());
                }
                self.restore_item_actions();
            }
            _ => self
                .status
                .set_text("Unexpected service response; reopen the window."),
        }
    }
    fn refresh_at(&self, offset: u64) {
        self.load_manager(offset);
    }
    fn restore_item_actions(&self) {
        let selected = self.model.borrow().selected.is_some();
        for button in [&self.copy, &self.copy_keep, &self.copy_delete, &self.delete] {
            button.set_sensitive(selected);
        }
    }
    fn show_page(&self, page: Page) {
        self.clear_rows();
        self.updating.set(true);
        for item in &page.rows {
            let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
            column.set_margin_top(5);
            column.set_margin_bottom(5);
            let title = gtk::Label::new(Some(&if item.kind == ContentKind::Text {
                item.text.replace('\0', "␀").replace('\n', " ↵ ")
            } else {
                "[Saved image]".into()
            }));
            title.set_xalign(0.0);
            title.set_wrap(true);
            title.set_lines(2);
            title.set_ellipsize(gtk::pango::EllipsizeMode::End);
            title.set_max_width_chars(60);
            let source = gtk::Label::new(Some(&super::source_description(&item.source)));
            source.set_xalign(0.0);
            source.set_ellipsize(gtk::pango::EllipsizeMode::End);
            source.set_max_width_chars(60);
            source.add_css_class("dim-label");
            column.append(&title);
            column.append(&source);
            {
                let meta = gtk::Label::new(Some(&format!(
                    "{} · {} bytes · {}",
                    if item.kind == ContentKind::Text {
                        "Text"
                    } else {
                        "PNG"
                    },
                    item.byte_count,
                    timestamp(item.last_saved_ms)
                )));
                meta.set_xalign(0.0);
                meta.add_css_class("dim-label");
                column.append(&meta);
            }
            {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let check = gtk::CheckButton::new();
                check.set_tooltip_text(Some("Include this item in the checked deletion"));
                let weak = self.weak();
                check.connect_toggled(move |_| {
                    if let Some(p) = weak.upgrade() {
                        p.update_bulk();
                    }
                });
                self.checks.borrow_mut().push((item.id, check.clone()));
                row.append(&check);
                row.append(&column);
                column.set_hexpand(true);
                let wrapper = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                wrapper.append(&row);
                wrapper.set_hexpand(true);
                self.list.append(&wrapper);
            }
        }
        self.updating.set(false);
        self.select_page.set_active(false);
        self.update_bulk();
        self.status.set_text(&if page.rows.is_empty() {
            "No saved clips match these filters.".into()
        } else {
            format!(
                "Items {}–{}{}",
                page.offset + 1,
                page.offset + page.rows.len() as u64,
                if page.more { " · more available" } else { "" }
            )
        });
        self.previous.set_sensitive(page.offset > 0);
        self.next.set_sensitive(page.more);
        let target = self
            .pending_select
            .take()
            .or_else(|| self.model.borrow().selected)
            .and_then(|id| page.rows.iter().position(|row| row.id == id))
            .or(if page.rows.is_empty() { None } else { Some(0) });
        self.model.borrow_mut().page = Some(page);
        if let Some(index) = target {
            self.updating.set(true);
            let row = self.list.row_at_index(index as i32);
            self.list.select_row(row.as_ref());
            self.updating.set(false);
            self.selected(Some(index));
        } else {
            self.selected(None);
            self.model.borrow_mut().selected = None;
        }
    }
    fn show_info(&self, info: ManagerInfo) {
        if !self
            .model
            .borrow()
            .state
            .is_some_and(|state| state.session == info.session)
        {
            return;
        }
        self.usage.set_text(&format!(
            "Items: {} · Logical usage: {} · Database size: {} (not a quota)",
            info.items,
            format_bytes(info.logical_bytes),
            format_bytes(info.physical_bytes)
        ));
        self.clear_flow(&self.categories);
        self.category_buttons.borrow_mut().clear();
        let all = gtk::Button::with_label("All sources");
        all.set_tooltip_text(Some("Clear the exact source-category filter"));
        let weak = self.weak();
        all.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.source.set_text("");
                p.source_id.set(None);
                p.query(0);
            }
        });
        self.categories.insert(&all, -1);
        self.category_buttons.borrow_mut().push((None, all));
        for facet in &info.sources {
            let button = gtk::Button::with_label(&format!(
                "{} ({})",
                category_label(&facet.source),
                facet.clips
            ));
            button.set_tooltip_text(Some(&super::source_description(&facet.source)));
            let weak = self.weak();
            let id = facet.id;
            button.connect_clicked(move |_| {
                if let Some(p) = weak.upgrade() {
                    p.source.set_text("");
                    p.source_id.set(Some(id));
                    p.query(0);
                }
            });
            self.categories.insert(&button, -1);
            self.category_buttons.borrow_mut().push((Some(id), button));
        }
        // Completing the metadata request starts the page request.
        self.query(self.pending_offset.get());
    }
    fn show_config(&self, config: ConfigView, action: Action) {
        if !self
            .model
            .borrow()
            .state
            .is_some_and(|state| state.session == config.session)
        {
            return;
        }
        self.save_entry.set_text(&config.save);
        self.picker_entry.set_text(&config.picker);
        self.enabled.set_active(config.enabled);
        self.autostart.set_active(config.autostart);
        self.notifications.set_active(config.notifications);
        self.clip_entry.set_text(&size_text(config.max_clip_bytes));
        self.vault_entry
            .set_text(&size_text(config.max_vault_bytes));
        self.timeout_entry
            .set_text(&config.lock_timeout_seconds.to_string());
        self.config_loaded.set(true);
        self.settings_status.set_text("");
        if matches!(action, Action::ApplyConfig) {
            self.settings_status
                .set_text("Settings applied. Shortcut status is shown under the shortcut switch.");
        } else {
            let session = config.session;
            self.runner
                .submit(Request::SensitiveApps { session }, Action::Sensitive);
        }
    }
    fn show_sensitive(&self, apps: Vec<String>) {
        while let Some(child) = self.sensitive_list.first_child() {
            self.sensitive_list.remove(&child);
        }
        for app in apps {
            let row = gtk::ListBoxRow::new();
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            line.set_margin_top(3);
            line.set_margin_bottom(3);
            let label = gtk::Label::new(Some(&app));
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_selectable(true);
            let remove = gtk::Button::with_label("Remove");
            let weak = self.weak();
            let value = app.clone();
            remove.connect_clicked(move |_| {
                if let Some(p) = weak.upgrade() {
                    p.remove_sensitive(&value);
                }
            });
            line.append(&label);
            line.append(&remove);
            row.set_child(Some(&line));
            self.sensitive_list.append(&row);
        }
    }
    fn open_settings(&self) {
        if self.model.borrow().busy {
            return;
        }
        self.stack.set_visible_child_name("settings");
        if !self.config_loaded.get() {
            let session = match self.model.borrow().state {
                Some(state) => state.session,
                None => return,
            };
            self.settings_status.set_text("Loading settings…");
            self.runner
                .submit(Request::Config { session }, Action::Config);
        }
    }
    fn apply_settings(&self) {
        let session = match self.model.borrow().state {
            Some(state) => state.session,
            None => return,
        };
        let size = |entry: &gtk::Entry, name: &str| -> Option<u64> {
            let value = parse_size(&entry.text());
            if value.is_none() {
                self.settings_status.set_text(&format!(
                    "{name} must be a positive size, such as 26214400, 25 MiB, or 1 GiB."
                ));
            }
            value
        };
        let Some(max_clip_bytes) = size(&self.clip_entry, "Maximum clip size") else {
            return;
        };
        let Some(max_vault_bytes) = size(&self.vault_entry, "Maximum vault size") else {
            return;
        };
        let Some(lock_timeout_seconds) = self
            .timeout_entry
            .text()
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
        else {
            self.settings_status
                .set_text("Lock timeout must be a positive number of seconds.");
            return;
        };
        self.settings_status.set_text("Applying settings…");
        self.mutate(
            Request::SetConfig {
                session,
                enabled: self.enabled.is_active(),
                autostart: self.autostart.is_active(),
                notifications: self.notifications.is_active(),
                save: self.save_entry.text().into(),
                picker: self.picker_entry.text().into(),
                max_clip_bytes,
                max_vault_bytes,
                lock_timeout_seconds,
            },
            Action::ApplyConfig,
        );
    }
    fn add_sensitive(&self) {
        let value = self.sensitive_entry.text().to_string();
        if value.trim().is_empty() {
            self.settings_status
                .set_text("Enter an observed app identity before adding a rule.");
            return;
        }
        let session = match self.model.borrow().state {
            Some(state) => state.session,
            None => return,
        };
        self.sensitive_entry.set_text("");
        self.mutate(
            Request::AddSensitive {
                session,
                app_id: value,
            },
            Action::Sensitive,
        );
    }
    fn remove_sensitive(&self, app_id: &str) {
        let session = match self.model.borrow().state {
            Some(state) => state.session,
            None => return,
        };
        self.mutate(
            Request::RemoveSensitive {
                session,
                app_id: app_id.to_owned(),
            },
            Action::Sensitive,
        );
    }
    fn add_tag(&self) {
        if self.model.borrow().busy {
            return;
        }
        let Some(id) = self.model.borrow().selected else {
            return;
        };
        let tag = self.tag_entry.text().to_string();
        if tag.trim().is_empty() {
            self.status.set_text("Enter a tag name first.");
            return;
        }
        let session = match self.model.borrow().state {
            Some(state) => state.session,
            None => return,
        };
        self.tag_entry.set_text("");
        self.search.grab_focus();
        self.mutate(Request::AddTag { session, id, tag }, Action::Tags);
    }
    fn remove_tag(&self, id: i64, tag: &str) {
        if self.model.borrow().busy {
            return;
        }
        let session = match self.model.borrow().state {
            Some(state) => state.session,
            None => return,
        };
        self.mutate(
            Request::RemoveTag {
                session,
                id,
                tag: tag.to_owned(),
            },
            Action::Tags,
        );
    }
    fn delete_current(&self) {
        if self.model.borrow().busy {
            return;
        }
        let Some(id) = self.model.borrow().selected else {
            return;
        };
        let session = match self.model.borrow().state {
            Some(state) => state.session,
            None => return,
        };
        let weak = self.weak();
        self.confirm(
            "Delete the selected saved item?\nThis cannot be undone.",
            Rc::new(move || {
                if let Some(p) = weak.upgrade() {
                    p.pending_select.set(None);
                    p.mutate(Request::Delete { session, id }, Action::Deleted);
                }
            }),
        );
    }
    fn delete_checked(&self) {
        if self.model.borrow().busy {
            return;
        }
        let ids = self.checked_ids();
        if ids.is_empty() {
            self.status
                .set_text("Check at least one item on this page first.");
            return;
        }
        let session = match self.model.borrow().state {
            Some(state) => state.session,
            None => return,
        };
        let weak = self.weak();
        let count = ids.len();
        let ids = RefCell::new(Some(ids));
        self.confirm(
            &format!("Delete {count} checked saved item(s)?\nThis cannot be undone."),
            Rc::new(move || {
                if let (Some(p), Some(ids)) = (weak.upgrade(), ids.borrow_mut().take()) {
                    p.pending_select.set(None);
                    p.mutate(Request::DeleteMany { session, ids }, Action::Deleted);
                }
            }),
        );
    }
    fn confirm(&self, text: &str, action: Rc<dyn Fn()>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let dialog = gtk::MessageDialog::builder()
            .text(text)
            .modal(true)
            .transient_for(&window)
            .buttons(gtk::ButtonsType::OkCancel)
            .build();
        dialog.set_default_response(gtk::ResponseType::Ok);
        let action = RefCell::new(Some(action));
        dialog.connect_response(move |dialog, response| {
            if response == gtk::ResponseType::Ok
                && let Some(action) = action.borrow_mut().take()
            {
                action();
            }
            dialog.close();
        });
        dialog.present();
    }
    fn checked_ids(&self) -> Vec<i64> {
        self.checks
            .borrow()
            .iter()
            .filter(|(_, check)| check.is_active())
            .map(|(id, _)| *id)
            .collect()
    }
    fn toggle_page(&self, active: bool) {
        if self.updating.get() {
            return;
        }
        for (_, check) in self.checks.borrow().iter() {
            check.set_active(active);
        }
        self.update_bulk();
    }
    fn update_bulk(&self) {
        let count = self.checked_ids().len();
        self.delete_selected.set_sensitive(count > 0);
        if count > 0 {
            self.delete_selected
                .set_label(&format!("Delete c_hecked ({count})"));
        } else {
            self.delete_selected.set_label("Delete c_hecked");
        }
    }
    /// Starts an edit. Reads submitted while it runs are deferred rather than
    /// canceling it, so its reply (and follow-up actions) cannot be lost.
    fn mutate(&self, request: Request, action: Action) -> bool {
        {
            let mut model = self.model.borrow_mut();
            if !model.active || model.busy {
                return false;
            }
            model.busy = true;
        }
        self.query_delay.cancel();
        if !self.runner.idle() {
            self.stale.set(true);
        }
        for button in [&self.copy, &self.copy_keep, &self.copy_delete, &self.delete] {
            button.set_sensitive(false);
        }
        self.runner.submit(request, action);
        true
    }
    fn weak(&self) -> Weak<Picker> {
        self.me.borrow().clone()
    }
}

fn schedule(weak: Weak<Picker>, action: impl FnOnce(&Picker) + 'static) {
    glib::idle_add_local_once(move || {
        if let Some(picker) = weak.upgrade() {
            action(&picker);
        }
    });
}
fn entry(placeholder: &str, max: i32) -> gtk::Entry {
    gtk::Entry::builder()
        .placeholder_text(placeholder)
        .max_length(max)
        .build()
}
fn attach_setting(grid: &gtk::Grid, row: i32, label: &str, widget: &impl IsA<gtk::Widget>) {
    let text = gtk::Label::with_mnemonic(label);
    text.set_xalign(0.0);
    text.set_mnemonic_widget(Some(widget));
    grid.attach(&text, 0, row, 1, 1);
    grid.attach(widget, 1, row, 1, 1);
}
fn timestamp(milliseconds: i64) -> String {
    let seconds = milliseconds.div_euclid(1000).max(0);
    glib::DateTime::from_unix_local(seconds)
        .ok()
        .and_then(|date| date.format("%Y-%m-%d %H:%M").ok())
        .map(|text| text.to_string())
        .unwrap_or_else(|| "unknown".into())
}
fn format_bytes(bytes: u64) -> String {
    let mib = bytes as f64 / (1024.0 * 1024.0);
    if mib >= 1.0 {
        format!("{mib:.2} MiB")
    } else {
        let kib = bytes as f64 / 1024.0;
        if kib >= 1.0 {
            format!("{kib:.1} KiB")
        } else {
            format!("{bytes} B")
        }
    }
}
const KIB: u64 = 1024;
/// Shows exact binary multiples with a unit; other values stay in bytes.
fn size_text(bytes: u64) -> String {
    for (unit, name) in [(KIB * KIB * KIB, "GiB"), (KIB * KIB, "MiB"), (KIB, "KiB")] {
        if bytes >= unit && bytes.is_multiple_of(unit) {
            return format!("{} {name}", bytes / unit);
        }
    }
    bytes.to_string()
}
/// Accepts a byte count or a whole/decimal number with a binary unit
/// (K/KiB/KB, M/MiB/MB, G/GiB/GB, B), case-insensitive. Results must be a
/// positive whole number of bytes.
fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => KIB,
        "m" | "mb" | "mib" => KIB * KIB,
        "g" | "gb" | "gib" => KIB * KIB * KIB,
        _ => return None,
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    if whole.is_empty() && fraction.is_empty() || fraction.len() > 9 {
        return None;
    }
    let whole: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let mut bytes = whole.checked_mul(multiplier)?;
    if !fraction.is_empty() {
        let scale = 10u64.pow(fraction.len() as u32);
        let part = fraction.parse::<u64>().ok()?.checked_mul(multiplier)?;
        if !part.is_multiple_of(scale) {
            return None;
        }
        bytes = bytes.checked_add(part / scale)?;
    }
    (bytes > 0).then_some(bytes)
}
fn category_label(source: &snippy_core::source::SourceIdentity) -> String {
    use snippy_core::source::SourceIdentity as S;
    match source {
        S::Observed { display_name, .. } => display_name.clone(),
        S::Manual { label } => format!("Manual: {label}"),
        S::Unknown => "Unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_sizes_round_trip_with_units() {
        assert_eq!(size_text(26_214_400), "25 MiB");
        assert_eq!(size_text(1 << 30), "1 GiB");
        assert_eq!(size_text(1536), "1536");
        assert_eq!(size_text(4096), "4 KiB");
        assert_eq!(size_text(4097), "4097");
        for bytes in [1, 4095, 4096, 26_214_400, 1 << 30, 3 << 30, 123_456_789] {
            assert_eq!(parse_size(&size_text(bytes)), Some(bytes));
        }
        assert_eq!(parse_size("25 MiB"), Some(26_214_400));
        assert_eq!(parse_size(" 25mb "), Some(26_214_400));
        assert_eq!(parse_size("1.5 GiB"), Some(1_610_612_736));
        assert_eq!(parse_size(".5 KiB"), Some(512));
        assert_eq!(parse_size("8190"), Some(8190));
        for invalid in [
            "",
            "0",
            "0 MiB",
            "-1",
            "1.5",
            "1 TiB",
            "MiB",
            ".",
            "1.2.3 KiB",
            "99999999999999999999",
        ] {
            assert_eq!(parse_size(invalid), None, "{invalid}");
        }
        assert_eq!(parse_size(&format!("{} GiB", u64::MAX)), None);
    }
    #[test]
    fn quick_edits_coalesce_and_lock_cancels_the_pending_query() {
        let context = glib::MainContext::default();
        let _guard = context.acquire().unwrap();
        let delay = Rc::new(QueryDelay::default());
        let results = Rc::new(RefCell::new(Vec::new()));
        for value in ["S", "Synthetic", "Synthetic note 07"] {
            let results = results.clone();
            delay.schedule(move || results.borrow_mut().push(value));
        }
        assert!(results.borrow().is_empty());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while results.borrow().is_empty() && std::time::Instant::now() < deadline {
            while context.pending() {
                context.iteration(false);
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*results.borrow(), ["Synthetic note 07"]);
        let canceled = Rc::new(Cell::new(false));
        let fired = canceled.clone();
        delay.schedule(move || fired.set(true));
        delay.cancel();
        let end = std::time::Instant::now() + Duration::from_millis(200);
        while std::time::Instant::now() < end {
            while context.pending() {
                context.iteration(false);
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!canceled.get());
    }
}
