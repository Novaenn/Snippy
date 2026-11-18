//! Resource measurement helper: aggregate counters only, never source labels.
use clap::Parser;
use snippy::x11::{Observer, Update};
use snippy_core::config::ShortcutConfig;
use std::{
    fs,
    io::{self, Write},
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u64).range(1..=60))]
    seconds: u64,
}

fn cpu_ticks() -> Option<u64> {
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    let fields = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    Some(fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?)
}

fn memory_kib(field: &str) -> Option<u64> {
    fs::read_to_string("/proc/self/smaps_rollup")
        .ok()?
        .lines()
        .find(|line| line.starts_with(field))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn main() -> ExitCode {
    let args = Args::parse();
    let mut observer = match Observer::connect(&ShortcutConfig {
        enabled: false,
        ..ShortcutConfig::default()
    }) {
        Ok(observer) => observer,
        Err(error) => {
            eprintln!("x11-meter: {error}");
            return ExitCode::FAILURE;
        }
    };
    let updates = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicBool::new(false));
    let worker_updates = Arc::clone(&updates);
    let worker_failed = Arc::clone(&failed);
    std::thread::spawn(move || {
        loop {
            match observer.wait() {
                Ok(Update::Ownership) => {
                    worker_updates.fetch_add(1, Ordering::Relaxed);
                }
                Ok(_) => (),
                Err(_) => {
                    worker_failed.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    });
    let Some(before) = cpu_ticks() else {
        eprintln!("x11-meter: procfs CPU counters unavailable");
        return ExitCode::FAILURE;
    };
    let start = Instant::now();
    println!("READY");
    if io::stdout().flush().is_err() {
        return ExitCode::FAILURE;
    }
    // The worker blocks on X11 events. This main-thread timer is only the
    // measurement deadline, not an ownership polling interval.
    std::thread::sleep(Duration::from_secs(args.seconds));
    let (Some(after), Some(rss), Some(pss)) = (cpu_ticks(), memory_kib("Rss:"), memory_kib("Pss:"))
    else {
        eprintln!("x11-meter: procfs resource counters unavailable");
        return ExitCode::FAILURE;
    };
    if failed.load(Ordering::Relaxed) {
        eprintln!("x11-meter: observer disconnected");
        return ExitCode::FAILURE;
    }
    println!(
        "elapsed_ms={} cpu_ticks={} rss_kib={rss} pss_kib={pss} ownership_updates={}",
        start.elapsed().as_millis(),
        after.saturating_sub(before),
        updates.load(Ordering::Relaxed)
    );
    ExitCode::SUCCESS
}
