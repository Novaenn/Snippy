//! Native end-to-end fixtures, explicitly enabled only by check-capture.sh.
mod support {
    pub mod owner;
}
use snippy::{
    ipc::{self, Error, Paths, Reply, Request, State, VaultState},
    storage::{Passphrase, Vault},
};
use snippy_core::{config::CaptureLimits, source::SourceIdentity};
use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    process::{Child, Command, Stdio},
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};
use support::owner::{Offers, Owner};
use tempfile::TempDir;
use x11rb::{
    connection::Connection,
    protocol::xproto::{ConnectionExt as _, CreateWindowAux, InputFocus, WindowClass},
};
use zeroize::Zeroizing;
const KEY: &str = "synthetic-capture-fixture-key";
struct Service {
    _dir: TempDir,
    paths: Paths,
    child: Child,
    watch: Option<UnixStream>,
}
impl Service {
    fn new(extra: &[&str], unlocked: bool) -> Self {
        assert_eq!(
            std::env::var("SNIPPY_TEST_X11").as_deref(),
            Ok("1"),
            "Use scripts/check-capture.sh on a private display"
        );
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        let data = dir.path().join("data");
        for p in [&runtime, &data] {
            fs::create_dir(p).unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let paths = Paths {
            runtime: runtime.join("snippy"),
            vault: data.join("snippy"),
        };
        let child = Command::new(env!("CARGO_BIN_EXE_snippy"))
            .args(["daemon", "--no-shortcuts", "--lock-timeout-seconds", "60"])
            .args(extra)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_DATA_HOME", &data)
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("XDG_SESSION_TYPE", "x11")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut service = Self {
            _dir: dir,
            paths,
            child,
            watch: None,
        };
        wait(|| ipc::call(&service.paths, Request::Status).is_ok());
        if unlocked {
            service.unlock();
        }
        service.watch = Some(ipc::subscribe(&service.paths).unwrap().0);
        service
            .watch
            .as_ref()
            .unwrap()
            .set_read_timeout(Some(Duration::from_secs(8)))
            .unwrap();
        service
    }
    fn unlock(&self) -> State {
        let s = self.state();
        ipc::call(
            &self.paths,
            if s.exists {
                Request::Unlock(secret())
            } else {
                Request::Create {
                    passphrase: secret(),
                    confirmation: secret(),
                }
            },
        )
        .unwrap()
    }
    fn copy_item(&self, id: i64) -> Result<State, Error> {
        ipc::call(
            &self.paths,
            Request::Copy {
                session: self.state().session,
                id,
            },
        )
    }
    fn read_served(&self) -> snippy::x11::capture::Received {
        let observer = snippy::x11::Observer::connect(&snippy_core::config::ShortcutConfig {
            enabled: false,
            ..Default::default()
        })
        .unwrap();
        let mut transfer = snippy::x11::capture::Transfer::start(
            observer.snapshot().clone(),
            CaptureLimits::default(),
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        let received = transfer
            .result
            .recv_timeout(Duration::from_secs(8))
            .unwrap()
            .unwrap();
        transfer.finish();
        received
    }
    fn state(&self) -> State {
        ipc::call(&self.paths, Request::Status).unwrap()
    }
    fn save(&self) -> Result<Reply, Error> {
        ipc::exchange_reply(&mut self.paths.connect()?, Request::Save)
    }
    fn pending(&self) -> thread::JoinHandle<Result<Reply, Error>> {
        let paths = self.paths.clone();
        thread::spawn(move || ipc::exchange_reply(&mut paths.connect()?, Request::Save))
    }
    fn prompt(&mut self) -> ipc::Prompt {
        loop {
            if let (0, Reply::Prompt(p)) = ipc::read_reply(self.watch.as_mut().unwrap()).unwrap() {
                return p;
            }
        }
    }
    fn verify(&self) -> ipc::Verification {
        let session = self.state().session;
        match ipc::exchange_reply(
            &mut self.paths.connect().unwrap(),
            Request::Verify { session },
        )
        .unwrap()
        {
            Reply::Verification(v) => v,
            _ => panic!("expected verification"),
        }
    }
    fn confirm(&self, p: &ipc::Prompt, label: &str) -> Result<State, Error> {
        ipc::call(
            &self.paths,
            Request::Confirm {
                session: p.session,
                capture: p.capture,
                label: Zeroizing::new(label.into()),
            },
        )
    }
    fn cancel(&self, capture: u64) {
        ipc::call(&self.paths, Request::CancelSave { capture }).unwrap();
    }
    fn request(&self, request: Request) -> Result<Reply, Error> {
        ipc::exchange_reply(&mut self.paths.connect()?, request)
    }
    fn stop(&mut self) {
        self.watch.take();
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
            self.child.wait().unwrap();
        }
    }
    fn vault(&mut self) -> Vault {
        self.stop();
        let mut vault = Vault::at(&self.paths.vault, CaptureLimits::default()).unwrap();
        vault.unlock(&Passphrase::new(KEY.into()).unwrap()).unwrap();
        vault
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.stop();
    }
}
fn secret() -> Zeroizing<String> {
    Zeroizing::new(KEY.into())
}
fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for synthetic fixture");
}
fn offers(class: Option<&str>, text: &[u8]) -> Offers {
    Offers {
        class: class.map(str::to_owned),
        text: text.to_vec(),
        ..Default::default()
    }
}
fn saved(reply: Result<Reply, Error>) -> (i64, bool) {
    match reply.unwrap() {
        Reply::Saved { id, merged } => (id, merged),
        _ => panic!("expected committed save"),
    }
}
fn image() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut e = png::Encoder::new(&mut bytes, 1, 1);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.write_header()
            .unwrap()
            .write_image_data(&[12, 34, 56, 78])
            .unwrap();
    }
    bytes
}

#[test]
#[ignore = "private Xvfb and native sockets required"]
fn normal_copy_is_passive_and_explicit_text_save_retains_source_after_focus_changes() {
    let text = b"  synthetic Unicode: \xc3\xa9\t\n\r\n";
    let owner = Owner::new(offers(Some("SyntheticEditor"), text));
    let mut service = Service::new(&[], true);
    owner.copy_again();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(owner.requests.load(Ordering::SeqCst), 0);
    assert_eq!(service.verify().items, 0);
    // Focus another native window while leaving CLIPBOARD unchanged.
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
    c.map_window(window).unwrap().check().unwrap();
    c.set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME)
        .unwrap()
        .check()
        .unwrap();
    let (id, merged) = saved(service.save());
    assert!(!merged);
    assert!(
        matches!(service.verify().recent.unwrap().source,SourceIdentity::Observed{display_name,..} if display_name=="SyntheticEditor")
    );
    let clipboard = c
        .intern_atom(false, b"CLIPBOARD")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    assert_eq!(
        c.get_selection_owner(clipboard)
            .unwrap()
            .reply()
            .unwrap()
            .owner,
        owner.window,
        "Save must not acquire CLIPBOARD"
    );
    assert_eq!(saved(service.save()), (id, true));
    assert_eq!(service.verify().items, 1);
    let vault = service.vault();
    assert_eq!(vault.get(id).unwrap().payload.bytes(), text);
}
#[test]
#[ignore = "private Xvfb and native sockets required"]
fn png_has_priority_and_incremental_text_round_trips() {
    let owner = Owner::new(Offers {
        png: Some(image()),
        incr: true,
        ..offers(Some("SyntheticImage"), b"synthetic alternate text")
    });
    let mut service = Service::new(&[], true);
    let (image_id, _) = saved(service.save());
    assert_eq!(owner.payloads.load(Ordering::SeqCst), 1);
    let bytes = vec![b'T'; 180_000];
    let text = Owner::new(Offers {
        incr: true,
        ..offers(Some("SyntheticText"), &bytes)
    });
    wait(|| service.state().capture == 0);
    let (text_id, _) = saved(service.save());
    let vault = service.vault();
    assert_eq!(
        vault.get(image_id).unwrap().payload.kind(),
        snippy::storage::ContentKind::Png
    );
    assert_eq!(vault.get(text_id).unwrap().payload.bytes(), bytes);
    drop(text);
}
#[test]
#[ignore = "private Xvfb and native sockets required"]
fn unknown_sensitive_cancel_labels_and_repeated_saves_are_authorized_before_reads() {
    let unknown = Owner::new(offers(None, b"synthetic unknown"));
    let mut service = Service::new(&[], true);
    let a = service.pending();
    let prompt = service.prompt();
    assert!(!prompt.sensitive);
    assert_eq!(unknown.requests.load(Ordering::SeqCst), 0);
    let b = service.pending();
    wait(|| service.state().capture == prompt.capture);
    thread::sleep(Duration::from_millis(50));
    service.confirm(&prompt, "Manual fixture").unwrap();
    let first = saved(a.join().unwrap());
    assert_eq!(saved(b.join().unwrap()), first);
    assert!(
        matches!(service.verify().recent.unwrap().source,SourceIdentity::Manual{label} if label=="Manual fixture")
    );
    let sensitive = Owner::new(offers(Some("keepassxc"), b"synthetic not-a-secret"));
    let attempt = service.pending();
    let prompt = service.prompt();
    assert!(prompt.sensitive);
    assert_eq!(sensitive.requests.load(Ordering::SeqCst), 0);
    assert_eq!(
        service.confirm(&prompt, "bypass").err(),
        Some(Error::InvalidLabel)
    );
    service.cancel(prompt.capture);
    assert!(matches!(attempt.join().unwrap(), Err(Error::Canceled)));
    assert_eq!(service.verify().items, 1);
    let attempt = service.pending();
    let prompt = service.prompt();
    service.confirm(&prompt, "").unwrap();
    saved(attempt.join().unwrap());
    assert_eq!(service.verify().items, 2);
    let vault = service.vault();
    assert_eq!(vault.list(100, 0).unwrap().len(), 2);
}
#[test]
#[ignore = "private Xvfb and native sockets required"]
fn selection_changes_during_unlock_confirmation_and_owner_exit_cancel_without_reads() {
    let owner = Owner::new(offers(None, b"synthetic pending"));
    let mut service = Service::new(&[], false);
    let attempt = service.pending();
    wait(|| service.state().capture_stage == 1);
    assert_eq!(owner.requests.load(Ordering::SeqCst), 0);
    owner.copy_again();
    assert!(matches!(
        attempt.join().unwrap(),
        Err(Error::SelectionChanged)
    ));
    service.unlock();
    assert_eq!(service.verify().items, 0);
    let attempt = service.pending();
    let prompt = service.prompt();
    owner.copy_again();
    assert!(matches!(
        attempt.join().unwrap(),
        Err(Error::SelectionChanged)
    ));
    assert_eq!(service.confirm(&prompt, "").err(), Some(Error::Stale));
    let attempt = service.pending();
    service.prompt();
    drop(owner);
    assert!(matches!(
        attempt.join().unwrap(),
        Err(Error::SelectionChanged)
    ));
    assert_eq!(service.verify().items, 0);
}
#[test]
#[ignore = "private Xvfb and native sockets required"]
fn lock_and_confirmation_disconnect_cancel_pending_transfers() {
    let owner = Owner::new(offers(None, b"synthetic confirmation"));
    let mut service = Service::new(&[], true);
    let attempt = service.pending();
    service.prompt();
    service.watch.take();
    assert!(matches!(attempt.join().unwrap(), Err(Error::Canceled)));
    assert_eq!(service.verify().items, 0);
    let hung = Owner::new(Offers {
        hung: true,
        ..offers(Some("SyntheticHung"), b"synthetic")
    });
    let attempt = service.pending();
    wait(|| hung.requests.load(Ordering::SeqCst) > 0);
    ipc::call(&service.paths, Request::Lock).unwrap();
    assert!(matches!(attempt.join().unwrap(), Err(Error::Canceled)));
    service.unlock();
    assert_eq!(service.verify().items, 0);
    drop(owner);
}
#[test]
#[ignore = "private Xvfb and native sockets required"]
fn invalid_empty_unsupported_oversized_and_quota_saves_leave_no_partial_items() {
    let first = Owner::new(offers(Some("SyntheticLimit"), b"four"));
    let service = Service::new(&["--max-clip-bytes", "8", "--max-vault-bytes", "4"], true);
    let (id, _) = saved(service.save());
    for (offer, error) in [
        (offers(Some("SyntheticLimit"), b"12345"), Error::Quota),
        (
            offers(Some("SyntheticLimit"), b"123456789"),
            Error::TooLarge,
        ),
        (
            Offers {
                incr: true,
                oversized_hint: true,
                ..offers(Some("SyntheticLimit"), b"x")
            },
            Error::TooLarge,
        ),
        (offers(Some("SyntheticLimit"), b""), Error::InvalidContent),
        (
            Offers {
                unsupported: true,
                ..offers(Some("SyntheticLimit"), b"x")
            },
            Error::Unsupported,
        ),
        (
            Offers {
                png: Some(b"bad".to_vec()),
                ..offers(Some("SyntheticLimit"), b"x")
            },
            Error::InvalidContent,
        ),
    ] {
        let owner = Owner::new(offer);
        assert!(matches!(service.save(),Err(e) if e==error));
        assert_eq!(service.verify().items, 1);
        drop(owner);
    }
    first.copy_again();
    assert_eq!(saved(service.save()), (id, true));
    assert_eq!(service.verify().logical_bytes, 4);
}
#[test]
#[ignore = "private Xvfb and native sockets required"]
fn hung_and_malformed_incremental_transfers_are_rejected() {
    let service = Service::new(&[], true);
    for (offer, error) in [
        (
            Offers {
                hung: true,
                ..offers(Some("SyntheticHung"), b"x")
            },
            Error::TransferTimeout,
        ),
        (
            Offers {
                incr: true,
                bad_incr_type: true,
                ..offers(Some("SyntheticBadIncr"), &vec![b'X'; 8192])
            },
            Error::InvalidContent,
        ),
        (
            Offers {
                incr: true,
                truncated: true,
                ..offers(Some("SyntheticTruncated"), &vec![b'X'; 8192])
            },
            Error::TransferTimeout,
        ),
    ] {
        let owner = Owner::new(offer);
        assert!(matches!(service.save(),Err(e) if e==error));
        assert_eq!(service.verify().items, 0);
        drop(owner);
    }
    assert_eq!(service.state().phase, VaultState::Unlocked);
}

#[test]
#[ignore = "private Xvfb and native sockets required"]
fn reuse_text_survives_gui_disconnect_and_lock_without_history_changes() {
    let text = b"  synthetic reusable text: \xc3\xa9\n";
    let source = Owner::new(offers(Some("OriginalEditor"), text));
    let mut service = Service::new(&[], true);
    let (id, _) = saved(service.save());
    let before = service.verify().recent.unwrap();
    service.copy_item(id).unwrap();
    let content = service.read_served();
    assert!(!content.png);
    assert_eq!(&*content.bytes, text);
    service.watch.take();
    ipc::call(&service.paths, Request::Lock).unwrap();
    assert_eq!(&*service.read_served().bytes, text);
    assert!(matches!(service.save(), Ok(Reply::AlreadySaved)));
    assert_eq!(service.state().phase, VaultState::Locked);
    service.unlock();
    let after = service.verify();
    assert_eq!(after.items, 1);
    let latest = after.recent.unwrap();
    assert!(latest.source == before.source);
    assert_eq!(latest.last_saved_ms, before.last_saved_ms);
    source.copy_again();
    thread::sleep(Duration::from_millis(50));
    assert_eq!(&*service.read_served().bytes, text);
    assert_eq!(service.verify().items, 1);
    assert_eq!(saved(service.save()), (id, true));
}
#[test]
#[ignore = "private Xvfb and native sockets required"]
fn reuse_png_and_large_text_support_incremental_receivers_and_locked_requests_fail() {
    let png = Owner::new(Offers {
        png: Some(image()),
        ..offers(Some("OriginalImage"), b"alternative")
    });
    let service = Service::new(&[], true);
    let (image_id, _) = saved(service.save());
    service.copy_item(image_id).unwrap();
    assert!(service.read_served().png);
    let text = vec![b'T'; 180_000];
    let owner = Owner::new(Offers {
        incr: true,
        ..offers(Some("OriginalText"), &text)
    });
    let (text_id, _) = saved(service.save());
    service.copy_item(text_id).unwrap();
    assert_eq!(&*service.read_served().bytes, &text);
    let stale = service.state().session;
    ipc::call(&service.paths, Request::Lock).unwrap();
    assert_eq!(&*service.read_served().bytes, &text);
    assert_eq!(
        ipc::exchange_reply(
            &mut service.paths.connect().unwrap(),
            Request::Browse {
                session: stale,
                query: Default::default()
            }
        )
        .err(),
        Some(Error::Stale)
    );
    assert_eq!(
        ipc::exchange_reply(
            &mut service.paths.connect().unwrap(),
            Request::Preview {
                session: stale,
                id: image_id
            }
        )
        .err(),
        Some(Error::Stale)
    );
    assert_eq!(
        ipc::call(
            &service.paths,
            Request::Copy {
                session: stale,
                id: text_id
            }
        )
        .err(),
        Some(Error::Stale)
    );
    drop((png, owner));
}

#[test]
#[ignore = "private Xvfb and native sockets required; use scripts/check-capture.sh"]
fn manager_tags_deletion_settings_and_sensitive_rules_work_through_ipc() {
    let mut service = Service::new(&[], true);
    let session = service.state().session;
    let owner = Owner::new(offers(Some("ManagerEditor"), b"synthetic manager first"));
    let (first, _) = saved(service.save());
    let owner2 = Owner::new(offers(Some("ManagerEditor"), b"synthetic manager second"));
    let (second, _) = saved(service.save());
    drop(owner2);

    let info = match service.request(Request::ManagerInfo { session }).unwrap() {
        Reply::ManagerInfo(info) => info,
        _ => panic!("expected manager info"),
    };
    assert_eq!(info.items, 2);
    assert!(info.logical_bytes > 0 && info.physical_bytes > 0);
    let category = info
        .sources
        .iter()
        .find(|facet| matches!(&facet.source, SourceIdentity::Observed { app_id, .. } if app_id == "x11:wm-class:ManagerEditor"))
        .expect("observed manager category")
        .id;
    let page = match service
        .request(Request::Browse {
            session,
            query: snippy::storage::BrowseQuery {
                source_id: Some(category),
                full: true,
                ..Default::default()
            },
        })
        .unwrap()
    {
        Reply::Page(page) => page,
        _ => panic!("expected manager page"),
    };
    assert_eq!(page.rows.len(), 2);
    assert!(page.rows.iter().all(|row| row.source_id == category));
    let row = page.rows.iter().find(|row| row.id == first).unwrap();
    assert_eq!(row.text.as_str(), "synthetic manager first");
    assert!(row.first_saved_ms > 0);
    assert_eq!(row.first_saved_ms, row.last_saved_ms);

    let tags = match service
        .request(Request::AddTag {
            session,
            id: first,
            tag: "review".into(),
        })
        .unwrap()
    {
        Reply::Tags { id, tags } => {
            assert_eq!(id, first);
            tags
        }
        _ => panic!("expected tags"),
    };
    assert_eq!(tags, ["review"]);
    let tags = match service
        .request(Request::AddTag {
            session,
            id: first,
            tag: "even".into(),
        })
        .unwrap()
    {
        Reply::Tags { tags, .. } => tags,
        _ => panic!("expected tags"),
    };
    assert_eq!(tags, ["even", "review"]);
    let tags = match service
        .request(Request::RemoveTag {
            session,
            id: first,
            tag: "even".into(),
        })
        .unwrap()
    {
        Reply::Tags { tags, .. } => tags,
        _ => panic!("expected tags"),
    };
    assert_eq!(tags, ["review"]);

    let apps = match service.request(Request::SensitiveApps { session }).unwrap() {
        Reply::SensitiveApps(apps) => apps,
        _ => panic!("expected sensitive apps"),
    };
    assert!(apps.iter().any(|app| app == "x11:wm-class:KeePassXC"));
    let apps = match service
        .request(Request::AddSensitive {
            session,
            app_id: "x11:wm-class:ReviewApp".into(),
        })
        .unwrap()
    {
        Reply::SensitiveApps(apps) => apps,
        _ => panic!("expected sensitive apps"),
    };
    assert!(apps.iter().any(|app| app == "x11:wm-class:ReviewApp"));
    let apps = match service
        .request(Request::RemoveSensitive {
            session,
            app_id: "x11:wm-class:ReviewApp".into(),
        })
        .unwrap()
    {
        Reply::SensitiveApps(apps) => apps,
        _ => panic!("expected sensitive apps"),
    };
    assert!(!apps.iter().any(|app| app == "x11:wm-class:ReviewApp"));

    let config = match service.request(Request::Config { session }).unwrap() {
        Reply::Config(config) => config,
        _ => panic!("expected config"),
    };
    assert_eq!(
        config.max_clip_bytes,
        CaptureLimits::default().max_clip_bytes
    );
    assert!(config.lock_timeout_seconds >= 60);
    let config = match service
        .request(Request::SetConfig {
            session,
            enabled: true,
            autostart: true,
            notifications: false,
            save: "Super+F9".into(),
            picker: "Ctrl+Alt+Y".into(),
            max_clip_bytes: 4096,
            max_vault_bytes: 8190,
            lock_timeout_seconds: 120,
        })
        .unwrap()
    {
        Reply::Config(config) => config,
        _ => panic!("expected config reply"),
    };
    assert!(config.enabled);
    assert!(config.autostart);
    assert!(!config.notifications);
    assert_eq!(config.max_clip_bytes, 4096);
    assert_eq!(config.max_vault_bytes, 8190);
    assert_eq!(config.lock_timeout_seconds, 120);
    assert_eq!(config.save, "Super+F9");
    // The fixture daemon runs with --no-shortcuts: the saved preference is
    // enabled, but the one-run override still wins and nothing is grabbed.
    let state = service.state();
    assert_eq!((state.save_binding, state.picker_binding), (0, 0));
    let settings = service._dir.path().join("config/snippy/settings.conf");
    let text = fs::read_to_string(&settings).unwrap();
    assert!(text.contains("shortcuts_enabled=true"));
    assert!(
        !text.contains("lock_timeout_seconds=60\n"),
        "one-run override was persisted"
    );
    assert!(text.contains("max_clip_bytes=4096"));
    assert!(text.contains("save_shortcut=Super+F9"));
    assert!(text.contains("lock_timeout_seconds=120"));
    assert!(text.contains("autostart=true"));
    let autostart = service._dir.path().join("config/autostart/snippy.desktop");
    let desktop = fs::read_to_string(&autostart).unwrap();
    assert!(desktop.contains("X-snippy-Autostart=true"));
    assert!(desktop.contains(" daemon\n"));
    assert_eq!(
        fs::metadata(&autostart).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let disabled = match service
        .request(Request::SetConfig {
            session,
            enabled: true,
            autostart: false,
            notifications: false,
            save: "Super+F9".into(),
            picker: "Ctrl+Alt+Y".into(),
            max_clip_bytes: 4096,
            max_vault_bytes: 8190,
            lock_timeout_seconds: 120,
        })
        .unwrap()
    {
        Reply::Config(config) => config,
        _ => panic!("expected config reply"),
    };
    assert!(!disabled.autostart);
    assert!(!autostart.exists());
    assert!(matches!(
        service
            .request(Request::Delete {
                session,
                id: second,
            })
            .unwrap(),
        Reply::Deleted { count: 1 }
    ));
    let owner3 = Owner::new(offers(Some("ManagerEditor"), b"synthetic manager third"));
    let (third, _) = saved(service.save());
    let owner4 = Owner::new(offers(Some("ManagerEditor"), b"synthetic manager fourth"));
    let (fourth, _) = saved(service.save());
    drop((owner3, owner4));
    assert!(matches!(
        service
            .request(Request::DeleteMany {
                session,
                ids: vec![third, fourth, 999_999],
            })
            .unwrap(),
        Reply::Deleted { count: 2 }
    ));
    let info = match service.request(Request::ManagerInfo { session }).unwrap() {
        Reply::ManagerInfo(info) => info,
        _ => panic!("expected manager info"),
    };
    assert_eq!(info.items, 1);
    assert!(info.tags.iter().any(|tag| tag.name == "review"));

    drop(owner);
    ipc::call(&service.paths, Request::Lock).unwrap();
    assert_eq!(
        service.request(Request::ManagerInfo { session }).err(),
        Some(Error::Stale)
    );
    service.stop();
}
