//! Optional content-free desktop notifications. Core operation never depends on them.

use std::process::{Command, Stdio};

/// Best-effort notification for a committed Save. Only fixed text is sent.
pub fn save_succeeded(enabled: bool) {
    notify(enabled, "Saved successfully.");
}

/// Best-effort notification for serving a stored item onto the clipboard.
/// Only fixed text is sent.
pub fn copied(enabled: bool) {
    notify(enabled, "Copied to clipboard.");
}

fn notify(enabled: bool, body: &'static str) {
    if !enabled {
        return;
    }
    let child = notification_command(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        // Reap the optional helper without blocking the service loop.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

fn notification_command(body: &str) -> Command {
    let mut command = Command::new("notify-send");
    command.args(["--app-name=Snippy", "--icon=snippy", "Snippy", body]);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(body: &str) -> Vec<String> {
        notification_command(body)
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn notification_text_is_fixed_and_contains_no_clipboard_data() {
        assert_eq!(
            arguments("Saved successfully."),
            [
                "--app-name=Snippy",
                "--icon=snippy",
                "Snippy",
                "Saved successfully."
            ]
        );
        assert_eq!(
            arguments("Copied to clipboard."),
            [
                "--app-name=Snippy",
                "--icon=snippy",
                "Snippy",
                "Copied to clipboard."
            ]
        );
    }
}
