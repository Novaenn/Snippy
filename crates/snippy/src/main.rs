//! CLI/service composition. Save waits for its committed outcome; window
//! commands return once the service accepts the request.

mod cli;
mod error;

use std::process::ExitCode;

use clap::Parser;

use cli::{Cli, Command};
use error::AppError;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Only fixed, typed diagnostics; no payloads, source labels, or keys.
            eprintln!("snippy: {error}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(command: Command) -> Result<(), AppError> {
    match command {
        Command::Daemon {
            lock_timeout_seconds,
            no_shortcuts,
            headless,
            max_clip_bytes,
            max_vault_bytes,
            save_shortcut,
            picker_shortcut,
        } => {
            let store = snippy::settings::Store::discover()?;
            let base = store.load()?;
            let overrides = snippy::settings::Overrides::new(
                lock_timeout_seconds,
                no_shortcuts,
                max_clip_bytes,
                max_vault_bytes,
                picker_shortcut.map(Into::into),
                save_shortcut.map(Into::into),
            )?;
            snippy::service::run(snippy::service::Options {
                paths: snippy::ipc::Paths::discover()?,
                base,
                overrides,
                settings: store.path().to_owned(),
                x11: !headless,
            })?;
            Ok(())
        }
        Command::Save => {
            let mut stream = snippy::ipc::Paths::discover()?.connect()?;
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(130)))
                .map_err(|_| snippy::ipc::Error::Io)?;
            match snippy::ipc::exchange_reply(&mut stream, snippy::ipc::Request::Save)? {
                snippy::ipc::Reply::Saved { merged, .. } => println!(
                    "{}",
                    if merged {
                        "Saved: duplicate merged."
                    } else {
                        "Saved."
                    }
                ),
                snippy::ipc::Reply::AlreadySaved => println!("Already saved; history unchanged."),
                _ => return Err(snippy::ipc::Error::Protocol.into()),
            }
            Ok(())
        }
        Command::Picker => {
            snippy::ipc::call(
                &snippy::ipc::Paths::discover()?,
                snippy::ipc::Request::OpenPicker,
            )?;
            Ok(())
        }
        Command::Manager => {
            snippy::ipc::call(
                &snippy::ipc::Paths::discover()?,
                snippy::ipc::Request::OpenGui,
            )?;
            Ok(())
        }
        Command::Lock => {
            snippy::ipc::call(&snippy::ipc::Paths::discover()?, snippy::ipc::Request::Lock)?;
            Ok(())
        }
        Command::Status => {
            let state = snippy::ipc::call(
                &snippy::ipc::Paths::discover()?,
                snippy::ipc::Request::Status,
            )?;
            println!(
                "Service: running\nVault: {}\nSetup: {}",
                match state.phase {
                    snippy::ipc::VaultState::Locked => "locked",
                    snippy::ipc::VaultState::Unlocking => "unlocking",
                    snippy::ipc::VaultState::Unlocked => "unlocked",
                },
                if state.exists { "complete" } else { "required" }
            );
            Ok(())
        }
    }
}
