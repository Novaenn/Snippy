//! Authorization policies; no clipboard bytes or platform/toolkit handles.
use crate::source::SourceIdentity;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Confirmation {
    None,
    Unknown,
    Sensitive,
}

pub fn confirmation(source: &SourceIdentity, sensitive: bool) -> Confirmation {
    if sensitive {
        Confirmation::Sensitive
    } else if matches!(source, SourceIdentity::Unknown) {
        Confirmation::Unknown
    } else {
        Confirmation::None
    }
}

/// Labels are a per-capture choice, exclusively for an unknown source. They
/// cannot replace an observed identity or satisfy a sensitive-source warning.
pub fn approved_source(
    source: &SourceIdentity,
    warning: Confirmation,
    label: &str,
) -> Option<SourceIdentity> {
    if label.len() > 256 || label.chars().any(char::is_control) {
        return None;
    }
    let label = label.trim();
    if label.is_empty() {
        return Some(source.clone());
    }
    if warning != Confirmation::Unknown || !matches!(source, SourceIdentity::Unknown) {
        return None;
    }
    Some(SourceIdentity::Manual {
        label: label.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sensitive_warning_cannot_be_bypassed_with_a_manual_label() {
        let source = SourceIdentity::Observed {
            app_id: "sensitive".into(),
            display_name: "Synthetic".into(),
        };
        assert_eq!(confirmation(&source, true), Confirmation::Sensitive);
        assert!(approved_source(&source, Confirmation::Sensitive, "innocent").is_none());
        assert!(approved_source(&source, Confirmation::Sensitive, "") == Some(source));
        assert_eq!(
            confirmation(&SourceIdentity::Unknown, true),
            Confirmation::Sensitive
        );
        assert!(
            approved_source(&SourceIdentity::Unknown, Confirmation::Sensitive, "label").is_none()
        );
    }
    #[test]
    fn unknown_labels_are_explicit_and_never_observed() {
        assert_eq!(
            confirmation(&SourceIdentity::Unknown, false),
            Confirmation::Unknown
        );
        assert!(
            matches!(approved_source(&SourceIdentity::Unknown, Confirmation::Unknown, " demo "), Some(SourceIdentity::Manual { label }) if label == "demo")
        );
        assert!(
            approved_source(&SourceIdentity::Unknown, Confirmation::Unknown, "")
                == Some(SourceIdentity::Unknown)
        );
        assert!(
            approved_source(
                &SourceIdentity::Unknown,
                Confirmation::Unknown,
                "bad\nlabel"
            )
            .is_none()
        );
    }
}

/// Linearizes cancellation with the final storage commit. Once commit has been
/// claimed, a later lock/change cannot turn a committed Save into a failure.
#[derive(Default)]
pub struct CommitPermit(std::sync::atomic::AtomicU8);
impl CommitPermit {
    pub fn cancel(&self) -> bool {
        self.0
            .compare_exchange(
                0,
                1,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
    }
    pub fn claim(&self) -> bool {
        self.0
            .compare_exchange(
                0,
                2,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
    }
}
#[cfg(test)]
mod commit_tests {
    use super::*;
    #[test]
    fn canceled_captures_cannot_commit_and_committed_saves_cannot_be_canceled() {
        let canceled = CommitPermit::default();
        assert!(canceled.cancel());
        assert!(!canceled.claim());
        let committing = CommitPermit::default();
        assert!(committing.claim());
        assert!(!committing.cancel());
        assert!(!committing.claim());
    }
}
