//! Synthetic ownership changes for isolated measurements. No clipboard payload.
use clap::Parser;
use std::{process::ExitCode, time::Duration};
use x11rb::{
    CURRENT_TIME,
    connection::Connection,
    protocol::xproto::{AtomEnum, ConnectionExt as _, CreateWindowAux, PropMode, WindowClass},
    wrapper::ConnectionExt as _,
};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=100_000))]
    changes: u32,
}

fn run(changes: u32) -> Result<(), Box<dyn std::error::Error>> {
    let (connection, screen) = x11rb::connect(None)?;
    let root = connection.setup().roots[screen].root;
    let clipboard = connection.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
    let mut windows = Vec::new();
    for _ in 0..2 {
        let window = connection.generate_id()?;
        connection
            .create_window(
                0,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new(),
            )?
            .check()?;
        connection
            .change_property8(
                PropMode::REPLACE,
                window,
                AtomEnum::WM_CLASS,
                AtomEnum::STRING,
                b"fixture\0SyntheticChurn\0",
            )?
            .check()?;
        windows.push(window);
    }
    for index in 0..changes {
        connection
            .set_selection_owner(windows[index as usize % 2], clipboard, CURRENT_TIME)?
            .check()?;
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

fn main() -> ExitCode {
    if std::env::var("SNIPPY_TEST_X11").as_deref() != Ok("1") {
        eprintln!("x11-churn: run only through the isolated benchmark script");
        return ExitCode::FAILURE;
    }
    match run(Args::parse().changes) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => {
            eprintln!("x11-churn: synthetic X11 setup failed");
            ExitCode::FAILURE
        }
    }
}
