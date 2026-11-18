//! Manual, synthetic offers. This tool replaces CLIPBOARD only when launched.
#[path = "../tests/support/owner.rs"]
mod owner;
use clap::{Parser, ValueEnum};
use std::io::{self, BufRead};
#[derive(Parser)]
#[command(
    about = "Offer synthetic nonsecret text/PNG on CLIPBOARD; each input line recopies, EOF exits"
)]
struct Args {
    #[arg(long, value_enum, default_value = "editor")]
    source: Source,
    #[arg(long, value_enum, default_value = "text")]
    format: Format,
    #[arg(long, default_value_t=64, value_parser=clap::value_parser!(u64).range(0..=26_214_401))]
    bytes: u64,
    #[arg(long)]
    incr: bool,
}
#[derive(Clone, ValueEnum)]
enum Source {
    Editor,
    Unknown,
    Sensitive,
}
#[derive(Clone, ValueEnum)]
enum Format {
    Text,
    Png,
    Both,
    Unsupported,
    Malformed,
}
fn main() {
    let args = Args::parse();
    let image = if matches!(args.format, Format::Png | Format::Both) {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[255, 0, 0, 255, 0, 128, 255, 128])
                .unwrap();
        }
        Some(bytes)
    } else if matches!(args.format, Format::Malformed) {
        Some(b"synthetic invalid PNG".to_vec())
    } else {
        None
    };
    let owner = owner::Owner::new(owner::Offers {
        class: match args.source {
            Source::Editor => Some("SnippySyntheticEditor".into()),
            Source::Sensitive => Some("KeePassXC".into()),
            Source::Unknown => None,
        },
        text: vec![b'S'; args.bytes as usize],
        png: image,
        unsupported: matches!(args.format, Format::Unsupported),
        incr: args.incr,
        ..Default::default()
    });
    // These fields support protocol assertions in the shared fixture.
    let _ = (owner.window, &owner.requests, &owner.payloads);
    println!(
        "Synthetic clipboard offered. Leave this process running; Enter recopies, Ctrl+D exits."
    );
    for line in io::stdin().lock().lines() {
        if line.is_err() {
            break;
        }
        owner.copy_again();
    }
}
