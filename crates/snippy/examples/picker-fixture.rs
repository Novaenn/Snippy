//! Create a fresh encrypted synthetic review vault; never reads CLIPBOARD.
use clap::Parser;
use snippy::storage::{Passphrase, Payload, Vault};
use snippy_core::{config::CaptureLimits, source::SourceIdentity};
use std::{
    io::{self, BufRead},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::PathBuf,
};
use zeroize::Zeroizing;
#[derive(Parser)]
#[command(about = "Create a fresh encrypted vault of synthetic picker samples and tags")]
struct Args {
    #[arg(long)]
    data_dir: PathBuf,
    /// Read two passphrase lines from stdin instead of a controlling terminal (isolated fixtures)
    #[arg(long, hide = true)]
    stdin_passphrase: bool,
}
fn main() {
    if run(Args::parse()).is_err() {
        eprintln!(
            "picker-fixture: creation failed; use a fresh absolute private data directory and matching nonempty passphrases"
        );
        std::process::exit(1);
    }
}
fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    if !args.data_dir.is_absolute() {
        return Err("absolute directory required".into());
    }
    match std::fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&args.data_dir)
    {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    let metadata = std::fs::symlink_metadata(&args.data_dir)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err("private data directory required".into());
    }
    let (pass, confirm) = if args.stdin_passphrase {
        let mut input = io::stdin().lock();
        let mut pass = Zeroizing::new(String::new());
        let mut confirm = Zeroizing::new(String::new());
        // A bounded reader also keeps malformed fixture input from growing forever.
        {
            use std::io::Read;
            input.by_ref().take(1026).read_line(&mut pass)?;
            input.by_ref().take(1026).read_line(&mut confirm)?;
        }
        for value in [&mut pass, &mut confirm] {
            if value.ends_with('\n') {
                value.pop();
                if value.ends_with('\r') {
                    value.pop();
                }
            }
        }
        (
            Passphrase::new(std::mem::take(&mut *pass))?,
            Passphrase::new(std::mem::take(&mut *confirm))?,
        )
    } else {
        (
            Passphrase::new(rpassword::prompt_password("Review passphrase: ")?)?,
            Passphrase::new(rpassword::prompt_password("Confirm review passphrase: ")?)?,
        )
    };
    let limits = CaptureLimits::default();
    let mut vault = Vault::at(args.data_dir.join("snippy"), limits.clone())?;
    vault.create(&pass, &confirm)?;
    for n in 0..24 {
        let source = if n % 2 == 0 {
            SourceIdentity::Observed {
                app_id: "fixture:editor".into(),
                display_name: "Synthetic Editor".into(),
            }
        } else {
            SourceIdentity::Manual {
                label: "Manual fixture".into(),
            }
        };
        let saved = vault.save(
            &source,
            &Payload::text(
                &format!("Synthetic note {n:02}: café 日本語\nSecond line kept exactly."),
                &limits,
            )?,
        )?;
        vault.set_tags(
            saved.id,
            if n % 2 == 0 {
                &["demo", "even"]
            } else {
                &["demo", "odd"]
            },
        )?;
    }
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, 2, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()?
            .write_image_data(&[255, 0, 0, 255, 0, 128, 255, 128])?;
    }
    let saved = vault.save(
        &SourceIdentity::Observed {
            app_id: "fixture:image".into(),
            display_name: "Synthetic Image".into(),
        },
        &Payload::png(&png, &limits)?,
    )?;
    vault.set_tags(saved.id, &["demo", "image"])?;
    vault.lock()?;
    println!(
        "Created a fresh encrypted synthetic picker fixture (25 items). Start the daemon with this data directory."
    );
    Ok(())
}
