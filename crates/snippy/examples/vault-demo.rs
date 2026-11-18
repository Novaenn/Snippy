//! Synthetic fixtures only: no display, X11 connection, IPC, or clipboard access.
use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use snippy::storage::{Passphrase, Payload, StorageError, Vault};
use snippy_core::{config::CaptureLimits, source::SourceIdentity};

#[derive(Parser)]
#[command(
    about = "Exercise an encrypted vault using synthetic fixtures; never accesses the clipboard"
)]
struct Args {
    /// Prompt without echo for a new passphrase, confirmation, and later unlock.
    #[arg(long)]
    interactive: bool,
    /// Keep the encrypted fixture vault in this new absolute directory.
    /// Existing vaults are never replaced. Default: temporary, removed on exit.
    #[arg(long)]
    directory: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vault exercise: {error}");
            ExitCode::FAILURE
        }
    }
}

fn prompt(message: &str) -> Result<Passphrase, StorageError> {
    Passphrase::new(rpassword::prompt_password(message).map_err(|_| StorageError::Io)?)
}
fn synthetic_key() -> Result<Passphrase, StorageError> {
    Passphrase::new("fixed synthetic demo passphrase; not for real data".into())
}
fn app(name: &str) -> SourceIdentity {
    SourceIdentity::Observed {
        app_id: format!("fixture:{name}"),
        display_name: name.into(),
    }
}
fn require(value: bool) -> Result<(), StorageError> {
    if value {
        Ok(())
    } else {
        Err(StorageError::Integrity)
    }
}

fn run(args: Args) -> Result<(), StorageError> {
    let temporary = tempfile::tempdir().map_err(|_| StorageError::Io)?;
    let keep = args.directory.is_some();
    let directory = args
        .directory
        .unwrap_or_else(|| temporary.path().join("vault"));
    let limits = CaptureLimits {
        max_vault_bytes: 4096,
        ..CaptureLimits::default()
    };
    let mut vault = Vault::at(directory, limits.clone())?;
    let passphrase = if args.interactive {
        prompt("New fixture vault passphrase: ")?
    } else {
        synthetic_key()?
    };
    let confirmation = if args.interactive {
        prompt("Confirm passphrase: ")?
    } else {
        synthetic_key()?
    };
    vault.create(&passphrase, &confirmation)?;
    drop(confirmation);
    drop(passphrase);
    println!("PASS: private encrypted vault created with confirmed passphrase");
    let info = vault.security_info()?;
    println!(
        "SQLCipher {}; provider {} ({}); KDF iterations {}; memory-only temporary storage; encrypted FTS5",
        info.cipher_version,
        info.crypto_provider,
        info.crypto_provider_version,
        info.kdf_iterations
    );
    let text = Payload::text(
        "  Synthetic café / 日本語 / 🦀\n\twhitespace stays intact\r\n",
        &limits,
    )?;
    let a = vault.save(&app("Editor"), &text)?;
    vault.set_tags(a.id, &["fixture", "keep-this-tag"])?;
    let first = vault.get(a.id)?.summary.first_saved_ms;
    let merged = vault.save(&app("Editor"), &text)?;
    let loaded = vault.get(a.id)?;
    require(
        merged.merged
            && merged.id == a.id
            && loaded.summary.first_saved_ms == first
            && loaded.payload.bytes() == text.bytes()
            && loaded.summary.tags == ["fixture", "keep-this-tag"],
    )?;
    require(!vault.save(&app("Browser"), &text)?.merged)?;
    println!(
        "PASS: exact Unicode/whitespace round trip; same-source duplicate preserves tags; different source remains separate"
    );
    let mut png_bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_bytes, 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .map_err(|_| StorageError::InvalidContent)?
            .write_image_data(&[255, 0, 0, 128, 0, 255, 0, 255])
            .map_err(|_| StorageError::InvalidContent)?;
    }
    let image = Payload::png(&png_bytes, &limits)?;
    let image_id = vault.save(&app("Image"), &image)?.id;
    let loaded_image = vault.get(image_id)?;
    require(
        loaded_image.payload.bytes() == image.bytes()
            && !loaded_image.payload.thumbnail().is_empty(),
    )?;
    println!("PASS: canonical PNG and encrypted thumbnail round trip");
    let used = vault.usage()?.logical_bytes;
    let fill = Payload::text(
        &"x".repeat((limits.max_vault_bytes - used) as usize),
        &limits,
    )?;
    let fill_id = vault.save(&app("Filler"), &fill)?.id;
    require(vault.usage()?.logical_bytes == limits.max_vault_bytes)?;
    require(
        vault
            .save(&app("Overflow"), &Payload::text("overflow", &limits)?)
            .err()
            == Some(StorageError::QuotaExceeded),
    )?;
    require(vault.save(&app("Editor"), &text)?.merged)?;
    vault.delete(fill_id)?;
    require(vault.usage()?.logical_bytes == used && vault.usage()?.items == 3)?;
    println!(
        "PASS: 4096-byte quota rejects new content, allows duplicates, preserves history, and deletion frees usage"
    );
    require(vault.search("keep-this-tag", 10, 0)?.len() == 1)?;
    vault.integrity_check()?;
    // Release loaded plaintext before locking, as future service/UI callers must.
    drop(loaded);
    drop(loaded_image);
    drop(text);
    drop(image);
    drop(fill);
    vault.lock()?;
    require(vault.get(a.id).err() == Some(StorageError::Locked))?;
    require(
        vault
            .unlock(&Passphrase::new(
                "intentionally incorrect fixture passphrase".into(),
            )?)
            .err()
            == Some(StorageError::UnlockFailed),
    )?;
    println!("PASS: lock prevents reads; deliberately wrong passphrase is rejected without reset");
    let passphrase = if args.interactive {
        prompt("Unlock the fixture vault again: ")?
    } else {
        synthetic_key()?
    };
    vault.unlock(&passphrase)?;
    drop(passphrase);
    require(
        vault.usage()?.items == 3 && vault.get(a.id)?.summary.tags == ["fixture", "keep-this-tag"],
    )?;
    vault.integrity_check()?;
    vault.lock()?;
    println!(
        "PASS: correct passphrase reopens the existing records; cipher and database integrity checks pass"
    );
    if keep {
        println!(
            "Encrypted synthetic vault retained in the requested directory; existing vaults are never overwritten."
        );
    } else {
        println!("Temporary synthetic vault removed on exit.");
    }
    Ok(())
}
