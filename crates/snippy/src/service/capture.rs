//! Save coordination. Only this module authorizes transfer and storage work.
use super::*;
use crate::{
    ipc::{Prompt, VaultState},
    x11::{Snapshot, capture::Transfer},
};
use snippy_core::{
    capture::{Confirmation, approved_source, confirmation},
    source::SourceIdentity,
};
use worker::{Work, WorkKey, WorkResult};

/// x11_events handles at most 64 events per batch.
const FENCE_BATCHES: usize = 16;
pub(super) struct Capture {
    pub id: u64,
    pub snapshot: Snapshot,
    source: SourceIdentity,
    warning: Confirmation,
    pub revoked: Arc<AtomicBool>,
    decision: Arc<snippy_core::capture::CommitPermit>,
    waiters: Vec<(u64, u64)>,
    deadline: Instant,
    had_gui: bool,
    /// The worker claimed the commit permit; only its result can end this Save.
    committing: bool,
}
impl Capture {
    pub(super) fn clear_sources(&mut self) {
        self.source = SourceIdentity::Unknown;
        self.snapshot.source = SourceIdentity::Unknown;
    }
}
impl Server {
    pub(super) fn save(&mut self, waiter: Option<(u64, u64)>) -> Result<(), Error> {
        if self.observer.is_none() {
            return Err(Error::X11);
        }
        if self.observer.as_ref().is_some_and(|o| o.serving_current()) {
            if self.control.state.phase == VaultState::Unlocked {
                let _ = self
                    .control
                    .touch(self.control.state.session, Instant::now());
            }
            if let Some((client, id)) = waiter {
                self.reply(client, id, Reply::AlreadySaved);
            }
            self.control.state.save_outcome = 3;
            self.control.changed();
            self.broadcast();
            return Ok(());
        }
        if self.control.state.phase == VaultState::Unlocked {
            let _ = self
                .control
                .touch(self.control.state.session, Instant::now());
        }
        if self.capture.is_some() {
            if !self.capture_valid() {
                self.cancel_capture(Error::SelectionChanged);
                return Err(Error::SelectionChanged);
            }
            if let Some(waiter) = waiter {
                if self.capture.as_ref().expect("capture").waiters.len() >= 8 {
                    return Err(Error::Busy);
                }
                self.capture.as_mut().expect("pending").waiters.push(waiter);
                self.mark_save_waiter(waiter.0);
            }
            return Ok(());
        }
        if self.transfer.is_some() {
            return Err(Error::Busy);
        }
        let snapshot = self.observer.as_ref().expect("observer").snapshot().clone();
        if snapshot.owner == 0 {
            return Err(Error::Unsupported);
        }
        self.next_capture = self.next_capture.checked_add(1).ok_or(Error::Busy)?;
        self.capture = Some(Capture {
            id: self.next_capture,
            source: snapshot.source.clone(),
            snapshot,
            warning: Confirmation::None,
            revoked: Arc::new(AtomicBool::new(false)),
            decision: Arc::new(snippy_core::capture::CommitPermit::default()),
            waiters: waiter.into_iter().collect(),
            deadline: Instant::now() + Duration::from_secs(120),
            had_gui: self.clients.iter().any(|c| c.watch && !c.closed),
            committing: false,
        });
        if let Some((client, _)) = waiter {
            self.mark_save_waiter(client);
        }
        self.control.state.capture = self.next_capture;
        self.control.state.save_outcome = 0;
        if self.control.state.phase != VaultState::Unlocked {
            self.capture_stage(1);
            if let Err(error) = self.open_gui() {
                self.cancel_capture(error);
            }
        } else {
            self.check_capture()?;
        }
        Ok(())
    }
    fn mark_save_waiter(&mut self, client: u64) {
        if let Some(c) = self.clients.iter_mut().find(|c| c.id == client) {
            c.pending = true;
            c.deadline = Some(Instant::now() + Duration::from_secs(125));
        }
    }
    pub(super) fn capture_stage(&mut self, stage: u8) {
        self.control.state.capture_stage = stage;
        self.control.changed();
        self.broadcast();
    }
    pub(super) fn capture_valid(&self) -> bool {
        self.capture.as_ref().is_some_and(|c| {
            self.observer.as_ref().is_some_and(|o| {
                o.snapshot().owner == c.snapshot.owner
                    && o.snapshot().generation == c.snapshot.generation
            })
        })
    }
    fn capture_fence(&mut self, id: u64) -> Result<bool, Error> {
        if let Some(observer) = &self.observer {
            observer.synchronize().map_err(|_| Error::X11)?;
        }
        // Drain notifications ordered before the fence. Only an actual
        // owner/generation change cancels; unrelated events (keys, bindings)
        // do not. A queue that cannot be drained in bounds fails closed.
        let mut drained = false;
        for _ in 0..FENCE_BATCHES {
            if !self.x11_events()? {
                drained = true;
                break;
            }
        }
        if self.capture.as_ref().is_none_or(|c| c.id != id) {
            return Ok(false);
        }
        if !drained || !self.capture_valid() {
            self.cancel_capture(Error::SelectionChanged);
            return Ok(false);
        }
        Ok(true)
    }
    pub(super) fn check_capture(&mut self) -> Result<(), Error> {
        if !self.capture_valid() {
            self.cancel_capture(Error::SelectionChanged);
            return Ok(());
        }
        let c = self.capture.as_ref().expect("capture");
        let job = Job::Work {
            epoch: self.control.state.session,
            key: WorkKey::Capture(c.id),
            work: Work::Check(c.snapshot.source.clone()),
        };
        if let Err(error) = self.worker.send(job) {
            self.cancel_capture(error);
        } else {
            self.capture_stage(2);
        }
        Ok(())
    }
    pub(super) fn prompt(&self) -> Option<Prompt> {
        if self.control.state.phase != VaultState::Unlocked || self.control.state.capture_stage != 3
        {
            return None;
        }
        self.capture.as_ref().map(|c| Prompt {
            session: self.control.state.session,
            capture: c.id,
            sensitive: c.warning == Confirmation::Sensitive,
            source: c.snapshot.source.clone(),
        })
    }
    pub(super) fn send_prompt(&mut self) {
        if let Some(prompt) = self.prompt()
            && let Some(gui) = self.clients.iter_mut().find(|c| c.watch && !c.closed)
        {
            gui.queue(0, Reply::Prompt(prompt));
        }
    }
    pub(super) fn confirm_capture(
        &mut self,
        session: u64,
        capture: u64,
        label: &str,
    ) -> Result<(), Error> {
        if self.control.state.phase != VaultState::Unlocked
            || self.control.state.session != session
            || self.control.state.capture != capture
            || self.control.state.capture_stage != 3
        {
            return Err(Error::Stale);
        }
        if !self.capture_valid() {
            self.cancel_capture(Error::SelectionChanged);
            return Err(Error::SelectionChanged);
        }
        let c = self.capture.as_mut().ok_or(Error::Stale)?;
        c.source =
            approved_source(&c.snapshot.source, c.warning, label).ok_or(Error::InvalidLabel)?;
        self.control.touch(session, Instant::now())?;
        self.begin_transfer()
    }
    fn begin_transfer(&mut self) -> Result<(), Error> {
        let id = self.capture.as_ref().ok_or(Error::Stale)?.id;
        if !self.capture_fence(id)? {
            return Ok(());
        }
        if !self.capture_valid() {
            self.cancel_capture(Error::SelectionChanged);
            return Ok(());
        }
        let c = self.capture.as_ref().ok_or(Error::Stale)?;
        match Transfer::start(c.snapshot.clone(), self.limits.clone(), c.revoked.clone()) {
            Ok(transfer) => {
                self.transfer = Some((c.id, transfer));
                self.capture_stage(4);
            }
            Err(error) => self.cancel_capture(error),
        }
        Ok(())
    }
    pub(super) fn transfer_complete(&mut self) -> Result<(), Error> {
        let Some((id, transfer)) = &mut self.transfer else {
            return Ok(());
        };
        let result = match transfer.result.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(()),
            Err(_) => Err(Error::X11),
        };
        let id = *id;
        self.transfer.take();
        if self.capture.as_ref().is_none_or(|c| c.id != id) {
            return Ok(());
        }
        if !self.capture_valid() {
            self.cancel_capture(Error::SelectionChanged);
            return Ok(());
        }
        match result {
            Err(error) => self.cancel_capture(error),
            Ok(received) => {
                let c = self.capture.as_ref().expect("capture");
                if let Err(error) = self.worker.send(Job::Work {
                    epoch: self.control.state.session,
                    key: WorkKey::Capture(c.id),
                    work: Work::Prepare {
                        received,
                        revoked: c.revoked.clone(),
                    },
                }) {
                    self.cancel_capture(error);
                }
            }
        }
        Ok(())
    }
    pub(super) fn capture_done(
        &mut self,
        id: u64,
        epoch: u64,
        result: Result<WorkResult, Error>,
    ) -> Result<(), Error> {
        if self.capture.as_ref().is_none_or(|c| c.id != id) {
            return Ok(());
        }
        // The commit permit is the cancellation boundary. A later ownership
        // event/lock must not report failure for a Save that already committed.
        let committed = matches!(result, Ok(WorkResult::Saved(_)));
        if let Err(error) = result.as_ref()
            && self.control.state.capture_stage == 5
        {
            let capture = self.capture.as_mut().expect("capture");
            capture.decision = Arc::new(snippy_core::capture::CommitPermit::default());
            capture.committing = false;
            self.cancel_capture(*error);
            return Ok(());
        }
        if !committed && (epoch != self.control.state.session || !self.capture_valid()) {
            self.cancel_capture(Error::SelectionChanged);
            return Ok(());
        }
        match result {
            Err(error) => self.cancel_capture(error),
            Ok(WorkResult::Checked(sensitive)) => {
                let c = self.capture.as_mut().expect("capture");
                c.warning = confirmation(&c.snapshot.source, sensitive);
                if c.warning == Confirmation::None {
                    self.begin_transfer()?;
                } else {
                    self.capture_stage(3);
                    if let Err(error) = self.open_gui() {
                        self.cancel_capture(error);
                    }
                    self.send_prompt();
                }
            }
            Ok(WorkResult::Prepared(payload)) => {
                if !self.capture_fence(id)? {
                    return Ok(());
                }
                let c = self.capture.as_ref().expect("capture");
                if let Err(error) = self.worker.send(Job::Work {
                    epoch,
                    key: WorkKey::Capture(id),
                    work: Work::Commit {
                        source: c.source.clone(),
                        payload,
                        revoked: c.revoked.clone(),
                        decision: c.decision.clone(),
                    },
                }) {
                    self.cancel_capture(error);
                } else {
                    self.capture_stage(5);
                }
            }
            Ok(WorkResult::Saved(result)) => {
                let capture = self.capture.take().expect("capture");
                for (client, id) in capture.waiters {
                    self.reply(
                        client,
                        id,
                        Reply::Saved {
                            id: result.id,
                            merged: result.merged,
                        },
                    );
                }
                self.control.state.capture = 0;
                self.control.state.history_revision =
                    self.control.state.history_revision.saturating_add(1);
                self.control.state.save_outcome = if result.merged { 2 } else { 1 };
                self.capture_stage(0);
                let _ = self.control.touch(epoch, Instant::now());
                crate::notifier::save_succeeded(self.config.notifications);
            }
            Ok(
                WorkResult::Verification(_)
                | WorkResult::Page(_)
                | WorkResult::Image(_)
                | WorkResult::Content(_)
                | WorkResult::Configured { .. }
                | WorkResult::SensitiveApps(_)
                | WorkResult::Tags(_, _)
                | WorkResult::Deleted(_)
                | WorkResult::ManagerInfo(_),
            ) => return Err(Error::Protocol),
        }
        Ok(())
    }
    pub(super) fn cancel_capture(&mut self, error: Error) {
        if let Some(capture) = &mut self.capture
            && !capture.decision.cancel()
        {
            // Already claimed for commit: wait for the worker without
            // re-arming an expired deadline (which would busy-poll).
            capture.committing = true;
            return;
        }
        if let Some(capture) = self.capture.take() {
            capture.revoked.store(true, Ordering::SeqCst);
            if let Some((_, transfer)) = &mut self.transfer {
                transfer.cancel();
            }
            let shortcut_only = capture.waiters.is_empty();
            for (client, id) in capture.waiters {
                self.reply(client, id, Reply::Error(error));
            }
            if shortcut_only && error != Error::Canceled {
                let _ = self.open_gui();
            }
            self.control.state.capture = 0;
            self.control.state.save_outcome = error as u8;
            self.capture_stage(0);
        }
    }
    pub(super) fn maintain_capture(&mut self) {
        if self.capture.as_ref().is_none_or(|c| c.committing) {
            return;
        }
        if !self.capture_valid() {
            self.cancel_capture(Error::SelectionChanged);
            return;
        }
        let c = self.capture.as_mut().expect("capture");
        let has_gui = self.clients.iter().any(|c| c.watch && !c.closed);
        let needs_gui = matches!(self.control.state.capture_stage, 1 | 3);
        let lost_gui = needs_gui && c.had_gui && !has_gui;
        c.had_gui |= has_gui;
        let had_waiters = !c.waiters.is_empty();
        c.waiters.retain(|(id, _)| {
            self.clients
                .iter()
                .any(|client| client.id == *id && !client.closed && client.pending)
        });
        let disconnected = had_waiters && c.waiters.is_empty();
        if c.deadline <= Instant::now() || lost_gui || disconnected {
            self.cancel_capture(Error::Canceled);
        }
    }
    pub(super) fn capture_deadline(&self) -> Option<Instant> {
        self.capture
            .as_ref()
            .filter(|c| !c.committing)
            .map(|c| c.deadline)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claimed_commit_ignores_cancellation_without_busy_polling() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = super::super::tests::idle_server(dir.path());
        let decision = Arc::new(snippy_core::capture::CommitPermit::default());
        assert!(decision.claim(), "worker claimed the commit permit");
        server.capture = Some(Capture {
            id: 1,
            snapshot: Snapshot {
                owner: 7,
                generation: 1,
                selection_timestamp: None,
                source: SourceIdentity::Unknown,
                unknown_reason: None,
            },
            source: SourceIdentity::Unknown,
            warning: Confirmation::None,
            revoked: Arc::new(AtomicBool::new(false)),
            decision,
            waiters: Vec::new(),
            deadline: Instant::now() - Duration::from_secs(1),
            had_gui: false,
            committing: false,
        });
        server.control.state.capture = 1;
        server.control.state.capture_stage = 5;
        assert!(server.capture_deadline().is_some());
        // An expired deadline or ownership change after the claim cannot
        // relabel the Save as failed, and must not leave a past poll deadline.
        server.maintain_capture();
        server.cancel_capture(Error::SelectionChanged);
        let capture = server.capture.as_ref().expect("commit still owns the Save");
        assert!(!capture.revoked.load(Ordering::SeqCst));
        assert_eq!(server.control.state.capture, 1);
        assert_eq!(server.capture_deadline(), None);
    }
}
