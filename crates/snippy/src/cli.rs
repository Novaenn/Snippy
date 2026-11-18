use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "snippy",
    version,
    about = "Save selected clipboard items, organized by their source application",
    after_help = "Start 'snippy daemon' before opening 'snippy manager'.\nSave waits for unlock/confirmation and the final outcome. Picker opens saved history for manual reuse.\nMetadata-only X11 probe: cargo run -p snippy-gui --example x11-probe"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Run the initially locked service in the foreground
    Daemon {
        /// Inactivity lock timeout (development override; default 15 minutes)
        #[arg(long, value_parser=clap::value_parser!(u64).range(1..=86400))]
        lock_timeout_seconds: Option<u64>,
        /// Leave shortcut registration to your window manager
        #[arg(long)]
        no_shortcuts: bool,
        /// Isolated IPC fixtures only; disables all X11 integration
        #[arg(long, hide = true)]
        headless: bool,
        /// Maximum received/stored bytes per clip (development override)
        #[arg(long, value_parser=clap::value_parser!(u64).range(1..=26_214_400))]
        max_clip_bytes: Option<u64>,
        /// Logical vault quota in bytes (development override)
        #[arg(long, value_parser=clap::value_parser!(u64).range(1..=1_073_741_824))]
        max_vault_bytes: Option<u64>,
        /// Save shortcut (leave built-in shortcuts disabled when using WM bindings)
        #[arg(long)]
        save_shortcut: Option<String>,
        /// Picker shortcut (leave built-in shortcuts disabled when using WM bindings)
        #[arg(long)]
        picker_shortcut: Option<String>,
    },
    /// Save the current clipboard explicitly; wait for unlock/confirmation and commit
    Save,
    /// Open/focus searchable saved history; Enter copies an item for manual pasting
    Picker,
    /// Open/focus the full history manager with tags, deletion, usage, and settings
    Manager,
    /// Lock the vault and cancel pending unlock work
    Lock,
    /// Show service availability and lock state
    Status,
}
