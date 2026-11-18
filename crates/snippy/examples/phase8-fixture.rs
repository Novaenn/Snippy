//! Synthetic near-quota vault for phase 8 release measurements. No real data.

use snippy::storage::{Passphrase, Payload, Vault};
use snippy_core::{config::CaptureLimits, source::SourceIdentity};
use std::{
    fs::DirBuilder,
    io::{self, Read},
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
};

fn main() {
    let mut directory = None;
    let mut items = 800usize;
    let mut bytes = 60_000usize;
    let mut quota = 64 * 1024 * 1024u64;
    let mut stdin_passphrase = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => directory = args.next().map(PathBuf::from),
            "--items" => items = args.next().and_then(|v| v.parse().ok()).unwrap_or(items),
            "--bytes" => bytes = args.next().and_then(|v| v.parse().ok()).unwrap_or(bytes),
            "--quota-bytes" => quota = args.next().and_then(|v| v.parse().ok()).unwrap_or(quota),
            "--stdin-passphrase" => stdin_passphrase = true,
            other => {
                eprintln!("phase8-fixture: unexpected argument {other}");
                std::process::exit(2);
            }
        }
    }
    let Some(directory) = directory else {
        eprintln!("phase8-fixture: --data-dir is required");
        std::process::exit(2);
    };
    if !directory.is_absolute() {
        eprintln!("phase8-fixture: --data-dir must be absolute");
        std::process::exit(2);
    }
    if items == 0 || bytes < 64 || quota == 0 {
        eprintln!("phase8-fixture: invalid item/byte/quota limits");
        std::process::exit(2);
    }
    let passphrase = read_passphrase(stdin_passphrase);
    let limits = CaptureLimits {
        max_vault_bytes: quota,
        ..CaptureLimits::default()
    };
    DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&directory)
        .unwrap_or_else(|error| fail(&format!("directory: {error}")));
    let mut vault = Vault::at(directory.join("snippy"), limits)
        .unwrap_or_else(|error| fail(&error.to_string()));
    let pass = Passphrase::new(passphrase.clone()).unwrap_or_else(|error| fail(&error.to_string()));
    let confirm = Passphrase::new(passphrase).unwrap_or_else(|error| fail(&error.to_string()));
    vault
        .create(&pass, &confirm)
        .unwrap_or_else(|error| fail(&error.to_string()));
    let source = SourceIdentity::Observed {
        app_id: "fixture:phase8".into(),
        display_name: "Phase 8 synthetic source".into(),
    };
    let padding = "synthetic phase 8 measurement ".repeat(bytes / 30 + 1);
    for index in 0..items {
        let text = format!(
            "Synthetic phase 8 item {index:04} {}\n{}",
            if index % 5 == 0 { "even" } else { "odd" },
            &padding[..bytes]
        );
        let payload =
            Payload::text(&text, vault.limits()).unwrap_or_else(|error| fail(&error.to_string()));
        let saved = vault
            .save(&source, &payload)
            .unwrap_or_else(|error| fail(&error.to_string()));
        let tag = if index % 2 == 0 { "even" } else { "odd" };
        vault
            .set_tags(saved.id, &["phase8", tag])
            .unwrap_or_else(|error| fail(&error.to_string()));
    }
    let usage = vault
        .usage()
        .unwrap_or_else(|error| fail(&error.to_string()));
    let physical = vault
        .physical_bytes()
        .unwrap_or_else(|error| fail(&error.to_string()));
    println!(
        "Created synthetic phase 8 vault: items={} logical_bytes={} physical_bytes={} quota_bytes={}",
        usage.items, usage.logical_bytes, physical, quota
    );
    vault
        .lock()
        .unwrap_or_else(|error| fail(&error.to_string()));
}

fn read_passphrase(from_stdin: bool) -> String {
    if from_stdin {
        let mut value = String::new();
        io::stdin()
            .read_to_string(&mut value)
            .unwrap_or_else(|error| fail(&format!("stdin: {error}")));
        value.lines().next().unwrap_or_default().to_owned()
    } else {
        "synthetic-phase8-fixture-key".to_owned()
    }
}

fn fail(message: &str) -> ! {
    eprintln!("phase8-fixture: {message}");
    std::process::exit(1);
}
