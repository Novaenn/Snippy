#![cfg(feature = "sqlcipher")]

use std::{
    fs,
    io::Cursor,
    os::unix::fs::{PermissionsExt, symlink},
};

use snippy::storage::{BrowseQuery, ContentKind, Passphrase, Payload, StorageError, Vault};
use snippy_core::{config::CaptureLimits, source::SourceIdentity};
use tempfile::TempDir;

fn key() -> Passphrase {
    Passphrase::new("Synthetic fixture key ' 日本語".into()).unwrap()
}
fn source(name: &str) -> SourceIdentity {
    SourceIdentity::Observed {
        app_id: format!("fixture:{name}"),
        display_name: name.into(),
    }
}
fn fixture(limits: CaptureLimits) -> (TempDir, Vault) {
    let temp = tempfile::tempdir().unwrap();
    let mut vault = Vault::at(temp.path().join("vault"), limits).unwrap();
    vault.create(&key(), &key()).unwrap();
    (temp, vault)
}
fn text(value: &str) -> Payload {
    Payload::text(value, &CaptureLimits::default()).unwrap()
}
fn png(color: png::ColorType, depth: png::BitDepth, pixels: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
        encoder.set_color(color);
        encoder.set_depth(depth);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(pixels)
            .unwrap();
    }
    bytes
}
fn pixels(bytes: &[u8]) -> (png::BitDepth, Vec<u8>) {
    let mut reader = png::Decoder::new(Cursor::new(bytes)).read_info().unwrap();
    let mut out = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut out).unwrap();
    out.truncate(info.buffer_size());
    assert_eq!(info.color_type, png::ColorType::Rgba);
    (info.bit_depth, out)
}

#[test]
fn encrypted_reopen_unicode_lock_permissions_and_wrong_password() {
    let (_temp, mut vault) = fixture(CaptureLimits::default());
    let payload = text("  café\t日本語\n🦀\r\n");
    let saved = vault
        .save(&source("synthetic-editor-source-marker"), &payload)
        .unwrap();
    vault
        .set_tags(saved.id, &["日本語", "synthetic-private-tag-marker"])
        .unwrap();
    vault
        .set_sensitive_apps(&["fixture:private-source-marker"])
        .unwrap();
    let settings = vault.security_info().unwrap();
    assert!(settings.cipher_version.starts_with("4."));
    assert!(
        settings.kdf_iterations >= 256_000
            && settings.hmac
            && settings.memory_security
            && settings.fts5
            && settings.memory_temp_build
    );
    assert_eq!(settings.temp_store, 2);
    assert_eq!(settings.plaintext_header_bytes, 0);
    vault.integrity_check().unwrap();
    assert_eq!(
        fs::metadata(vault.path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(vault.path().parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    vault.lock().unwrap();
    assert_eq!(vault.get(saved.id).err(), Some(StorageError::Locked));
    assert_eq!(vault.usage(), Err(StorageError::Locked));
    let ciphertext = fs::read(vault.path()).unwrap();
    assert_ne!(&ciphertext[..16], b"SQLite format 3\0");
    for marker in [
        payload.bytes(),
        b"private-source-marker",
        b"synthetic-editor-source-marker",
        b"synthetic-private-tag-marker",
        b"Synthetic fixture key",
    ] {
        assert!(
            !ciphertext
                .windows(marker.len())
                .any(|window| window == marker)
        );
    }
    let mut reopened = Vault::at(vault.path().parent().unwrap(), CaptureLimits::default()).unwrap();
    assert_eq!(
        reopened.unlock(&Passphrase::new("wrong fixture key".into()).unwrap()),
        Err(StorageError::UnlockFailed)
    );
    assert!(!reopened.is_unlocked());
    assert_eq!(fs::read(vault.path()).unwrap(), ciphertext);
    reopened.unlock(&key()).unwrap();
    let loaded = reopened.get(saved.id).unwrap();
    assert_eq!(loaded.payload.bytes(), payload.bytes());
    assert!(loaded.summary.source == source("synthetic-editor-source-marker"));
    assert_eq!(
        loaded.summary.tags,
        vec!["synthetic-private-tag-marker", "日本語"]
    );
    assert_eq!(
        reopened.sensitive_apps().unwrap(),
        vec!["fixture:private-source-marker"]
    );
    reopened.integrity_check().unwrap();
}

#[test]
fn duplicates_preserve_tags_timestamps_and_distinct_source_provenance() {
    let (_temp, mut vault) = fixture(CaptureLimits::default());
    let payload = text("fixture repeated text");
    let first = vault.save(&source("One"), &payload).unwrap();
    vault
        .set_tags(first.id, &["kept", "kept", "  trimmed  "])
        .unwrap();
    let before = vault.get(first.id).unwrap().summary;
    let repeat = vault.save(&source("One"), &payload).unwrap();
    assert!(repeat.merged);
    assert_eq!(repeat.id, first.id);
    let after = vault.get(first.id).unwrap().summary;
    assert_eq!(before.first_saved_ms, after.first_saved_ms);
    assert!(after.last_saved_ms >= before.last_saved_ms);
    assert_eq!(after.tags, vec!["kept", "trimmed"]);
    for src in [
        source("Two"),
        SourceIdentity::Unknown,
        SourceIdentity::Manual {
            label: "One".into(),
        },
    ] {
        let saved = vault.save(&src, &payload).unwrap();
        assert!(!saved.merged);
        assert!(vault.get(saved.id).unwrap().summary.source == src);
    }
    assert_eq!(vault.usage().unwrap().items, 4);
    assert_eq!(
        vault.usage().unwrap().logical_bytes,
        payload.stored_bytes() * 4
    );
    // Whitespace is content, not normalization noise.
    assert!(
        !vault
            .save(&source("One"), &text("fixture repeated text "))
            .unwrap()
            .merged
    );
}

#[test]
fn canonical_png_preserves_pixels_alpha_and_sixteen_bit_samples() {
    let limits = CaptureLimits::default();
    let (_temp, mut vault) = fixture(limits.clone());
    let rgb = png(
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &[1, 2, 3, 4, 5, 6],
    );
    let rgba = png(
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        &[1, 2, 3, 255, 4, 5, 6, 255],
    );
    let first = Payload::png(&rgb, &limits).unwrap();
    let equivalent = Payload::png(&rgba, &limits).unwrap();
    assert_eq!(first.bytes(), equivalent.bytes());
    let saved = vault.save(&source("Image"), &first).unwrap();
    assert!(vault.save(&source("Image"), &equivalent).unwrap().merged);
    let loaded = vault.get(saved.id).unwrap();
    assert_eq!(loaded.summary.kind, ContentKind::Png);
    assert_eq!(
        pixels(loaded.payload.bytes()).1,
        vec![1, 2, 3, 255, 4, 5, 6, 255]
    );
    assert_eq!(
        pixels(loaded.payload.thumbnail()).1,
        vec![1, 2, 3, 255, 4, 5, 6, 255]
    );
    assert_eq!(vault.usage().unwrap().logical_bytes, first.stored_bytes());
    let samples = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    let image = Payload::png(
        &png(png::ColorType::Rgba, png::BitDepth::Sixteen, &samples),
        &limits,
    )
    .unwrap();
    let saved = vault.save(&source("Image"), &image).unwrap();
    let result = pixels(vault.get(saved.id).unwrap().payload.bytes());
    assert_eq!(result.0, png::BitDepth::Sixteen);
    assert_eq!(result.1, samples);
}

#[test]
fn quota_is_transactional_duplicates_at_capacity_and_delete_reclaims_usage() {
    let limits = CaptureLimits {
        max_vault_bytes: 8,
        ..CaptureLimits::default()
    };
    let (_temp, mut vault) = fixture(limits);
    let saved = vault.save(&source("App"), &text("12345678")).unwrap();
    vault.set_tags(saved.id, &["preserved"]).unwrap();
    assert_eq!(
        vault.save(&source("Rejected"), &text("x")).err(),
        Some(StorageError::QuotaExceeded)
    );
    assert_eq!(vault.usage().unwrap().logical_bytes, 8);
    assert!(
        vault
            .save(&source("App"), &text("12345678"))
            .unwrap()
            .merged
    );
    assert_eq!(vault.get(saved.id).unwrap().summary.tags, vec!["preserved"]);
    vault.delete(saved.id).unwrap();
    assert_eq!(vault.usage().unwrap().logical_bytes, 0);
    assert!(vault.search("preserved", 100, 0).unwrap().is_empty());
    assert!(vault.search("12345678", 100, 0).unwrap().is_empty());
    vault.save(&source("App"), &text("restored")).unwrap();
}

#[test]
fn bounded_search_tag_edits_and_deletion_are_consistent() {
    let (_temp, mut vault) = fixture(CaptureLimits::default());
    let a = vault
        .save(&source("Editor"), &text("café needle sample"))
        .unwrap();
    let b = vault
        .save(&source("Browser"), &text("different sample"))
        .unwrap();
    vault.set_tags(a.id, &["project-red"]).unwrap();
    for query in ["needle", "Editor", "project-red", "café"] {
        assert_eq!(vault.search(query, 10, 0).unwrap()[0].id, a.id);
    }
    assert_eq!(vault.list(1, 0).unwrap().len(), 1);
    assert_eq!(vault.list(1, 1).unwrap().len(), 1);
    assert!(vault.list(1, 2).unwrap().is_empty());
    assert_eq!(
        vault.list(101, 0).err(),
        Some(StorageError::InvalidMetadata)
    );
    assert!(vault.search("\" OR 1=1 --", 10, 0).unwrap().is_empty());
    vault.set_tags(a.id, &["new-tag"]).unwrap();
    assert!(vault.search("project-red", 10, 0).unwrap().is_empty());
    vault.delete(a.id).unwrap();
    assert_eq!(vault.get(a.id).err(), Some(StorageError::NotFound));
    assert_eq!(vault.list(10, 0).unwrap()[0].id, b.id);
    assert_eq!(
        vault.set_tags(a.id, &["missing"]),
        Err(StorageError::NotFound)
    );
    vault.integrity_check().unwrap();
}

#[test]
fn malformed_and_oversized_images_text_and_metadata_are_rejected() {
    let mut limits = CaptureLimits::default();
    assert_eq!(
        Payload::text("", &limits).err(),
        Some(StorageError::InvalidContent)
    );
    assert_eq!(
        Payload::png(b"not a PNG", &limits).err(),
        Some(StorageError::InvalidContent)
    );
    let image = png(png::ColorType::Rgba, png::BitDepth::Eight, &[0; 8]);
    limits.max_image_pixels = 1;
    assert_eq!(
        Payload::png(&image, &limits).err(),
        Some(StorageError::ClipTooLarge)
    );
    limits.max_image_pixels = 16_000_000;
    limits.max_decoded_image_bytes = 7;
    assert_eq!(
        Payload::png(&image, &limits).err(),
        Some(StorageError::ClipTooLarge)
    );
    limits.max_clip_bytes = 2;
    assert_eq!(
        Payload::text("123", &limits).err(),
        Some(StorageError::ClipTooLarge)
    );
    let (_temp, mut vault) = fixture(CaptureLimits::default());
    assert_eq!(
        vault
            .save(
                &SourceIdentity::Manual { label: "\n".into() },
                &text("fixture")
            )
            .err(),
        Some(StorageError::InvalidMetadata)
    );
    assert_eq!(
        vault.set_sensitive_apps(&["\0"]),
        Err(StorageError::InvalidMetadata)
    );
}

#[test]
fn creation_is_exclusive_and_private_and_foreign_files_are_never_reset() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("new");
    let mut vault = Vault::at(&directory, CaptureLimits::default()).unwrap();
    let other = Passphrase::new("different".into()).unwrap();
    assert_eq!(
        vault.create(&key(), &other),
        Err(StorageError::PassphraseMismatch)
    );
    assert!(!directory.exists());
    vault.create(&key(), &key()).unwrap();
    vault.lock().unwrap();
    let before = fs::read(vault.path()).unwrap();
    assert_eq!(
        vault.create(&key(), &key()),
        Err(StorageError::AlreadyExists)
    );
    assert_eq!(fs::read(vault.path()).unwrap(), before);
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(vault.unlock(&key()), Err(StorageError::UnsafePath));
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let linked = temp.path().join("linked");
    symlink(&directory, &linked).unwrap();
    let mut linked = Vault::at(linked, CaptureLimits::default()).unwrap();
    assert_eq!(linked.unlock(&key()), Err(StorageError::UnsafePath));
    fs::write(
        vault.path(),
        b"synthetic foreign database: do not overwrite",
    )
    .unwrap();
    let before = fs::read(vault.path()).unwrap();
    assert!(vault.unlock(&key()).is_err());
    assert_eq!(fs::read(vault.path()).unwrap(), before);
    assert_eq!(
        Passphrase::new(String::new()).err(),
        Some(StorageError::InvalidPassphrase)
    );
    assert_eq!(
        Passphrase::new("bad\0key".into()).err(),
        Some(StorageError::InvalidPassphrase)
    );
}

#[test]
fn revoked_captures_roll_back_source_index_payload_and_duplicate_timestamp() {
    let (_temp, mut vault) = fixture(CaptureLimits::default());
    let payload = text("synthetic canceled capture marker");
    assert_eq!(
        vault.save_guarded(&source("canceled-source"), &payload, || false),
        Err(StorageError::Canceled)
    );
    assert_eq!(vault.usage().unwrap().items, 0);
    assert!(vault.search("marker", 10, 0).unwrap().is_empty());
    let id = vault.save(&source("accepted"), &payload).unwrap().id;
    vault.set_tags(id, &["preserved"]).unwrap();
    let before = vault.get(id).unwrap().summary;
    assert_eq!(
        vault.save_guarded(&source("accepted"), &payload, || false),
        Err(StorageError::Canceled)
    );
    let after = vault.get(id).unwrap().summary;
    assert_eq!(before.last_saved_ms, after.last_saved_ms);
    assert_eq!(after.tags, ["preserved"]);
    vault.lock().unwrap();
    vault.unlock(&key()).unwrap();
    assert_eq!(vault.usage().unwrap().items, 1);
}
#[test]
fn sensitive_rules_match_identities_case_insensitively_and_never_manual_labels() {
    let (_temp, mut vault) = fixture(CaptureLimits::default());
    for class in [
        "KeePassXC",
        "keepassxc",
        "Bitwarden",
        "bitwarden",
        "1Password",
        "1password",
    ] {
        assert!(
            vault
                .is_sensitive(&SourceIdentity::Observed {
                    app_id: format!("x11:wm-class:{class}"),
                    display_name: "innocent display label".into()
                })
                .unwrap()
        );
    }
    assert!(
        !vault
            .is_sensitive(&SourceIdentity::Manual {
                label: "KeePassXC".into()
            })
            .unwrap()
    );
    assert!(!vault.is_sensitive(&SourceIdentity::Unknown).unwrap());
    vault.set_sensitive_apps(&["fixture:custom"]).unwrap();
    assert!(vault.is_sensitive(&source("custom")).unwrap());
    assert!(!vault.is_sensitive(&source("other")).unwrap());
}

#[test]
fn picker_queries_combine_literal_search_filters_and_bounded_recent_pages() {
    use snippy::storage::{BrowseQuery, PAGE_SIZE, TEXT_PREVIEW_BYTES};
    let (_dir, mut vault) = fixture(CaptureLimits::default());
    let mut ids = Vec::new();
    for n in 0..24 {
        let source = if n % 2 == 0 {
            source("editor")
        } else {
            SourceIdentity::Manual {
                label: "manual".into(),
            }
        };
        let saved = vault
            .save(
                &source,
                &text(&format!("needle {n}: {}", "日本語".repeat(100))),
            )
            .unwrap();
        if n % 2 == 0 {
            vault.set_tags(saved.id, &["even", "demo"]).unwrap();
        }
        ids.push(saved.id);
    }
    let first = vault.browse(&BrowseQuery::default()).unwrap();
    assert_eq!(first.rows.len(), PAGE_SIZE);
    assert!(first.more);
    assert_eq!(first.rows[0].id, *ids.last().unwrap());
    assert!(
        first
            .rows
            .iter()
            .all(|row| row.text.len() <= TEXT_PREVIEW_BYTES
                && std::str::from_utf8(row.text.as_bytes()).is_ok())
    );
    let second = vault
        .browse(&BrowseQuery {
            offset: 10,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(second.rows.len(), 10);
    assert!(second.more);
    let third = vault
        .browse(&BrowseQuery {
            offset: 20,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(third.rows.len(), 4);
    assert!(!third.more);
    assert!(
        first
            .rows
            .iter()
            .all(|a| second.rows.iter().all(|b| a.id != b.id))
    );
    let filtered = vault
        .browse(&BrowseQuery {
            text: "needle".into(),
            source: "fixture:editor".into(),
            tag: "even".into(),
            kind: Some(ContentKind::Text),
            offset: 0,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(filtered.rows.len(), 10);
    assert!(filtered.more);
    assert!(filtered.rows.iter().all(|r| r.tags == ["demo", "even"]));
    for query in [
        BrowseQuery {
            text: "needle OR nonexistent".into(),
            ..Default::default()
        },
        BrowseQuery {
            tag: "EVEN".into(),
            ..Default::default()
        },
        BrowseQuery {
            source: "' OR 1=1 --".into(),
            ..Default::default()
        },
        BrowseQuery {
            kind: Some(ContentKind::Png),
            ..Default::default()
        },
    ] {
        assert!(vault.browse(&query).unwrap().rows.is_empty());
    }
    assert_eq!(
        vault
            .browse(&BrowseQuery {
                offset: u64::MAX,
                ..Default::default()
            })
            .err(),
        Some(StorageError::InvalidMetadata)
    );
    assert_eq!(
        vault
            .browse(&BrowseQuery {
                text: "x".repeat(1025),
                ..Default::default()
            })
            .err(),
        Some(StorageError::InvalidMetadata)
    );
}
#[test]
fn picker_preview_is_bounded_and_reuse_preserves_provenance_tags_and_timestamps() {
    use snippy::storage::BrowseQuery;
    let (_dir, mut vault) = fixture(CaptureLimits::default());
    let image = png(
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        &[11, 22, 33, 44, 55, 66, 77, 88],
    );
    let id = vault
        .save(
            &source("original"),
            &Payload::png(&image, &CaptureLimits::default()).unwrap(),
        )
        .unwrap()
        .id;
    vault.set_tags(id, &["preserved"]).unwrap();
    let original = vault.get(id).unwrap();
    let usage = vault.usage().unwrap();
    let preview = vault.image_preview(id).unwrap();
    assert_eq!((preview.width, preview.height), (2, 1));
    assert_eq!(&*preview.rgba, &[11, 22, 33, 44, 55, 66, 77, 88]);
    let content = vault.clipboard_content(id).unwrap();
    assert_eq!(&*content.bytes, original.payload.bytes());
    assert_eq!(content.kind, ContentKind::Png);
    let after = vault.get(id).unwrap();
    assert!(after.summary.source == original.summary.source);
    assert_eq!(after.summary.tags, ["preserved"]);
    assert_eq!(after.summary.last_saved_ms, original.summary.last_saved_ms);
    assert_eq!(vault.usage().unwrap(), usage);
    let page = vault.browse(&BrowseQuery::default()).unwrap();
    assert!(page.rows[0].text.is_empty());
    vault.lock().unwrap();
    assert_eq!(
        vault.browse(&BrowseQuery::default()).err(),
        Some(StorageError::Locked)
    );
    assert_eq!(vault.image_preview(id).err(), Some(StorageError::Locked));
    assert_eq!(
        vault.clipboard_content(id).err(),
        Some(StorageError::Locked)
    );
    assert_eq!(&*content.bytes, original.payload.bytes());
}

#[test]
fn picker_finds_the_exact_unicode_review_note_with_a_zero_prefixed_number() {
    use snippy::storage::BrowseQuery;
    let (_dir, mut vault) = fixture(CaptureLimits::default());
    let payload = text("Synthetic note 07: café 日本語\nSecond line kept exactly.");
    let saved = vault
        .save(
            &SourceIdentity::Manual {
                label: "Manual fixture".into(),
            },
            &payload,
        )
        .unwrap();
    vault.set_tags(saved.id, &["demo", "odd"]).unwrap();
    for query in ["Synthetic note 07", "07", "café 日本語"] {
        let page = vault
            .browse(&BrowseQuery {
                text: query.into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].id, saved.id);
        assert_eq!(page.rows[0].text.as_bytes(), payload.bytes());
        assert_eq!(page.rows[0].tags, ["demo", "odd"]);
    }
}

#[test]
fn manager_tag_edits_bulk_deletion_facets_and_physical_size_are_consistent() {
    let (_temp, mut vault) = fixture(CaptureLimits::default());
    let long = "Unicode text ".repeat(100);
    let first = vault.save(&source("editor"), &text(&long)).unwrap().id;
    let second = vault
        .save(&source("editor"), &text("second note"))
        .unwrap()
        .id;
    let third = vault
        .save(&source("browser"), &text("third note"))
        .unwrap()
        .id;
    vault.set_tags(first, &["demo", "even"]).unwrap();

    let facets = vault.facets().unwrap();
    assert_eq!(facets.sources.len(), 2);
    assert_eq!(facets.tags.len(), 2);
    assert!(facets.sources.iter().any(|source| source.clips == 2));
    let editor = facets
        .sources
        .iter()
        .find(|source| matches!(&source.source, SourceIdentity::Observed { app_id, .. } if app_id == "fixture:editor"))
        .unwrap()
        .id;
    let page = vault
        .browse(&BrowseQuery {
            source_id: Some(editor),
            full: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.rows.len(), 2);
    assert!(page.rows.iter().all(|row| row.source_id == editor));
    let full = page.rows.iter().find(|row| row.id == first).unwrap();
    assert_eq!(full.text.as_str(), long);
    assert!(full.text.len() > 384);
    assert_eq!(full.first_saved_ms, full.last_saved_ms);
    assert_eq!(full.tags, ["demo", "even"]);

    let tags = vault.edit_tag(first, "review", true).unwrap();
    assert_eq!(tags, ["demo", "even", "review"]);
    let tags = vault.edit_tag(first, "demo", false).unwrap();
    assert_eq!(tags, ["even", "review"]);
    assert_eq!(
        vault.edit_tag(999, "missing", true).err(),
        Some(StorageError::NotFound)
    );

    let physical = vault.physical_bytes().unwrap();
    assert!(physical > 0);
    assert_eq!(vault.usage().unwrap().items, 3);
    assert_eq!(vault.delete_many(&[first, second, 999]).unwrap(), 2);
    assert_eq!(vault.usage().unwrap().items, 1);
    assert_eq!(vault.facets().unwrap().sources.len(), 1);
    assert_eq!(
        vault.delete_many(&[]).err(),
        Some(StorageError::InvalidMetadata)
    );

    let lower = CaptureLimits {
        max_vault_bytes: 1,
        ..CaptureLimits::default()
    };
    vault.set_limits(lower).unwrap();
    assert_eq!(
        vault.save(&source("editor"), &text("rejected")).err(),
        Some(StorageError::QuotaExceeded)
    );
    vault.set_limits(CaptureLimits::default()).unwrap();
    assert!(vault.save(&source("editor"), &text("allowed")).is_ok());
    assert_eq!(vault.get(third).unwrap().summary.id, third);
}

#[test]
fn lowering_the_clip_limit_keeps_larger_history_reusable_and_only_blocks_new_saves() {
    let (_temp, mut vault) = fixture(CaptureLimits::default());
    let large = "synthetic history clip ".repeat(4096);
    let id = vault.save(&source("editor"), &text(&large)).unwrap().id;
    let lower = CaptureLimits {
        max_clip_bytes: 1024,
        ..CaptureLimits::default()
    };
    vault.set_limits(lower.clone()).unwrap();
    for _ in 0..2 {
        let content = vault.clipboard_content(id).unwrap();
        assert_eq!(content.kind, ContentKind::Text);
        assert_eq!(&*content.bytes, large.as_bytes());
        assert_eq!(vault.get(id).unwrap().payload.bytes(), large.as_bytes());
        assert_eq!(
            vault
                .save(&source("editor"), &text(&"new ".repeat(512)))
                .err(),
            Some(StorageError::ClipTooLarge)
        );
        // The lower limit also holds for a vault unlocked under it.
        vault.lock().unwrap();
        vault = Vault::at(vault.path().parent().unwrap(), lower.clone()).unwrap();
        vault.unlock(&key()).unwrap();
    }
    assert!(vault.save(&source("editor"), &text("small")).is_ok());
}

#[test]
fn sensitive_rule_edits_are_case_insensitive() {
    let (_temp, mut vault) = fixture(CaptureLimits::default());
    let seeded = vault.sensitive_apps().unwrap();
    assert!(seeded.iter().any(|id| id == "x11:wm-class:KeePassXC"));
    let after = vault
        .edit_sensitive("x11:wm-class:bitwarden", true)
        .unwrap();
    assert_eq!(after, seeded);
    let after = vault
        .edit_sensitive("x11:wm-class:keepassxc", false)
        .unwrap();
    assert!(
        !after
            .iter()
            .any(|id| id.eq_ignore_ascii_case("x11:wm-class:keepassxc"))
    );
    assert_eq!(after.len(), seeded.len() - 1);
    assert!(
        !vault
            .is_sensitive(&SourceIdentity::Observed {
                app_id: "x11:wm-class:KeePassXC".into(),
                display_name: "fixture".into(),
            })
            .unwrap()
    );
    vault
        .set_sensitive_apps(&["fixture:Custom", "fixture:custom"])
        .unwrap();
    assert_eq!(vault.sensitive_apps().unwrap(), ["fixture:Custom"]);
}

#[test]
fn source_filter_and_search_ignore_case_but_tag_filter_stays_exact() {
    let (_dir, mut vault) = fixture(CaptureLimits::default());
    let vscode = SourceIdentity::Observed {
        app_id: "x11:wm-class:Code".into(),
        display_name: "VSCode".into(),
    };
    let saved = vault
        .save(&vscode, &text("synthetic editor snippet"))
        .unwrap();
    vault.set_tags(saved.id, &["Work"]).unwrap();
    vault
        .save(&source("terminal"), &text("synthetic shell line"))
        .unwrap();
    let ids = |query: BrowseQuery| -> Vec<i64> {
        vault
            .browse(&query)
            .unwrap()
            .rows
            .iter()
            .map(|row| row.id)
            .collect()
    };
    // Source filter: identity or label substring, any letter case.
    for filter in [
        "vscode",
        "VSCODE",
        "VsCo",
        "wm-class:code",
        "X11:WM-CLASS:CODE",
    ] {
        let found = ids(BrowseQuery {
            source: filter.into(),
            ..Default::default()
        });
        assert_eq!(found, [saved.id], "{filter}");
    }
    // Main search matches source labels and tag names regardless of case too.
    for text in ["vscode", "work"] {
        let found = ids(BrowseQuery {
            text: text.into(),
            ..Default::default()
        });
        assert_eq!(found, [saved.id], "{text}");
    }
    // The exact tag filter remains case-sensitive.
    assert!(
        ids(BrowseQuery {
            tag: "work".into(),
            ..Default::default()
        })
        .is_empty()
    );
    assert_eq!(
        ids(BrowseQuery {
            tag: "Work".into(),
            ..Default::default()
        }),
        [saved.id]
    );
}
