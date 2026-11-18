#!/bin/sh
# Inspect prerequisites only: no downloads, package installs, or filesystem writes.
set -u

failed=0

for tool in rustc cargo cc make pkg-config; do
    if command -v "$tool" >/dev/null 2>&1; then
        printf 'OK: %s is available\n' "$tool"
    else
        printf 'MISSING: %s\n' "$tool"
        failed=1
    fi
done

if command -v cargo >/dev/null 2>&1; then
    if cargo fmt --version; then :; else failed=1; fi
    if cargo clippy --version; then :; else failed=1; fi
fi

if command -v pkg-config >/dev/null 2>&1; then
    if pkg-config --atleast-version=4.8 gtk4; then
        printf 'OK: GTK %s development files\n' "$(pkg-config --modversion gtk4)"
    else
        printf 'MISSING: GTK4 development files (version 4.8 or newer)\n'
        failed=1
    fi
    for package in x11 openssl; do
        if pkg-config --exists "$package"; then
            printf 'OK: %s %s development files\n' "$package" "$(pkg-config --modversion "$package")"
        else
            printf 'MISSING: %s development files\n' "$package"
            failed=1
        fi
    done
fi

for tool in Xvfb xdotool; do
    if command -v "$tool" >/dev/null 2>&1; then
        printf 'OK: %s for isolated GUI testing\n' "$tool"
    else
        printf 'NOTE: %s unavailable; isolated GUI smoke check needs it\n' "$tool"
    fi
done

exit "$failed"
