#![cfg(not(feature = "sqlcipher"))]

use snippy::storage::{Passphrase, StorageError, Vault};
use snippy_core::config::CaptureLimits;

#[test]
fn ordinary_sqlite_is_rejected_before_creating_a_vault_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("must-not-exist");
    let mut vault = Vault::at(&directory, CaptureLimits::default()).unwrap();
    let passphrase = Passphrase::new("synthetic only".into()).unwrap();
    assert_eq!(
        vault.create(&passphrase, &passphrase),
        Err(StorageError::EncryptionUnavailable)
    );
    assert!(!directory.exists());
}
