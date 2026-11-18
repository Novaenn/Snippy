#!/bin/sh
# Display-independent storage checks; synthetic data in private temporary folders.
set -eu
cd "$(dirname "$0")/.."
cargo test -p snippy --locked --offline storage::
cargo test -p snippy --locked --offline --test storage
cargo test -p snippy --locked --offline --no-default-features --features plaintext-test-build --test storage_no_cipher
# Restore the runnable CLI with its production encryption feature after the fixture build.
cargo build -p snippy --locked --offline
cargo run -p snippy --locked --offline --example vault-demo
