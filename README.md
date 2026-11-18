<p align="center">
  <img src="packaging/snippy.svg" alt="Snippy clipboard mascot" width="112" height="112">
</p>

# Snippy

Snippy is an explicit-save clipboard manager for Linux X11. Ordinary copy
operations are never added to history: you decide when to keep something, and
Snippy stores it in an encrypted vault, organized by the application it came
from and by tags you assign.

Text and raster images stay local. There is no network access, telemetry, tray
icon, or required desktop environment integration.

> Snippy is pre-1.0 software. Interfaces and the vault schema may change between
> releases, so keep backups of any vault you care about.

## Features

- **Explicit Save only.** A shortcut, the CLI, or the window saves the current
  clipboard. Ordinary Copy, PRIMARY selection, opening history, and reusing an
  item never create history entries.
- **Encrypted vault.** Saved content, thumbnails, tags, source identities,
  sensitive-app rules, and the search index live in a SQLCipher database.
- **Source attribution.** Captures are attributed to the clipboard owner using
  X11 metadata, not to whichever application has focus when Save is pressed.
  Uncertain sources remain `Unknown` or use a clearly marked manual label.
- **Sensitive-source confirmation.** Recognized password managers and
  user-added sensitive identities require confirmation on every save.
- **Tags and search.** Literal text search, exact source-category and tag
  filters, type filters, pagination, and lazy image previews.
- **One history window.** Browse, search, copy, tag, delete, review usage, and
  edit settings in the same window.
- **Bounded resources.** Bounded IPC messages, transfers, image decoding,
  paginated queries, and lazy previews keep the vault out of a single large
  in-memory load.
- **Desktop-friendly operation.** No GNOME extension, systemd service, tray, or
  notification daemon is required. Autostart and notifications are opt-in.

## Support boundary

Snippy targets **native Linux X11 sessions**, including standalone window
managers. Wayland is not supported; access through XWayland alone does not
provide native Wayland support.

The application is designed for a trusted local user session. IPC uses private
paths and same-user peer checks, but processes running as the same user are not
treated as an isolation boundary.

## Requirements

- Linux with X11 and the XFixes extension
- Rust 1.89 or newer (edition 2024; `rust-toolchain.toml` selects stable)
- GTK 4.8 or newer, without Libadwaita
- A C build environment, pkg-config, X11 development headers, and OpenSSL
  development headers

On Ubuntu or Debian:

```sh
sudo apt install build-essential pkg-config libgtk-4-dev libx11-dev libssl-dev
rustup component add rustfmt clippy
```

For the isolated GUI and protocol checks, also install:

```sh
sudo apt install xvfb xdotool
```

A read-only prerequisite check is available:

```sh
sh scripts/check-prerequisites.sh
```

## Build and run

From the repository root:

```sh
cargo build --workspace --locked

# Terminal 1: start the resident service (it starts locked)
./target/debug/snippy daemon

# Terminal 2: open the history and settings window
./target/debug/snippy manager
```

Create a vault with a confirmed passphrase on first use. The CLI and GUI must
come from the same build because the local control protocol is versioned.

Useful commands:

```sh
cargo run --locked -p snippy -- --help
cargo run --locked -p snippy -- --version
cargo run --locked -p snippy -- save
```

With built-in shortcuts enabled (the default), <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>G</kbd>
requests Save and <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>V</kbd> opens the history
window. Start the daemon with `--no-shortcuts` to bind `snippy save`,
`snippy picker`, and `snippy manager` in your window manager instead.

## Install

```sh
./install.sh
```

This builds release binaries and installs, for the current user only:

- `snippy`, `snippy-gui`, and `snippy-launch` in `~/.local/bin` (or
  `$XDG_BIN_HOME`)
- the application icon in the hicolor theme
- `org.snippy.Snippy.desktop` in the application menu

The menu entry starts the service when needed, waits until it is ready, and
opens the history window. Installing does not create a vault, settings, or
autostart entry, and does not touch clipboard data.

Rerun `./install.sh` to update. `./uninstall.sh` removes installed programs,
the menu entry, the icon, and a Snippy-written autostart entry. Your vault and
settings are kept.

## CLI

| Command | Behavior |
| --- | --- |
| `snippy daemon` | Run the single-instance service in the foreground, initially locked |
| `snippy save` | Save the current clipboard; wait for unlock, confirmation, and the final result |
| `snippy picker` | Open the history window, placing it on the pointer's monitor when possible |
| `snippy manager` | Open the same history window |
| `snippy lock` | Lock the vault and clear browsing state |
| `snippy status` | Report service availability and lock state without clipboard content |

Save succeeds only after the encrypted transaction commits. A canceled,
changed, unsupported, or oversized clipboard produces a fixed error and no
partial history entry. The CLI itself never starts a daemon silently.

## History window

`snippy picker` and `snippy manager` open the same window. The picker command
adds pointer-monitor placement and attempts to restore the previous focus after
copying.

Keyboard summary:

| Keys | Action |
| --- | --- |
| <kbd>Enter</kbd> | Copy the selected item and close |
| <kbd>Ctrl</kbd>+<kbd>Enter</kbd> | Copy and keep the window open |
| <kbd>Ctrl</kbd>+<kbd>X</kbd> | Copy, delete the history entry after confirmation, keep the window open |
| <kbd>Ctrl</kbd>+<kbd>F</kbd> | Focus search |
| <kbd>Ctrl</kbd>+<kbd>T</kbd> | Focus tag entry |
| <kbd>Ctrl</kbd>+<kbd>D</kbd> | Delete the selected item |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>D</kbd> | Delete checked items on the current page |
| <kbd>Ctrl</kbd>+<kbd>K</kbd> | Check the current page |
| <kbd>Ctrl</kbd>+<kbd>,</kbd> | Open settings |
| <kbd>Escape</kbd> | Close without copying |

Search text is treated as a literal phrase, never as executable SQL or an FTS
expression. Tag filtering is exact and case-sensitive. Source and tag-name
substring matching ignores ASCII letter case.

## Files and configuration

| Purpose | Default location |
| --- | --- |
| Encrypted vault | `$XDG_DATA_HOME/snippy/vault.db` or `~/.local/share/snippy/vault.db` |
| Nonsecret settings | `$XDG_CONFIG_HOME/snippy/settings.conf` or `~/.config/snippy/settings.conf` |
| Runtime socket and lock | `$XDG_RUNTIME_DIR/snippy`, or a verified private temporary directory |
| Opt-in autostart entry | `$XDG_CONFIG_HOME/autostart/snippy.desktop` |

Settings include shortcut bindings, maximum clip and vault sizes, the inactivity
lock timeout, autostart, and notifications. Sensitive-app identities remain
encrypted inside the vault. Settings files and application directories use
user-only permissions; the settings file is replaced atomically.

Defaults include a 25 MiB maximum clip size, a 1 GiB logical vault quota, and a
15-minute inactivity lock. New saves are rejected at these limits; existing
history is never silently pruned. Startup flags are one-run overrides and are
not written to the settings file.

Autostart is off by default. Enabling it writes an entry with the current
absolute executable path, so re-enable it after moving the binary. Optional
notifications are off by default and use a content-free `notify-send` call when
available; the GUI and CLI remain the fallback.

## Security and privacy

- Clipboard ownership is observed through XFixes events. The GTK-free observer
  connection does not request text, image data, or TARGETS during ordinary
  copying. An open GTK window may enumerate advertised formats through the
  toolkit; Snippy's GUI does not fetch or persist clipboard payloads.
- Clipboard content is read only for an authorized Save or an explicit request
  to reuse a stored item. Selection identity is rechecked during capture.
- Saved content and private metadata are stored in SQLCipher with authenticated
  pages, memory-only temporary storage, and no plaintext thumbnails, indexes,
  exports, or logs. The application fails closed if encrypted storage is
  unavailable.
- Passphrases are not accepted as CLI arguments, stored in configuration, or
  written to logs. There is no keyring integration and no forgotten-passphrase
  recovery.
- On lock, database connections, previews, query results, and pending captures
  are released. The one clip already served on the system clipboard may remain
  available until another owner replaces it or the service exits; locking cannot
  retract copies another application has already received.
- Snippy makes no network requests and collects no telemetry.
- Complete process-memory erasure and forensic erasure of free database pages
  are not promised.

Use synthetic data for testing. Automated checks must never read your live
clipboard, real vault, or real source metadata.

## Limitations

- Native X11 only. Wayland and XWayland-only sessions are unsupported.
- Formats are limited to Unicode plain text and raster images normalized to
  PNG. Rich text, HTML bundles, SVG, animated PNG, file references, and OCR are
  not supported.
- Source attribution is best effort. Clipboard proxies, hidden helpers, and
  ambiguous client identities can leave the source `Unknown`.
- There is no forgotten-passphrase recovery, in-app vault reset, or automatic
  plaintext export. Never overwrite an unreadable vault.
- Deleting an item releases logical quota but does not guarantee immediate
  database-file shrinkage or forensic erasure.
- Bulk deletion applies to checked items on the current page.
- Behavior under real GNOME X11 and standalone window managers remains
  best-effort; the automated suite uses synthetic X11 clients and private Xvfb
  displays.
- No performance or latency guarantees are made. Release measurements are
  environment-specific observations.

## Development checks

Run the standard Rust checks for any change:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --locked
```

The complete automated suite adds storage, X11, capture, manager, picker, and GUI
checks on private authenticated Xvfb displays:

```sh
sh scripts/check-phase8.sh
```

All test scripts use isolated XDG paths, temporary encrypted vaults, and
synthetic clipboard owners. They never use your live clipboard, vault, or
runtime directory. Run them from a normal terminal if your environment restricts
Unix sockets or display creation.

## Architecture

| Component | Responsibility |
| --- | --- |
| `snippy-core` | Platform-independent settings, provenance, confirmation, and capture policy |
| `snippy` | CLI and resident service composition |
| `snippy::x11` | Event-driven ownership observation, shortcuts, authorized capture, bounded clipboard serving |
| `snippy::storage` | Verified SQLCipher lifecycle, versioned schema, bounded records, index, tags, quotas |
| `snippy::ipc` | Versioned, bounded private control protocol and same-user peer checks |
| `snippy::service` | Event loop, single-instance lock, vault lifecycle, storage worker, GUI routing |
| `snippy-gui` | GTK presentation for setup, unlock, confirmation, history, and settings |

The daemon owns the database, capture transactions, vault state, shortcuts,
and clipboard serving. The GUI communicates with it over private local IPC and
exits when its last window closes. Blocking database, passphrase, and image work
is isolated from the GTK and X11 event loops.

## Contributing

Behavior-focused tests are expected for capture authorization, attribution,
confirmations, limits, deduplication, encryption, lock transitions, IPC, and
clipboard ownership. Keep fixtures synthetic, run the checks above, and report
untested scenarios or display limitations instead of claiming they passed.
