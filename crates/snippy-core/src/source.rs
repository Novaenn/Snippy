//! Source provenance is separate from any desktop's window or client handles.

/// Private metadata: persist only inside the encrypted vault, never in logs.
/// Deliberately has no `Debug` implementation.
#[derive(Clone, PartialEq, Eq)]
pub enum SourceIdentity {
    Observed {
        app_id: String,
        display_name: String,
    },
    Manual {
        label: String,
    },
    Unknown,
}
