//! Temporary metadata-only development view; production GUI uses IPC in phase 4.

use clap::Parser;
use futures_channel::mpsc;
use futures_util::StreamExt;
use gtk::{gio, glib, prelude::*};
use snippy::x11::{Action, BindingStatus, Observer, Snapshot, Update};
use snippy_core::{config::ShortcutConfig, source::SourceIdentity};
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(
    about = "Display current X11 source metadata without reading or saving clipboard content"
)]
struct Args {
    #[arg(long)]
    no_shortcuts: bool,
    #[arg(long, default_value = "Ctrl+Alt+G")]
    save_shortcut: String,
    #[arg(long, default_value = "Ctrl+Alt+V")]
    picker_shortcut: String,
}

#[derive(Default)]
struct View {
    snapshot: Option<Snapshot>,
    bindings: Vec<BindingStatus>,
    ownership_updates: u64,
    save_activations: u64,
    picker_activations: u64,
    error: Option<String>,
}

fn main() -> glib::ExitCode {
    let args = Args::parse();
    if gtk::init().is_err() {
        eprintln!("x11-probe: could not initialize GTK on a graphical X11 session");
        return glib::ExitCode::FAILURE;
    }
    let config = ShortcutConfig {
        enabled: !args.no_shortcuts,
        save: args.save_shortcut,
        picker: args.picker_shortcut,
    };
    let application = gtk::Application::builder()
        .application_id("org.snippy.X11Probe")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.connect_activate(move |application| {
        if let Some(window) = application.active_window() {
            window.present();
            return;
        }
        let source = gtk::Label::new(Some("Connecting to X11…"));
        source.set_wrap(true);
        source.set_selectable(false);
        let counters = gtk::Label::new(None);
        let bindings = gtk::Label::new(None);
        bindings.set_wrap(true);
        let note = gtk::Label::new(Some(
            "Copy in the source app first (terminals usually use Ctrl+Shift+C).\nSave/picker shortcuts only count presses; nothing is saved yet.",
        ));
        note.set_wrap(true);
        let close = gtk::Button::with_label("Close");
        let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
        for widget in [
            source.upcast_ref::<gtk::Widget>(),
            counters.upcast_ref(),
            bindings.upcast_ref(),
            note.upcast_ref(),
            close.upcast_ref(),
        ] {
            content.append(widget);
        }
        content.set_margin_top(24);
        content.set_margin_bottom(24);
        content.set_margin_start(24);
        content.set_margin_end(24);
        let window = gtk::ApplicationWindow::builder()
            .application(application)
            .title("Snippy — X11 metadata probe")
            .default_width(560)
            .default_height(340)
            .child(&content)
            .build();
        let weak_window = window.downgrade();
        close.connect_clicked(move |_| {
            if let Some(window) = weak_window.upgrade() {
                window.close();
            }
        });

        let current = Arc::new(Mutex::new(View::default()));
        let (mut sender, mut receiver) = mpsc::channel::<()>(1);
        let worker_view = Arc::clone(&current);
        let worker_config = config.clone();
        std::thread::spawn(move || {
            let mut observer = match Observer::connect(&worker_config) {
                Ok(observer) => observer,
                Err(error) => {
                    worker_view.lock().expect("probe view lock").error = Some(error.to_string());
                    let _ = sender.try_send(());
                    return;
                }
            };
            {
                let mut view = worker_view.lock().expect("probe view lock");
                view.snapshot = Some(observer.snapshot().clone());
                view.bindings = observer.bindings().to_vec();
            }
            let _ = sender.try_send(());
            loop {
                let update = observer.wait(); // No timer and no clipboard polling.
                let mut view = worker_view.lock().expect("probe view lock");
                match update {
                    Ok(Update::Ownership) => {
                        view.snapshot = Some(observer.snapshot().clone());
                        view.ownership_updates = view.ownership_updates.saturating_add(1);
                    }
                    Ok(Update::Shortcut(Action::Save)) => {
                        view.save_activations = view.save_activations.saturating_add(1)
                    }
                    Ok(Update::Shortcut(Action::Picker)) => {
                        view.picker_activations = view.picker_activations.saturating_add(1)
                    }
                    Ok(Update::Bindings) => view.bindings = observer.bindings().to_vec(),
                    Ok(Update::Other) => continue,
                    Err(error) => {
                        view.error = Some(error.to_string());
                        drop(view);
                        let _ = sender.try_send(());
                        break;
                    }
                }
                drop(view);
                // Signals carry no metadata. The shared slot always contains the
                // latest view even when multiple signals are coalesced.
                if sender.try_send(()).is_err() && sender.is_closed() {
                    break;
                }
            }
        });
        glib::spawn_future_local(async move {
            while receiver.next().await.is_some() {
                let view = current.lock().expect("probe view lock");
                if let Some(error) = &view.error {
                    source.set_text(error);
                } else if let Some(snapshot) = &view.snapshot {
                    let name = match &snapshot.source {
                        SourceIdentity::Observed { display_name, .. } => display_name.as_str(),
                        SourceIdentity::Manual { label } => label.as_str(),
                        SourceIdentity::Unknown => snapshot
                            .unknown_reason
                            .map(|r| r.description())
                            .unwrap_or("Unknown"),
                    };
                    // Plain text, not markup, and only the current source is shown.
                    source.set_text(&format!(
                        "Source: {name}\nOwner: {:#x} · Generation: {}",
                        snapshot.owner, snapshot.generation
                    ));
                }
                counters.set_text(&format!(
                    "Ownership updates: {} · Save presses: {} · Picker presses: {}",
                    view.ownership_updates, view.save_activations, view.picker_activations
                ));
                bindings.set_text(
                    &view
                        .bindings
                        .iter()
                        .map(|b| format!("{:?}: {:?}", b.action, b.state))
                        .collect::<Vec<_>>()
                        .join(" · "),
                );
            }
        });
        window.present();
    });
    // Clap consumed development options; do not pass them to GTK's parser.
    application.run_with_args(&["x11-probe"])
}
