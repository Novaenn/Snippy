//! Optional content-free desktop notifications. Core operation never depends on them.

use std::process::{Command, Stdio};

/// Best-effort notification for a committed Save. Only fixed text is sent.
pub fn save_succeeded(enabled: bool) {
    if !enabled {
        return;
    }
    let child = notification_command()
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

fn notification_command() -> Command {
    let mut command = Command::new("notify-send");
    command.args([
        "--app-name=Snippy",
        "--icon=snippy",
        "Snippy",
        "Saved successfully.",
    ]);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_text_is_fixed_and_contains_no_clipboard_data() {
        let command = notification_command();
        let arguments: Vec<_> = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            arguments,
            [
                "--app-name=Snippy",
                "--icon=snippy",
                "Snippy",
                "Saved successfully."
            ]
        );
    }
}
