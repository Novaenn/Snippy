use crate::ipc::{Error, State, VaultState};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub(super) struct Lifecycle {
    pub state: State,
    pub epoch: Arc<AtomicU64>,
    pub storage_locked: bool,
    timeout: Duration,
    last_activity: Option<Instant>,
}
impl Lifecycle {
    pub fn new(exists: bool, timeout: Duration) -> Self {
        Self {
            state: State {
                revision: 1,
                session: 0,
                phase: VaultState::Locked,
                exists,
                save_binding: 0,
                picker_binding: 0,
                capture: 0,
                capture_stage: 0,
                save_outcome: 0,
                gui_mode: 0,
                history_revision: 0,
            },
            epoch: Arc::new(AtomicU64::new(0)),
            storage_locked: true,
            timeout,
            last_activity: None,
        }
    }
    pub fn changed(&mut self) {
        self.state.revision = self.state.revision.saturating_add(1)
    }
    fn next_session(&mut self) {
        self.state.session = self.state.session.saturating_add(1);
        self.epoch.store(self.state.session, Ordering::SeqCst);
        self.changed();
    }
    pub fn begin(&mut self) -> Result<u64, Error> {
        if self.state.phase != VaultState::Locked || !self.storage_locked {
            return Err(Error::Busy);
        }
        self.next_session();
        self.state.phase = VaultState::Unlocking;
        self.storage_locked = false;
        Ok(self.state.session)
    }
    pub fn complete(
        &mut self,
        epoch: u64,
        result: Result<(), Error>,
        exists: bool,
        now: Instant,
    ) -> bool {
        if self.state.exists != exists {
            self.state.exists = exists;
            self.changed()
        }
        if epoch != self.state.session || self.state.phase != VaultState::Unlocking {
            return false;
        }
        self.state.phase = if result.is_ok() {
            VaultState::Unlocked
        } else {
            VaultState::Locked
        };
        self.storage_locked = result.is_err();
        self.last_activity = result.ok().map(|()| now);
        self.changed();
        true
    }
    pub fn lock(&mut self) -> u64 {
        if self.state.phase != VaultState::Locked {
            self.next_session();
            self.state.phase = VaultState::Locked;
            self.last_activity = None
        }
        self.state.session
    }
    pub fn acknowledge_lock(&mut self, epoch: u64) {
        if epoch == self.state.session && self.state.phase == VaultState::Locked {
            self.storage_locked = true
        }
    }
    pub fn touch(&mut self, session: u64, now: Instant) -> Result<(), Error> {
        if self.state.phase != VaultState::Unlocked || session != self.state.session {
            return Err(Error::Stale);
        }
        self.last_activity = Some(now);
        Ok(())
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.last_activity.and_then(|t| t.checked_add(self.timeout))
    }
    /// Changing the timeout affects the current deadline without counting as activity.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }
    pub fn expired(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| deadline <= now)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lock_invalidates_late_unlock_and_old_activity() {
        let now = Instant::now();
        let mut c = Lifecycle::new(false, Duration::from_secs(5));
        let pending = c.begin().unwrap();
        let locked = c.lock();
        assert!(locked > pending);
        assert!(!c.complete(pending, Ok(()), true, now));
        assert_eq!(c.state.phase, VaultState::Locked);
        c.acknowledge_lock(locked);
        let next = c.begin().unwrap();
        assert!(c.complete(next, Ok(()), true, now));
        assert_eq!(c.touch(pending, now), Err(Error::Stale));
        assert!(c.expired(now + Duration::from_secs(5)));
    }
    #[test]
    fn only_explicit_current_session_activity_extends_timeout() {
        let now = Instant::now();
        let mut c = Lifecycle::new(true, Duration::from_secs(5));
        let session = c.begin().unwrap();
        c.complete(session, Ok(()), true, now);
        let original = c.deadline();
        for _ in 0..1000 {
            let _status = c.state; /* ownership/status events intentionally do not call touch */
        }
        assert_eq!(c.deadline(), original);
        c.touch(session, now + Duration::from_secs(4)).unwrap();
        assert!(!c.expired(now + Duration::from_secs(5)));
        assert!(c.expired(now + Duration::from_secs(9)));
    }
}
