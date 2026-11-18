//! Private-display clipboard readback assertions. Never prints clipboard bytes.
use clap::{ArgGroup, Parser};
use snippy::{
    storage::Payload,
    x11::{Observer, capture::Transfer},
};
use snippy_core::config::{CaptureLimits, ShortcutConfig};
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
#[derive(Parser)]
#[command(group(ArgGroup::new("expected").required(true).args(["image","note"])))]
struct Args {
    #[arg(long)]
    image: bool,
    #[arg(long,value_parser=clap::value_parser!(u8).range(0..=23))]
    note: Option<u8>,
}
fn main() {
    if run(Args::parse()).is_err() {
        eprintln!("picker-read: synthetic clipboard assertion failed");
        std::process::exit(1);
    }
    println!("PASS: served synthetic clipboard matches the expected fixture");
}
fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("SNIPPY_TEST_X11").as_deref() != Ok("1") {
        return Err("private display required".into());
    }
    let observer = Observer::connect(&ShortcutConfig {
        enabled: false,
        ..Default::default()
    })?;
    let mut transfer = Transfer::start(
        observer.snapshot().clone(),
        CaptureLimits::default(),
        Arc::new(AtomicBool::new(false)),
    )?;
    let received = transfer.result.recv_timeout(Duration::from_secs(8))??;
    transfer.finish();
    if args.image {
        if !received.png {
            return Err("expected image".into());
        }
        let payload = Payload::png(&received.bytes, &CaptureLimits::default())?;
        let mut reader = png::Decoder::new(std::io::Cursor::new(payload.bytes())).read_info()?;
        if reader.info().width != 2 || reader.info().height != 1 {
            return Err("image dimensions".into());
        }
        let mut pixels = [0; 8];
        reader.next_frame(&mut pixels)?;
        if pixels != [255, 0, 0, 255, 0, 128, 255, 128] {
            return Err("image samples".into());
        }
    } else {
        let expected = format!(
            "Synthetic note {:02}: café 日本語\nSecond line kept exactly.",
            args.note.ok_or("expected note")?
        );
        if received.png || received.bytes.as_slice() != expected.as_bytes() {
            return Err("text mismatch".into());
        }
    }
    Ok(())
}
