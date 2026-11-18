use crate::{
    ipc::{Error, Request},
    storage::{Passphrase, Payload, SaveResult, Vault},
    x11::capture::Received,
};
use std::{
    io::Write,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
};

pub(super) enum Work {
    Check(snippy_core::source::SourceIdentity),
    Prepare {
        received: Received,
        revoked: Arc<AtomicBool>,
    },
    Commit {
        source: snippy_core::source::SourceIdentity,
        payload: Payload,
        revoked: Arc<AtomicBool>,
        decision: Arc<snippy_core::capture::CommitPermit>,
    },
    Verify,
    Browse(crate::storage::BrowseQuery),
    Preview(i64),
    Copy(i64),
    /// Persist `base` and apply `runtime` (base plus one-run overrides) limits.
    SetLimits {
        base: Box<snippy_core::config::AppConfig>,
        runtime: Box<snippy_core::config::AppConfig>,
        previous_autostart: bool,
        settings: std::path::PathBuf,
        autostart: std::path::PathBuf,
        executable: std::path::PathBuf,
    },
    SensitiveApps,
    AddSensitive(String),
    RemoveSensitive(String),
    AddTag(i64, String),
    RemoveTag(i64, String),
    Delete(i64),
    DeleteMany(Vec<i64>),
    ManagerInfo,
}
pub(super) enum WorkResult {
    Checked(bool),
    Prepared(Payload),
    Saved(SaveResult),
    Verification(crate::ipc::Verification),
    Page(crate::storage::Page),
    Image(crate::storage::ImagePreview),
    Content(crate::storage::ClipboardContent),
    /// Files were written; the service must apply this configuration.
    Configured {
        base: Box<snippy_core::config::AppConfig>,
        runtime: Box<snippy_core::config::AppConfig>,
    },
    SensitiveApps(Vec<String>),
    Tags(i64, Vec<String>),
    Deleted(u64),
    ManagerInfo(crate::ipc::ManagerInfo),
}
#[derive(Clone, Copy)]
pub(super) enum WorkKey {
    Capture(u64),
    Verify(u64, u64),
    Read(u64, u64),
    /// Client, request, and the clipboard ownership generation at request time.
    Copy(u64, u64, u64),
    Config(u64, u64),
}
pub(super) enum Job {
    Unseal {
        epoch: u64,
        request: Request,
    },
    Lock {
        epoch: u64,
    },
    Work {
        epoch: u64,
        key: WorkKey,
        work: Work,
    },
}
pub(super) struct Completion {
    pub epoch: u64,
    pub result: Result<(), Error>,
    pub exists: bool,
    pub lock: bool,
    pub work: Option<(WorkKey, Result<WorkResult, Error>)>,
}
pub(super) struct Worker {
    pub jobs: Option<SyncSender<Job>>,
    pub results: Receiver<Completion>,
    pub wake: UnixStream,
    join: Option<JoinHandle<()>>,
}
impl Worker {
    pub fn start(mut vault: Vault, epoch: Arc<AtomicU64>) -> Result<Self, Error> {
        let (wake, mut writer) = UnixStream::pair()?;
        wake.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        let (jobs, recv) = mpsc::sync_channel(4);
        let (send, results) = mpsc::sync_channel(4);
        let join = thread::spawn(move || {
            while let Ok(job) = recv.recv() {
                let done = execute(&mut vault, &epoch, job);
                if send.send(done).is_err() {
                    break;
                }
                let _ = writer.write(&[1]);
            }
            let _ = vault.lock();
        });
        Ok(Self {
            jobs: Some(jobs),
            results,
            wake,
            join: Some(join),
        })
    }
    pub fn send(&self, job: Job) -> Result<(), Error> {
        self.jobs
            .as_ref()
            .ok_or(Error::Unavailable)?
            .try_send(job)
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => Error::Busy,
                // A dead worker cannot recover by retrying.
                mpsc::TrySendError::Disconnected(_) => Error::Storage,
            })
    }
    pub fn finish(&mut self) {
        self.jobs.take();
        // Do not let a full result queue deadlock shutdown after the poll loop
        // stops consuming. Dropped queued work releases its owned secret buffers.
        let (_, empty) = mpsc::channel();
        drop(std::mem::replace(&mut self.results, empty));
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.finish()
    }
}
fn execute(vault: &mut Vault, current: &AtomicU64, job: Job) -> Completion {
    execute_with(vault, current, job, || {})
}
fn execute_with(
    vault: &mut Vault,
    current: &AtomicU64,
    job: Job,
    after: impl FnOnce(),
) -> Completion {
    if let Job::Work { epoch, key, work } = job {
        let result = (|| {
            if current.load(Ordering::SeqCst) != epoch || !vault.is_unlocked() {
                return Err(Error::Canceled);
            }
            let mutation = matches!(work, Work::AddSensitive(_) | Work::RemoveSensitive(_));
            let result = match work {
                Work::Check(source) => WorkResult::Checked(vault.is_sensitive(&source)?),
                Work::Prepare { received, revoked } => {
                    if revoked.load(Ordering::SeqCst) {
                        return Err(Error::Canceled);
                    }
                    let payload = if received.png {
                        Payload::png(&received.bytes, vault.limits())?
                    } else if received.latin1 {
                        // Allocate the exact UTF-8 size once so growth never
                        // frees an unzeroed partial copy.
                        let size = received
                            .bytes
                            .iter()
                            .map(|&byte| if byte < 0x80 { 1 } else { 2 })
                            .sum();
                        let mut text = zeroize::Zeroizing::new(String::with_capacity(size));
                        for &byte in received.bytes.iter() {
                            text.push(char::from(byte));
                        }
                        Payload::text(&text, vault.limits())?
                    } else {
                        Payload::text(
                            std::str::from_utf8(&received.bytes)
                                .map_err(|_| Error::InvalidContent)?,
                            vault.limits(),
                        )?
                    };
                    if revoked.load(Ordering::SeqCst) {
                        return Err(Error::Canceled);
                    }
                    WorkResult::Prepared(payload)
                }
                Work::Commit {
                    source,
                    payload,
                    revoked,
                    decision,
                } => WorkResult::Saved(vault.save_guarded(&source, &payload, || {
                    current.load(Ordering::SeqCst) == epoch
                        && !revoked.load(Ordering::SeqCst)
                        && decision.claim()
                })?),
                Work::Browse(query) => {
                    let mut page = vault.browse(&query)?;
                    page.session = epoch;
                    WorkResult::Page(page)
                }
                Work::Preview(id) => {
                    let mut preview = vault.image_preview(id)?;
                    preview.session = epoch;
                    WorkResult::Image(preview)
                }
                Work::Copy(id) => WorkResult::Content(vault.clipboard_content(id)?),
                Work::Verify => {
                    let usage = vault.usage()?;
                    WorkResult::Verification(crate::ipc::Verification {
                        session: epoch,
                        items: usage.items,
                        logical_bytes: usage.logical_bytes,
                        recent: vault.list(1, 0)?.pop(),
                    })
                }
                Work::SetLimits {
                    base,
                    runtime,
                    previous_autostart,
                    settings,
                    autostart,
                    executable,
                } => {
                    let previous = vault.limits().clone();
                    vault.set_limits(runtime.limits.clone())?;
                    if current.load(Ordering::SeqCst) != epoch {
                        let _ = vault.set_limits(previous);
                        return Err(Error::Canceled);
                    }
                    let autostart = crate::autostart::Autostart::at(autostart);
                    if let Err(error) = autostart.set(base.autostart, &executable) {
                        let _ = vault.set_limits(previous);
                        return Err(error);
                    }
                    if let Err(error) = crate::settings::Store::at(settings).save(&base) {
                        let _ = autostart.set(previous_autostart, &executable);
                        let _ = vault.set_limits(previous);
                        return Err(error);
                    }
                    // Persisted: report it even if the session ends now, so the
                    // service applies exactly what was written.
                    return Ok(WorkResult::Configured { base, runtime });
                }
                Work::SensitiveApps => WorkResult::SensitiveApps(vault.sensitive_apps()?),
                Work::AddSensitive(app_id) => {
                    WorkResult::SensitiveApps(vault.edit_sensitive(&app_id, true)?)
                }
                Work::RemoveSensitive(app_id) => {
                    WorkResult::SensitiveApps(vault.edit_sensitive(&app_id, false)?)
                }
                Work::AddTag(id, tag) => WorkResult::Tags(id, vault.edit_tag(id, &tag, true)?),
                Work::RemoveTag(id, tag) => WorkResult::Tags(id, vault.edit_tag(id, &tag, false)?),
                Work::Delete(id) => {
                    vault.delete(id)?;
                    WorkResult::Deleted(1)
                }
                Work::DeleteMany(ids) => WorkResult::Deleted(vault.delete_many(&ids)?),
                Work::ManagerInfo => {
                    let usage = vault.usage()?;
                    let facets = vault.facets()?;
                    WorkResult::ManagerInfo(crate::ipc::ManagerInfo {
                        session: epoch,
                        items: usage.items,
                        logical_bytes: usage.logical_bytes,
                        physical_bytes: vault.physical_bytes()?,
                        sources: facets.sources,
                        tags: facets.tags,
                    })
                }
            };
            // Discard reads after a session change. Committed writes are reported;
            // the service maps them to Stale for a session that ended meanwhile.
            let committed = matches!(
                result,
                WorkResult::Saved(_) | WorkResult::Tags(_, _) | WorkResult::Deleted(_)
            ) || (mutation && matches!(result, WorkResult::SensitiveApps(_)));
            if current.load(Ordering::SeqCst) != epoch && !committed {
                return Err(Error::Canceled);
            }
            Ok(result)
        })();
        return Completion {
            epoch,
            result: Ok(()),
            exists: true,
            lock: false,
            work: Some((key, result)),
        };
    }
    let (epoch, result, lock) = match job {
        Job::Work { .. } => unreachable!(),
        Job::Lock { epoch } => (epoch, vault.lock().map_err(Error::from), true),
        Job::Unseal { epoch, request } => {
            let result = if current.load(Ordering::SeqCst) != epoch {
                Err(Error::Canceled)
            } else {
                (|| match request {
                    Request::Create {
                        mut passphrase,
                        mut confirmation,
                    } => {
                        let pass = Passphrase::new(std::mem::take(&mut *passphrase))?;
                        let confirm = Passphrase::new(std::mem::take(&mut *confirmation))?;
                        vault.create(&pass, &confirm)
                    }
                    Request::Unlock(mut value) => {
                        let pass = Passphrase::new(std::mem::take(&mut *value))?;
                        vault.unlock(&pass)
                    }
                    _ => Err(crate::storage::StorageError::InvalidContent),
                })()
                .map_err(Error::from)
            };
            after();
            let result = if current.load(Ordering::SeqCst) != epoch {
                let _ = vault.lock();
                Err(Error::Canceled)
            } else {
                result
            };
            if result.is_err() {
                let _ = vault.lock();
            }
            (epoch, result, false)
        }
    };
    Completion {
        epoch,
        result,
        exists: std::fs::symlink_metadata(vault.path()).is_ok(),
        lock,
        work: None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use snippy_core::config::CaptureLimits;
    use zeroize::Zeroizing;
    #[cfg(feature = "sqlcipher")]
    #[test]
    fn lock_during_derivation_closes_the_newly_unlocked_connection() {
        let dir = tempfile::tempdir().unwrap();
        let mut vault = Vault::at(dir.path().join("vault"), CaptureLimits::default()).unwrap();
        let current = AtomicU64::new(1);
        let request = Request::Create {
            passphrase: Zeroizing::new("synthetic".into()),
            confirmation: Zeroizing::new("synthetic".into()),
        };
        let done = execute_with(
            &mut vault,
            &current,
            Job::Unseal { epoch: 1, request },
            || current.store(2, Ordering::SeqCst),
        );
        assert_eq!(done.result, Err(Error::Canceled));
        assert!(!vault.is_unlocked());
        assert!(done.exists);
        vault
            .unlock(&Passphrase::new("synthetic".into()).unwrap())
            .unwrap();
    }
    #[cfg(feature = "sqlcipher")]
    #[test]
    fn capture_work_respects_revocation_and_lock_and_validates_text() {
        use snippy_core::{capture::CommitPermit, source::SourceIdentity};
        let dir = tempfile::tempdir().unwrap();
        let mut vault = Vault::at(dir.path().join("vault"), CaptureLimits::default()).unwrap();
        let pass = Passphrase::new("synthetic-worker-key".into()).unwrap();
        vault.create(&pass, &pass).unwrap();
        let current = AtomicU64::new(1);
        let received = |bytes: &[u8], latin1| Received {
            png: false,
            latin1,
            bytes: Zeroizing::new(bytes.to_vec()),
        };
        let revoked = Arc::new(AtomicBool::new(true));
        let done = execute(
            &mut vault,
            &current,
            Job::Work {
                epoch: 1,
                key: WorkKey::Capture(1),
                work: Work::Prepare {
                    received: received(b"synthetic", false),
                    revoked: revoked.clone(),
                },
            },
        );
        assert!(matches!(done.work, Some((_, Err(Error::Canceled)))));
        revoked.store(false, Ordering::SeqCst);
        let done = execute(
            &mut vault,
            &current,
            Job::Work {
                epoch: 1,
                key: WorkKey::Capture(1),
                work: Work::Prepare {
                    received: received(&[255], false),
                    revoked: revoked.clone(),
                },
            },
        );
        assert!(matches!(done.work, Some((_, Err(Error::InvalidContent)))));
        let done = execute(
            &mut vault,
            &current,
            Job::Work {
                epoch: 1,
                key: WorkKey::Capture(1),
                work: Work::Prepare {
                    received: received(b"  caf\xe9\n", true),
                    revoked: revoked.clone(),
                },
            },
        );
        let Some((_, Ok(WorkResult::Prepared(payload)))) = done.work else {
            panic!("expected decoded text");
        };
        assert_eq!(payload.bytes(), "  café\n".as_bytes());
        current.store(2, Ordering::SeqCst);
        let done = execute(
            &mut vault,
            &current,
            Job::Work {
                epoch: 1,
                key: WorkKey::Capture(1),
                work: Work::Commit {
                    source: SourceIdentity::Unknown,
                    payload,
                    revoked,
                    decision: Arc::new(CommitPermit::default()),
                },
            },
        );
        assert!(matches!(done.work, Some((_, Err(Error::Canceled)))));
        assert_eq!(vault.usage().unwrap().items, 0);
    }
    #[cfg(feature = "sqlcipher")]
    #[test]
    fn settings_job_saves_the_base_and_returns_both_configurations() {
        use snippy_core::config::AppConfig;
        let dir = tempfile::tempdir().unwrap();
        let mut vault = Vault::at(dir.path().join("vault"), CaptureLimits::default()).unwrap();
        let pass = Passphrase::new("synthetic-settings-key".into()).unwrap();
        vault.create(&pass, &pass).unwrap();
        let base = AppConfig {
            notifications: true,
            ..AppConfig::default()
        };
        let mut runtime = base.clone();
        runtime.limits.max_clip_bytes = 4096;
        let settings = dir.path().join("config/snippy/settings.conf");
        let done = execute(
            &mut vault,
            &AtomicU64::new(1),
            Job::Work {
                epoch: 1,
                key: WorkKey::Config(1, 1),
                work: Work::SetLimits {
                    base: Box::new(base.clone()),
                    runtime: Box::new(runtime.clone()),
                    previous_autostart: false,
                    settings: settings.clone(),
                    autostart: dir.path().join("autostart/snippy.desktop"),
                    executable: "/nonexistent/snippy".into(),
                },
            },
        );
        let Some((
            _,
            Ok(WorkResult::Configured {
                base: saved,
                runtime: applied,
            }),
        )) = done.work
        else {
            panic!("expected a persisted configuration");
        };
        assert_eq!((*saved, *applied), (base.clone(), runtime));
        assert_eq!(vault.limits().max_clip_bytes, 4096);
        assert_eq!(crate::settings::Store::at(settings).load().unwrap(), base);
    }
    #[test]
    fn dead_worker_is_a_storage_failure_not_a_retryable_busy() {
        let (jobs, receiver) = mpsc::sync_channel(1);
        let (_, results) = mpsc::sync_channel(1);
        let (wake, _) = UnixStream::pair().unwrap();
        let worker = Worker {
            jobs: Some(jobs),
            results,
            wake,
            join: None,
        };
        worker.send(Job::Lock { epoch: 1 }).unwrap();
        assert_eq!(worker.send(Job::Lock { epoch: 1 }), Err(Error::Busy));
        drop(receiver);
        assert_eq!(worker.send(Job::Lock { epoch: 1 }), Err(Error::Storage));
    }
    #[test]
    fn canceled_job_drops_credentials_without_creating_vault() {
        let dir = tempfile::tempdir().unwrap();
        let mut vault = Vault::at(dir.path().join("vault"), CaptureLimits::default()).unwrap();
        let request = Request::Create {
            passphrase: Zeroizing::new("synthetic".into()),
            confirmation: Zeroizing::new("synthetic".into()),
        };
        let done = execute(
            &mut vault,
            &AtomicU64::new(2),
            Job::Unseal { epoch: 1, request },
        );
        assert_eq!(done.result, Err(Error::Canceled));
        assert!(!vault.is_unlocked());
        assert!(!vault.path().exists());
    }
}
