//! Poll-driven service with one bounded, blocking vault worker.
mod capture;
mod client;
mod lifecycle;
mod worker;
use crate::{
    ipc::{self, Error, Paths, Reply, Request},
    settings::Overrides,
    storage::Vault,
    x11::{Action, BindingState, Observer, Update},
};
use client::Client;
use lifecycle::Lifecycle;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use snippy_core::config::AppConfig;
use std::{
    fs::{self, File},
    io::Read,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use worker::{Job, Work, WorkKey, WorkResult, Worker};

pub struct Options {
    pub paths: Paths,
    /// Saved preferences, shown and edited by the manager.
    pub base: AppConfig,
    /// One-run daemon overrides that take precedence but are never saved.
    pub overrides: Overrides,
    pub settings: PathBuf,
    pub x11: bool,
}
pub fn run(options: Options) -> Result<(), Error> {
    crate::settings::validate(&options.base)?;
    let config = options.overrides.apply(&options.base)?;
    // Fail before creating runtime files when the required display is unavailable.
    if options.x11
        && (std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("wayland")
            || std::env::var_os("DISPLAY").is_none_or(|display| display.is_empty()))
    {
        return Err(Error::X11);
    }
    // Take the single-instance lock before grabbing shortcuts or touching autostart.
    let guard = Listener::bind(&options.paths)?;
    let observer = if options.x11 {
        Some(Observer::connect(&config.shortcuts).map_err(|_| Error::X11)?)
    } else {
        None
    };
    let autostart = crate::autostart::Autostart::discover()?;
    let executable = std::env::current_exe().map_err(|_| Error::Io)?;
    if options.base.autostart {
        autostart.set(true, &executable)?;
    } else {
        // Best-effort cleanup of a stale entry we created; ignore foreign files.
        let _ = autostart.set(false, &executable);
    }
    let vault = Vault::at(&options.paths.vault, config.limits.clone()).map_err(Error::from)?;
    let control = Lifecycle::new(
        fs::symlink_metadata(vault.path()).is_ok(),
        config.lock_timeout,
    );
    let worker = Worker::start(vault, control.epoch.clone())?;
    let signals = Signals::new()?;
    let mut server = Server {
        listener: Some(guard),
        control,
        worker,
        signals,
        observer,
        clients: Vec::new(),
        pending: None,
        lock_waiters: Vec::new(),
        closing: false,
        child: None,
        reopen: false,
        next_client: 1,
        capture: None,
        transfer: None,
        next_capture: 0,
        limits: config.limits.clone(),
        config,
        base: options.base,
        overrides: options.overrides,
        settings: options.settings,
        autostart,
        executable,
        settings_job: false,
        accept_pause: None,
        lock_retry: None,
        lock_failures: 0,
        focus_before_picker: None,
    };
    server.bindings();
    let result = server.serve();
    server.cancel_capture(Error::Canceled);
    server.control.lock();
    let _ = server.worker.send(Job::Lock {
        epoch: server.control.state.session,
    });
    server.worker.finish();
    result
}
struct Listener {
    socket: PathBuf,
    inode: u64,
    _lock: File,
    listener: UnixListener,
}
impl Listener {
    fn bind(paths: &Paths) -> Result<Self, Error> {
        if !paths.runtime.is_absolute() || !paths.vault.is_absolute() {
            return Err(Error::UnsafePath);
        }
        // XDG runtime's parent must already exist and satisfy its own private boundary.
        if paths
            .runtime
            .file_name()
            .is_some_and(|name| name == "snippy")
        {
            ipc::private(paths.runtime.parent().ok_or(Error::UnsafePath)?, true)?;
        }
        ipc::make_private(&paths.runtime)?;
        let lock = ipc::process_lock(&paths.runtime.join("service.lock"))?;
        let socket = paths.socket();
        match fs::symlink_metadata(&socket) {
            Ok(_) => {
                let m = ipc::private(&socket, false)?;
                if !m.file_type().is_socket() {
                    return Err(Error::UnsafePath);
                }
                match UnixStream::connect(&socket) {
                    Ok(_) => return Err(Error::AlreadyRunning),
                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                        fs::remove_file(&socket)?
                    }
                    Err(_) => return Err(Error::Io),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err(Error::Io),
        }
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let inode = fs::symlink_metadata(&socket)?.ino();
        Ok(Self {
            socket,
            inode,
            _lock: lock,
            listener,
        })
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.socket).is_ok_and(|m| m.ino() == self.inode) {
            let _ = fs::remove_file(&self.socket);
        }
    }
}
struct Signals {
    read: UnixStream,
    stop: Arc<AtomicBool>,
    ids: Vec<signal_hook::SigId>,
}
impl Signals {
    fn new() -> Result<Self, Error> {
        let (read, write) = UnixStream::pair()?;
        read.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut ids = Vec::new();
        for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            ids.push(signal_hook::flag::register(signal, stop.clone())?);
            ids.push(signal_hook::low_level::pipe::register(
                signal,
                write.try_clone()?,
            )?);
        }
        ids.push(signal_hook::low_level::pipe::register(
            signal_hook::consts::SIGCHLD,
            write,
        )?);
        Ok(Self { read, stop, ids })
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}
struct Pending {
    client: u64,
    id: u64,
    epoch: u64,
}
struct Server {
    listener: Option<Listener>,
    control: Lifecycle,
    worker: Worker,
    signals: Signals,
    observer: Option<Observer>,
    clients: Vec<Client>,
    pending: Option<Pending>,
    lock_waiters: Vec<(u64, u64)>,
    closing: bool,
    child: Option<Child>,
    reopen: bool,
    next_client: u64,
    capture: Option<capture::Capture>,
    transfer: Option<(u64, crate::x11::capture::Transfer)>,
    next_capture: u64,
    limits: snippy_core::config::CaptureLimits,
    /// Runtime configuration: `overrides` applied to the saved `base`.
    config: AppConfig,
    base: AppConfig,
    overrides: Overrides,
    settings: PathBuf,
    autostart: crate::autostart::Autostart,
    executable: PathBuf,
    /// A settings job is queued; it is applied when the worker reports it,
    /// even if its client disconnected or the session ended meanwhile.
    settings_job: bool,
    /// Transient accept() failure (for example EMFILE): stop polling the listener briefly.
    accept_pause: Option<Instant>,
    /// Back off after a failed vault close instead of retrying immediately.
    lock_retry: Option<Instant>,
    lock_failures: u32,
    focus_before_picker: Option<u32>,
}
impl Server {
    fn bindings(&mut self) {
        if let Some(observer) = &self.observer {
            for binding in observer.bindings() {
                let code = match binding.state {
                    BindingState::Disabled => 0,
                    BindingState::Bound => 1,
                    _ => 2,
                };
                match binding.action {
                    Action::Save => self.control.state.save_binding = code,
                    Action::Picker => self.control.state.picker_binding = code,
                }
            }
        }
    }
    fn broadcast(&mut self) {
        let state = self.control.state;
        for c in &mut self.clients {
            if c.watch && !c.closed {
                c.queue(0, Reply::State(state));
            }
        }
    }
    fn history_changed(&mut self) {
        self.control.state.history_revision = self.control.state.history_revision.saturating_add(1);
        self.control.changed();
        self.broadcast();
    }
    /// The manager edits saved preferences, so it sees the base, not one-run overrides.
    fn config_view(&self, session: u64) -> ipc::ConfigView {
        let base = &self.base;
        ipc::ConfigView {
            session,
            enabled: base.shortcuts.enabled,
            autostart: base.autostart,
            notifications: base.notifications,
            save: base.shortcuts.save.clone(),
            picker: base.shortcuts.picker.clone(),
            max_clip_bytes: base.limits.max_clip_bytes,
            max_vault_bytes: base.limits.max_vault_bytes,
            lock_timeout_seconds: base.lock_timeout.as_secs(),
        }
    }
    /// Apply an already persisted configuration in full. A failed shortcut
    /// rebind is reported through the binding status, never as a partial apply.
    fn apply_config(&mut self, base: AppConfig, runtime: AppConfig) {
        self.control.set_timeout(runtime.lock_timeout);
        self.limits = runtime.limits.clone();
        if let Some(observer) = &mut self.observer
            && observer.set_shortcuts(&runtime.shortcuts).is_err()
        {
            self.control.state.save_binding = 2;
            self.control.state.picker_binding = 2;
        } else {
            self.bindings();
        }
        self.base = base;
        self.config = runtime;
        self.control.changed();
        self.broadcast();
    }
    fn config_done(&mut self, client: u64, id: u64, epoch: u64, result: Result<WorkResult, Error>) {
        self.settings_job = false;
        let current = epoch == self.control.state.session
            && self.control.state.phase == ipc::VaultState::Unlocked;
        let reply = match result {
            // Persisted: always apply, even if the requester is gone.
            Ok(WorkResult::Configured { base, runtime }) => {
                self.apply_config(*base, *runtime);
                if current {
                    Reply::Config(self.config_view(epoch))
                } else {
                    Reply::Error(Error::Stale)
                }
            }
            Err(_) if !current => Reply::Error(Error::Stale),
            Err(error) => Reply::Error(error),
            Ok(_) => Reply::Error(Error::Protocol),
        };
        if self
            .clients
            .iter()
            .any(|c| c.id == client && c.request_id == id && c.pending && !c.closed)
        {
            self.reply(client, id, reply);
        }
    }
    fn reply(&mut self, client: u64, id: u64, reply: Reply) {
        if let Some(c) = self
            .clients
            .iter_mut()
            .find(|c| c.id == client && !c.closed)
        {
            c.pending = false;
            c.queue(id, reply)
        }
    }
    fn request_lock(&mut self) -> Result<(), Error> {
        self.cancel_capture(Error::Canceled);
        self.control.lock();
        self.focus_before_picker = None;
        if let Some(c) = &mut self.capture {
            c.clear_sources();
        }
        for client in &mut self.clients {
            client.clear_private();
        }
        if let Some(p) = self.pending.take() {
            self.reply(p.client, p.id, Reply::Error(Error::Canceled));
        }
        if !self.control.storage_locked
            && !self.closing
            && self.lock_retry.is_none_or(|retry| retry <= Instant::now())
        {
            match self.worker.send(Job::Lock {
                epoch: self.control.state.session,
            }) {
                Ok(()) => self.closing = true,
                Err(Error::Busy) => (), // Retry once queued jobs deliver completions.
                Err(error) => return Err(error),
            }
        }
        self.broadcast();
        Ok(())
    }
    fn open_gui(&mut self) -> Result<(), Error> {
        if let Some(c) = self.clients.iter_mut().find(|c| c.watch && !c.closed) {
            self.reopen = false;
            c.queue(0, Reply::Present);
            return Ok(());
        }
        if self
            .child
            .as_mut()
            .is_some_and(|child| child.try_wait().is_ok_and(|status| status.is_none()))
        {
            self.reopen = true;
            return Ok(());
        }
        self.reopen = false;
        let binary = std::env::current_exe()?
            .parent()
            .ok_or(Error::Gui)?
            .join("snippy-gui");
        self.child = Some(
            Command::new(binary)
                .env("GDK_BACKEND", "x11")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .map_err(|_| Error::Gui)?,
        );
        Ok(())
    }
    fn open_picker(&mut self) -> Result<(), Error> {
        if self.control.state.gui_mode != 1 || !self.clients.iter().any(|c| c.watch && !c.closed) {
            self.focus_before_picker = self.observer.as_ref().and_then(|o| o.focused_window());
        }
        if let Some(gui) = self.clients.iter().find(|c| c.watch && !c.closed)
            && let (Some(observer), Some(pid)) = (&self.observer, gui.pid)
        {
            observer.position_gui(pid);
        }
        self.control.state.gui_mode = 1;
        self.control.changed();
        self.broadcast();
        self.open_gui()
    }
    fn read_work(&mut self, index: usize, session: u64, work: Work) {
        let client = self.clients[index].id;
        let id = self.clients[index].request_id;
        let key = match work {
            // Remember which clipboard owner the user replaced when choosing to copy.
            Work::Copy(_) => WorkKey::Copy(
                client,
                id,
                self.observer
                    .as_ref()
                    .map_or(0, |o| o.snapshot().generation),
            ),
            _ => WorkKey::Read(client, id),
        };
        let result = self.control.touch(session, Instant::now()).and_then(|()| {
            self.worker.send(Job::Work {
                epoch: session,
                key,
                work,
            })
        });
        if let Err(error) = result {
            self.reply(client, id, Reply::Error(error));
        } else {
            self.clients[index].pending = true;
        }
    }
    /// The clipboard owner changed since Copy was requested, and not because
    /// this service served another item: do not replace the user's newer copy.
    fn copy_superseded(&mut self, generation: u64) -> Result<bool, Error> {
        if let Some(observer) = &self.observer {
            observer.synchronize().map_err(|_| Error::X11)?;
        }
        // Drain ownership notifications that precede the fence (bounded).
        for _ in 0..16 {
            if !self.x11_events()? {
                break;
            }
        }
        Ok(self
            .observer
            .as_ref()
            .is_some_and(|o| o.snapshot().generation != generation && !o.serving_current()))
    }
    fn read_done(
        &mut self,
        client: u64,
        id: u64,
        epoch: u64,
        result: Result<WorkResult, Error>,
        copy_generation: Option<u64>,
    ) {
        let current = epoch == self.control.state.session
            && self.control.state.phase == ipc::VaultState::Unlocked;
        // A mutation the worker committed still changed history even if the
        // session ended before its reply could be delivered.
        let mutated = match &result {
            Ok(WorkResult::Tags(_, _)) => true,
            Ok(WorkResult::Deleted(count)) => *count > 0,
            _ => false,
        };
        if mutated && !current {
            self.history_changed();
        }
        if !self
            .clients
            .iter()
            .any(|c| c.id == client && c.request_id == id && c.pending && !c.closed)
        {
            if mutated && current {
                self.history_changed();
            }
            return;
        }
        let reply = if !current {
            Reply::Error(Error::Stale)
        } else {
            match result {
                Ok(WorkResult::Page(page)) => Reply::Page(page),
                Ok(WorkResult::Image(image)) => Reply::Image(image),
                Ok(WorkResult::Content(content)) => {
                    let superseded = match copy_generation {
                        Some(generation) => self.copy_superseded(generation),
                        None => Ok(false),
                    };
                    match superseded.and_then(|superseded| {
                        if superseded {
                            return Err(Error::SelectionChanged);
                        }
                        // The fence may process events that end this session.
                        if epoch != self.control.state.session
                            || self.control.state.phase != ipc::VaultState::Unlocked
                        {
                            return Err(Error::Stale);
                        }
                        self.observer
                            .as_mut()
                            .ok_or(Error::X11)
                            .and_then(|o| o.serve(content).map_err(|_| Error::X11))
                    }) {
                        Ok(()) => {
                            if let (Some(o), Some(window)) =
                                (&self.observer, self.focus_before_picker.take())
                            {
                                o.restore_focus(window);
                            }
                            crate::notifier::copied(self.config.notifications);
                            Reply::State(self.control.state)
                        }
                        Err(error) => Reply::Error(error),
                    }
                }
                Ok(WorkResult::SensitiveApps(apps)) => Reply::SensitiveApps(apps),
                Ok(WorkResult::Tags(id, tags)) => {
                    self.history_changed();
                    Reply::Tags { id, tags }
                }
                Ok(WorkResult::Deleted(count)) => {
                    if count > 0 {
                        self.history_changed();
                    }
                    Reply::Deleted { count }
                }
                Ok(WorkResult::ManagerInfo(info)) => Reply::ManagerInfo(info),
                Err(error) => Reply::Error(error),
                _ => Reply::Error(Error::Protocol),
            }
        };
        self.reply(client, id, reply);
    }
    fn handle(&mut self, index: usize, request: Request) -> Result<(), Error> {
        let client = self.clients[index].id;
        let id = self.clients[index].request_id;
        match request {
            Request::Status => self.reply(client, id, Reply::State(self.control.state)),
            Request::Watch => {
                if let Some(existing) = self.clients.iter_mut().find(|c| c.watch && !c.closed) {
                    existing.queue(0, Reply::Present);
                    self.reply(client, id, Reply::Error(Error::AlreadyOpen));
                } else {
                    self.reopen = false;
                    self.clients[index].watch = true;
                    self.clients[index].deadline = None;
                    self.reply(client, id, Reply::State(self.control.state));
                    self.send_prompt();
                    if self.control.state.gui_mode == 1
                        && let (Some(observer), Some(pid)) =
                            (&self.observer, self.clients[index].pid)
                    {
                        observer.position_gui(pid);
                    }
                }
            }
            Request::Save => {
                if let Err(error) = self.save(Some((client, id))) {
                    self.reply(client, id, Reply::Error(error));
                }
            }
            Request::Confirm {
                session,
                capture,
                label,
            } => {
                let result = self.confirm_capture(session, capture, &label);
                self.reply(
                    client,
                    id,
                    result
                        .map(|()| Reply::State(self.control.state))
                        .unwrap_or_else(Reply::Error),
                );
            }
            Request::CancelSave { capture } => {
                if self.control.state.capture == capture && capture != 0 {
                    self.cancel_capture(Error::Canceled);
                    self.reply(client, id, Reply::State(self.control.state));
                } else {
                    self.reply(client, id, Reply::Error(Error::Stale));
                }
            }
            Request::Verify { session } => {
                let result = self.control.touch(session, Instant::now()).and_then(|()| {
                    self.worker.send(Job::Work {
                        epoch: session,
                        key: WorkKey::Verify(client, id),
                        work: Work::Verify,
                    })
                });
                if let Err(error) = result {
                    self.reply(client, id, Reply::Error(error));
                } else {
                    self.clients[index].pending = true;
                }
            }
            Request::Browse { session, query } => {
                self.read_work(index, session, Work::Browse(query))
            }
            Request::Preview { session, id } => self.read_work(index, session, Work::Preview(id)),
            Request::Copy { session, id } => {
                if self.observer.is_none() {
                    self.reply(
                        client,
                        self.clients[index].request_id,
                        Reply::Error(Error::X11),
                    );
                } else {
                    self.read_work(index, session, Work::Copy(id));
                }
            }
            Request::Config { session } => {
                let reply = match self.control.touch(session, Instant::now()) {
                    Ok(()) => Reply::Config(self.config_view(session)),
                    Err(error) => Reply::Error(error),
                };
                self.reply(client, id, reply);
            }
            Request::SetConfig {
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
                if self.settings_job {
                    self.reply(client, id, Reply::Error(Error::Busy));
                    return Ok(());
                }
                // Edit the saved base; one-run overrides still win at runtime.
                let mut base = self.base.clone();
                base.shortcuts.enabled = enabled;
                base.autostart = autostart;
                base.notifications = notifications;
                base.shortcuts.save = save;
                base.shortcuts.picker = picker;
                base.limits.max_clip_bytes = max_clip_bytes;
                base.limits.max_vault_bytes = max_vault_bytes;
                base.lock_timeout = Duration::from_secs(lock_timeout_seconds);
                let result = crate::settings::validate(&base)
                    .and_then(|()| self.overrides.apply(&base))
                    .map_err(|_| Error::InvalidMetadata)
                    .and_then(|runtime| {
                        self.control.touch(session, Instant::now())?;
                        self.worker.send(Job::Work {
                            epoch: session,
                            key: WorkKey::Config(client, id),
                            work: Work::SetLimits {
                                previous_autostart: self.base.autostart,
                                base: Box::new(base),
                                runtime: Box::new(runtime),
                                settings: self.settings.clone(),
                                autostart: self.autostart.path().to_owned(),
                                executable: self.executable.clone(),
                            },
                        })
                    });
                if let Err(error) = result {
                    self.reply(client, id, Reply::Error(error));
                } else {
                    self.clients[index].pending = true;
                    self.settings_job = true;
                }
            }
            Request::SensitiveApps { session } => {
                self.read_work(index, session, Work::SensitiveApps)
            }
            Request::AddSensitive { session, app_id } => {
                self.read_work(index, session, Work::AddSensitive(app_id))
            }
            Request::RemoveSensitive { session, app_id } => {
                self.read_work(index, session, Work::RemoveSensitive(app_id))
            }
            Request::AddTag { session, id, tag } => {
                self.read_work(index, session, Work::AddTag(id, tag))
            }
            Request::RemoveTag { session, id, tag } => {
                self.read_work(index, session, Work::RemoveTag(id, tag))
            }
            Request::Delete { session, id } => self.read_work(index, session, Work::Delete(id)),
            Request::DeleteMany { session, ids } => {
                self.read_work(index, session, Work::DeleteMany(ids))
            }
            Request::ManagerInfo { session } => self.read_work(index, session, Work::ManagerInfo),
            Request::OpenPicker => {
                let result = self.open_picker();
                self.reply(
                    client,
                    id,
                    result
                        .map(|()| Reply::State(self.control.state))
                        .unwrap_or_else(Reply::Error),
                );
            }
            Request::OpenGui => {
                self.control.state.gui_mode = 0;
                self.control.changed();
                self.broadcast();
                let result = self.open_gui();
                self.reply(
                    client,
                    id,
                    result
                        .map(|()| Reply::State(self.control.state))
                        .unwrap_or_else(Reply::Error),
                );
            }
            Request::Lock => {
                self.request_lock()?;
                if self.control.storage_locked {
                    self.reply(client, id, Reply::State(self.control.state))
                } else {
                    self.clients[index].pending = true;
                    self.lock_waiters.push((client, id));
                }
            }
            Request::Activity { session } => {
                let result = self.control.touch(session, Instant::now());
                self.reply(
                    client,
                    id,
                    result
                        .map(|()| Reply::State(self.control.state))
                        .unwrap_or_else(Reply::Error),
                );
            }
            request @ (Request::Create { .. } | Request::Unlock(_)) => match self.control.begin() {
                Err(error) => self.reply(client, id, Reply::Error(error)),
                Ok(epoch) => {
                    if let Err(error) = self.worker.send(Job::Unseal { epoch, request }) {
                        self.control.complete(
                            epoch,
                            Err(error),
                            self.control.state.exists,
                            Instant::now(),
                        );
                        self.reply(client, id, Reply::Error(error));
                    } else {
                        self.clients[index].pending = true;
                        self.pending = Some(Pending { client, id, epoch });
                    }
                    self.broadcast();
                }
            },
        }
        Ok(())
    }
    fn completions(&mut self) -> Result<(), Error> {
        drain(&mut self.worker.wake);
        loop {
            let mut done = match self.worker.results.try_recv() {
                Ok(done) => done,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Err(Error::Storage),
            };
            if let Some((key, result)) = done.work.take() {
                match key {
                    WorkKey::Read(client, id) => {
                        self.read_done(client, id, done.epoch, result, None)
                    }
                    WorkKey::Copy(client, id, generation) => {
                        self.read_done(client, id, done.epoch, result, Some(generation))
                    }
                    WorkKey::Capture(id) => self.capture_done(id, done.epoch, result)?,
                    WorkKey::Verify(client, id) => {
                        let reply = if done.epoch != self.control.state.session
                            || self.control.state.phase != ipc::VaultState::Unlocked
                        {
                            Reply::Error(Error::Stale)
                        } else {
                            match result {
                                Ok(WorkResult::Verification(v)) => Reply::Verification(v),
                                Err(e) => Reply::Error(e),
                                _ => Reply::Error(Error::Protocol),
                            }
                        };
                        self.reply(client, id, reply);
                    }
                    WorkKey::Config(client, id) => self.config_done(client, id, done.epoch, result),
                }
                if self.control.state.phase == ipc::VaultState::Locked
                    && !self.control.storage_locked
                    && !self.closing
                {
                    self.request_lock()?;
                }
                continue;
            }
            if done.lock {
                self.closing = false;
                if done.result.is_ok() {
                    self.control.acknowledge_lock(done.epoch);
                    self.lock_failures = 0;
                    self.lock_retry = None;
                } else {
                    // The session stays invalidated; retry the close with backoff.
                    self.lock_failures = self.lock_failures.saturating_add(1);
                    self.lock_retry = Some(Instant::now() + lock_backoff(self.lock_failures));
                }
                let waiters = std::mem::take(&mut self.lock_waiters);
                for (client, id) in waiters {
                    self.reply(
                        client,
                        id,
                        done.result
                            .map(|()| Reply::State(self.control.state))
                            .unwrap_or(Reply::Error(Error::Storage)),
                    );
                }
            } else {
                let accepted =
                    self.control
                        .complete(done.epoch, done.result, done.exists, Instant::now());
                if accepted
                    && let Some(pending) = self.pending.take()
                    && pending.epoch == done.epoch
                {
                    self.reply(
                        pending.client,
                        pending.id,
                        done.result
                            .map(|()| Reply::State(self.control.state))
                            .unwrap_or_else(Reply::Error),
                    );
                }
            }
            if self.control.state.phase == ipc::VaultState::Locked
                && !self.control.storage_locked
                && !self.closing
            {
                self.request_lock()?;
            }
            self.broadcast();
            if self.control.state.phase == ipc::VaultState::Unlocked
                && self.control.state.capture_stage == 1
            {
                self.check_capture()?;
            }
        }
        Ok(())
    }
    fn x11_events(&mut self) -> Result<bool, Error> {
        for _ in 0..64 {
            let event = match &mut self.observer {
                Some(o) => o.poll().map_err(|_| Error::X11)?,
                None => None,
            };
            match event {
                Some(Update::Bindings) => {
                    self.bindings();
                    self.control.changed();
                    self.broadcast();
                }
                Some(Update::Shortcut(Action::Save)) => {
                    if let Err(error) = self.save(None) {
                        self.control.state.save_outcome = error as u8;
                        self.control.changed();
                        self.broadcast();
                        let _ = self.open_gui();
                    }
                }
                Some(Update::Shortcut(Action::Picker)) => {
                    let _ = self.open_picker();
                }
                Some(Update::Ownership) => self.maintain_capture(),
                Some(_) => (),
                None => return Ok(false),
            }
        }
        Ok(true)
    }
    fn serve(&mut self) -> Result<(), Error> {
        loop {
            if self.signals.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if self.control.expired(Instant::now())
                || (self.lock_retry.is_some_and(|retry| retry <= Instant::now())
                    && !self.control.storage_locked
                    && !self.closing)
            {
                self.request_lock()?;
            }
            if self
                .accept_pause
                .is_some_and(|until| until <= Instant::now())
            {
                self.accept_pause = None;
            }
            if let Some(observer) = &mut self.observer {
                observer.expire_serving();
            }
            let mut drain_again = self.x11_events()?;
            self.maintain_capture();
            let now = Instant::now();
            for c in &mut self.clients {
                if c.deadline.is_some_and(|d| d <= now) {
                    c.closed = true
                }
            }
            if self.pending.as_ref().is_some_and(|p| {
                !self
                    .clients
                    .iter()
                    .any(|c| c.id == p.client && !c.closed && c.pending)
            }) {
                self.request_lock()?;
            }
            self.clients.retain(|c| !c.closed);
            self.lock_waiters
                .retain(|(id, _)| self.clients.iter().any(|c| c.id == *id));
            if self.reopen && self.child.is_none() {
                let _ = self.open_gui();
            }
            let deadline = self
                .clients
                .iter()
                .filter_map(|c| c.deadline)
                .chain(self.control.deadline())
                .chain(self.capture_deadline())
                .chain(self.observer.as_ref().and_then(|o| o.serving_deadline()))
                .chain(self.accept_pause)
                .chain(
                    self.lock_retry
                        .filter(|_| !self.control.storage_locked && !self.closing),
                )
                .min();
            let listening = self.listener.is_some() && self.accept_pause.is_none();
            let timeout = if drain_again {
                Some(Duration::ZERO)
            } else {
                deadline.map(|d| d.saturating_duration_since(Instant::now()))
            };
            let timeout = timeout.map(|d| Timespec {
                tv_sec: d.as_secs().min(i64::MAX as u64) as i64,
                tv_nsec: d.subsec_nanos() as i64,
            });
            let mut fds = vec![
                PollFd::from_borrowed_fd(
                    self.listener
                        .as_ref()
                        .filter(|_| listening)
                        .map(|l| std::os::fd::AsFd::as_fd(&l.listener))
                        .unwrap_or_else(|| std::os::fd::AsFd::as_fd(&self.worker.wake)),
                    PollFlags::IN,
                ),
                PollFd::new(&self.worker.wake, PollFlags::IN),
                PollFd::new(&self.signals.read, PollFlags::IN),
            ];
            if let Some(observer) = &self.observer {
                fds.push(PollFd::from_borrowed_fd(observer.event_fd(), PollFlags::IN));
            }
            let transfer_index = if let Some((_, transfer)) = &self.transfer {
                let index = fds.len();
                fds.push(PollFd::new(&transfer.wake, PollFlags::IN));
                Some(index)
            } else {
                None
            };
            let base = fds.len();
            for c in &self.clients {
                fds.push(PollFd::new(
                    &c.stream,
                    PollFlags::IN
                        | if c.wants_write() {
                            PollFlags::OUT
                        } else {
                            PollFlags::empty()
                        },
                ));
            }
            match poll(&mut fds, timeout.as_ref()) {
                Ok(_) => (),
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => return Err(Error::Io),
            }
            let flags = fds.iter().map(PollFd::revents).collect::<Vec<_>>();
            drop(fds);
            if !flags[2].is_empty() {
                drain(&mut self.signals.read);
                // A reaping error must not stop clipboard serving; forget the child.
                if let Some(child) = &mut self.child
                    && !child.try_wait().is_ok_and(|status| status.is_none())
                {
                    self.child = None;
                }
            }
            drain_again |= self.x11_events()?;
            // Invalidate disconnected/canceled work before accepting a worker's reply.
            for i in 0..self.clients.len() {
                let f = flags[base + i];
                if f.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
                    match self.clients[i].read() {
                        Ok(Some(request)) => self.handle(i, request)?,
                        Ok(None) => (),
                        Err(error) => {
                            let id = self.clients[i].request_id;
                            self.clients[i].watch = false;
                            self.clients[i].queue(id, Reply::Error(error));
                            self.clients[i].pending = false;
                        }
                    }
                }
                if f.contains(PollFlags::OUT) {
                    self.clients[i].flush();
                }
            }
            if self.pending.as_ref().is_some_and(|p| {
                !self
                    .clients
                    .iter()
                    .any(|c| c.id == p.client && !c.closed && c.pending)
            }) {
                self.request_lock()?;
            }
            self.maintain_capture();
            // Buffered X11 events are drained before transfer/worker results;
            // their wakeups stay readable and are handled on the next pass.
            if !drain_again {
                if transfer_index.is_some_and(|i| !flags[i].is_empty()) {
                    self.transfer_complete()?;
                }
                if !flags[1].is_empty() {
                    self.completions()?;
                }
            }
            if listening
                && flags[0].contains(PollFlags::IN)
                && let Some(listener) = &self.listener
            {
                let mut attempts = 0;
                while attempts < 16 {
                    attempts += 1;
                    match listener.listener.accept() {
                        Ok((stream, _)) => {
                            if self.clients.len() < 8
                                && ipc::verify_peer(&stream).is_ok()
                                && let Ok(client) = Client::new(self.next_client, stream)
                            {
                                self.clients.push(client);
                                self.next_client = self.next_client.saturating_add(1);
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            if let Some(pause) = accept_error(&e) {
                                self.accept_pause = Some(Instant::now() + pause);
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
}
/// 0.5 s doubling to a 30 s ceiling between attempts to close a vault that failed to close.
fn lock_backoff(failures: u32) -> Duration {
    Duration::from_millis(500)
        .saturating_mul(1 << failures.saturating_sub(1).min(6))
        .min(Duration::from_secs(30))
}
/// accept() failures never stop the service. Resource exhaustion pauses the
/// listener briefly instead of spinning; peers that vanished are skipped.
fn accept_error(error: &std::io::Error) -> Option<Duration> {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::Interrupted | ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset => None,
        _ => Some(Duration::from_millis(100)),
    }
}
fn drain(stream: &mut UnixStream) {
    let mut bytes = [0; 64];
    loop {
        match stream.read(&mut bytes) {
            Ok(0) | Err(_) => break,
            Ok(_) => (),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "sqlcipher")]
    #[test]
    #[ignore = "requires native Unix socket I/O; use scripts/check-service.sh"]
    fn real_socketpair_control_round_trip_lock_wrong_password_and_idle_timeout() {
        use std::{io::Write, thread};
        use zeroize::Zeroizing;
        let dir = tempfile::tempdir().unwrap();
        let vault = Vault::at(
            dir.path().join("vault"),
            snippy_core::config::CaptureLimits::default(),
        )
        .unwrap();
        let control = Lifecycle::new(false, Duration::from_secs(1));
        let worker = Worker::start(vault, control.epoch.clone()).unwrap();
        let (read, mut signal) = UnixStream::pair().unwrap();
        read.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let mut peers = Vec::new();
        let mut clients = Vec::new();
        // Preconnected fixture endpoints bypass listener authentication only here.
        // Native tests exercise real SO_PEERCRED on filesystem-backed sockets.
        for id in 1..=8 {
            let (user, server) = UnixStream::pair().unwrap();
            user.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut client = Client::new(id, server).unwrap();
            client.deadline = None;
            clients.push(client);
            peers.push(user);
        }
        let shutdown = stop.clone();
        let join = thread::spawn(move || {
            let mut server = Server {
                listener: None,
                control,
                worker,
                signals: Signals {
                    read,
                    stop,
                    ids: Vec::new(),
                },
                observer: None,
                clients,
                pending: None,
                lock_waiters: Vec::new(),
                closing: false,
                child: None,
                reopen: false,
                next_client: 9,
                capture: None,
                transfer: None,
                next_capture: 0,
                limits: Default::default(),
                config: Default::default(),
                base: Default::default(),
                overrides: Default::default(),
                settings: PathBuf::from("/nonexistent/snippy-settings"),
                autostart: crate::autostart::Autostart::at("/nonexistent/autostart"),
                executable: PathBuf::from("/nonexistent/snippy"),
                settings_job: false,
                accept_pause: None,
                lock_retry: None,
                lock_failures: 0,
                focus_before_picker: None,
            };
            let result = server.serve();
            server.control.lock();
            server.worker.finish();
            result
        });
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let secret = || Zeroizing::new("synthetic-actor-key".into());
            assert_eq!(
                ipc::exchange(
                    &mut peers[0],
                    Request::Create {
                        passphrase: secret(),
                        confirmation: secret()
                    }
                )
                .unwrap()
                .phase,
                ipc::VaultState::Unlocked
            );
            assert_eq!(
                ipc::exchange(&mut peers[1], Request::Status).unwrap().phase,
                ipc::VaultState::Unlocked
            );
            assert_eq!(
                ipc::exchange(&mut peers[2], Request::Lock).unwrap().phase,
                ipc::VaultState::Locked
            );
            assert_eq!(
                ipc::exchange(
                    &mut peers[3],
                    Request::Unlock(Zeroizing::new("wrong".into()))
                ),
                Err(Error::WrongPassword)
            );
            let state = ipc::exchange(&mut peers[4], Request::Unlock(secret())).unwrap();
            assert_eq!(state.phase, ipc::VaultState::Unlocked);
            assert_eq!(
                ipc::exchange(
                    &mut peers[5],
                    Request::Activity {
                        session: state.session - 1
                    }
                ),
                Err(Error::Stale)
            );
            thread::sleep(Duration::from_millis(1100));
            assert_eq!(
                ipc::exchange(&mut peers[6], Request::Status).unwrap().phase,
                ipc::VaultState::Locked
            );
            peers[7]
                .write_all(&((ipc::MAX_FRAME + 1) as u32).to_be_bytes())
                .unwrap();
            assert!(matches!(
                ipc::read_reply(&mut peers[7]).unwrap().1,
                Reply::Error(Error::Protocol)
            ));
        }));
        shutdown.store(true, Ordering::Relaxed);
        signal.write_all(&[1]).unwrap();
        assert!(join.join().unwrap().is_ok());
        if let Err(panic) = result {
            std::panic::resume_unwind(panic)
        }
    }
    /// A locked, display-less service over an unopened synthetic vault path.
    pub(super) fn idle_server(dir: &std::path::Path) -> Server {
        let vault = Vault::at(
            dir.join("vault"),
            snippy_core::config::CaptureLimits::default(),
        )
        .unwrap();
        let control = Lifecycle::new(false, Duration::from_secs(60));
        let worker = Worker::start(vault, control.epoch.clone()).unwrap();
        let (read, _) = UnixStream::pair().unwrap();
        Server {
            listener: None,
            control,
            worker,
            signals: Signals {
                read,
                stop: Arc::new(AtomicBool::new(false)),
                ids: Vec::new(),
            },
            observer: None,
            clients: Vec::new(),
            pending: None,
            lock_waiters: Vec::new(),
            closing: false,
            child: None,
            reopen: false,
            next_client: 1,
            capture: None,
            transfer: None,
            next_capture: 0,
            limits: Default::default(),
            config: Default::default(),
            base: Default::default(),
            overrides: Default::default(),
            settings: dir.join("config/snippy/settings.conf"),
            autostart: crate::autostart::Autostart::at(dir.join("autostart/snippy.desktop")),
            executable: PathBuf::from("/nonexistent/snippy"),
            settings_job: false,
            accept_pause: None,
            lock_retry: None,
            lock_failures: 0,
            focus_before_picker: None,
        }
    }
    #[test]
    fn persisted_settings_apply_with_overrides_even_after_the_session_ends() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = idle_server(dir.path());
        server.overrides = Overrides::new(Some(30), true, None, None, None, None).unwrap();
        server.config = server.overrides.apply(&server.base).unwrap();
        let mut base = AppConfig {
            notifications: true,
            lock_timeout: Duration::from_secs(600),
            ..AppConfig::default()
        };
        base.limits.max_clip_bytes = 4096;
        let runtime = server.overrides.apply(&base).unwrap();
        server.settings_job = true;
        // The requester (client 7) disconnected and the session changed after
        // the worker wrote the files: the result is still applied.
        server.config_done(
            7,
            1,
            server.control.state.session + 1,
            Ok(WorkResult::Configured {
                base: Box::new(base.clone()),
                runtime: Box::new(runtime),
            }),
        );
        assert!(!server.settings_job);
        assert_eq!(server.base, base);
        assert!(server.config.notifications);
        assert!(
            !server.config.shortcuts.enabled,
            "--no-shortcuts still wins"
        );
        assert_eq!(server.config.lock_timeout, Duration::from_secs(30));
        assert_eq!(server.limits.max_clip_bytes, 4096);
        let view = server.config_view(1);
        assert!(view.enabled, "manager shows the saved preference");
        assert_eq!(view.lock_timeout_seconds, 600);
    }
    #[test]
    fn lock_close_failures_back_off_and_accept_errors_do_not_stop_service() {
        assert_eq!(lock_backoff(1), Duration::from_millis(500));
        assert!(lock_backoff(2) > lock_backoff(1));
        assert_eq!(lock_backoff(u32::MAX), Duration::from_secs(30));
        let error = |kind| std::io::Error::from(kind);
        assert_eq!(accept_error(&error(std::io::ErrorKind::Interrupted)), None);
        assert_eq!(
            accept_error(&error(std::io::ErrorKind::ConnectionAborted)),
            None
        );
        for errno in [rustix::io::Errno::MFILE, rustix::io::Errno::NFILE] {
            let error = std::io::Error::from_raw_os_error(errno.raw_os_error());
            assert!(accept_error(&error).is_some_and(|pause| !pause.is_zero()));
        }
    }
    #[test]
    fn private_lock_is_exclusive_and_other_uids_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let held = ipc::process_lock(&path).unwrap();
        assert_eq!(ipc::process_lock(&path).err(), Some(Error::AlreadyRunning));
        drop(held);
        assert!(ipc::process_lock(&path).is_ok());
        assert_eq!(ipc::check_uid(1001, 1000), Err(Error::Unauthorized));
        assert_eq!(ipc::check_uid(1000, 1000), Ok(()));
    }
}
