//! The master key file and the cipher over credential payloads
//! (Requirement 4 of the phase 2 spec).

use std::path::Path;

use mezame::store::crypto::{open, seal, CryptoError, KeyError, MasterKey, KEY_LEN, NONCE_LEN};
use tempfile::TempDir;

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn the_key_is_created_once_with_owner_only_mode_and_read_back_unchanged() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("master.key");
    let first = MasterKey::load_or_create(&path).unwrap();
    let on_disk = std::fs::read(&path).unwrap();
    assert_eq!(on_disk.len(), KEY_LEN);
    assert_eq!(&on_disk[..], first.bytes_for_test());
    #[cfg(unix)]
    assert_eq!(mode(&path), 0o600);
    let second = MasterKey::load_or_create(&path).unwrap();
    assert_eq!(
        second.bytes_for_test(),
        first.bytes_for_test(),
        "never replaced"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        on_disk,
        "the file is untouched"
    );
    let loaded = MasterKey::load(&path).unwrap();
    assert_eq!(loaded.bytes_for_test(), first.bytes_for_test());
}

#[test]
fn a_pre_placed_key_is_returned_unchanged() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("master.key");
    let bytes = [7u8; KEY_LEN];
    std::fs::write(&path, bytes).unwrap();
    #[cfg(unix)]
    set_mode(&path, 0o600);
    let key = MasterKey::load_or_create(&path).unwrap();
    assert_eq!(key.bytes_for_test(), &bytes);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[test]
fn a_file_of_another_length_is_refused_naming_the_path() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("master.key");
    std::fs::write(&path, [1u8; 31]).unwrap();
    #[cfg(unix)]
    set_mode(&path, 0o600);
    let err = MasterKey::load_or_create(&path).unwrap_err();
    assert!(
        matches!(err, KeyError::WrongLength { len: 31, .. }),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(text.contains("master.key"), "{text}");
    assert!(text.contains("exactly 32"), "{text}");
    assert_eq!(std::fs::read(&path).unwrap().len(), 31, "not replaced");
}

#[cfg(unix)]
#[test]
fn a_key_readable_by_others_is_refused_naming_the_mode() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("master.key");
    std::fs::write(&path, [1u8; KEY_LEN]).unwrap();
    for loose in [0o640, 0o644, 0o604] {
        set_mode(&path, loose);
        let err = MasterKey::load(&path).unwrap_err();
        assert!(matches!(err, KeyError::Loose { .. }), "{loose:o}: {err:?}");
        let text = err.to_string();
        assert!(text.contains("0600"), "{text}");
        assert!(text.contains("chmod 600"), "{text}");
        let err = MasterKey::load_or_create(&path).unwrap_err();
        assert!(
            matches!(err, KeyError::Loose { .. }),
            "create finds it and refuses too"
        );
    }
    set_mode(&path, 0o600);
    assert!(MasterKey::load(&path).is_ok());
}

#[cfg(unix)]
#[test]
fn a_symlink_at_the_path_is_refused_and_not_followed() {
    let tmp = TempDir::new().unwrap();
    let target = tmp.path().join("elsewhere");
    std::fs::write(&target, [2u8; KEY_LEN]).unwrap();
    set_mode(&target, 0o600);
    let path = tmp.path().join("master.key");
    std::os::unix::fs::symlink(&target, &path).unwrap();
    let err = MasterKey::load_or_create(&path).unwrap_err();
    assert!(matches!(err, KeyError::NotRegular(_)), "{err:?}");
    assert_eq!(
        std::fs::read(&target).unwrap(),
        [2u8; KEY_LEN],
        "the target was not written through"
    );
}

#[test]
fn eight_threads_creating_one_key_all_read_the_same_bytes() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("master.key");
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || *MasterKey::load_or_create(&path).unwrap().bytes_for_test())
        })
        .collect();
    let keys: Vec<[u8; KEY_LEN]> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let on_disk = std::fs::read(&path).unwrap();
    for key in &keys {
        assert_eq!(
            &key[..],
            &on_disk[..],
            "every creator holds the key on disk"
        );
    }
}

#[test]
fn load_on_a_missing_file_is_its_own_variant() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("master.key");
    let err = MasterKey::load(&path).unwrap_err();
    assert!(matches!(err, KeyError::NotFound(_)), "{err:?}");
    assert!(!path.exists(), "load creates nothing");
}

#[test]
fn derivation_is_deterministic_and_the_two_keys_differ() {
    let master = MasterKey::from_bytes_for_test([9u8; KEY_LEN]);
    let a = master.keys();
    let b = master.keys();
    assert_eq!(a.credential, b.credential);
    assert_eq!(a.cookie, b.cookie);
    assert_ne!(a.credential, a.cookie);
    assert_ne!(a.credential, [9u8; KEY_LEN], "not the master key itself");
    let other = MasterKey::from_bytes_for_test([10u8; KEY_LEN]).keys();
    assert_ne!(other.credential, a.credential);
    assert_eq!(format!("{a:?}"), "Keys(..)", "no key bytes in Debug");
}

#[test]
fn a_payload_round_trips_and_every_tamper_fails() {
    let keys = MasterKey::from_bytes_for_test([3u8; KEY_LEN]).keys();
    let payload = br#"{"region":"us-east-1","profile":"work"}"#;
    let (nonce, ciphertext) = seal(&keys.credential, "row-1", payload);
    assert_eq!(nonce.len(), NONCE_LEN);
    assert_ne!(&ciphertext[..], &payload[..]);
    assert_eq!(
        open(&keys.credential, "row-1", &nonce, &ciphertext).unwrap(),
        payload
    );
    // Moved to another row.
    assert_eq!(
        open(&keys.credential, "row-2", &nonce, &ciphertext),
        Err(CryptoError::Tampered)
    );
    // A changed nonce.
    let mut other_nonce = nonce.clone();
    other_nonce[0] ^= 1;
    assert_eq!(
        open(&keys.credential, "row-1", &other_nonce, &ciphertext),
        Err(CryptoError::Tampered)
    );
    // A changed byte, anywhere.
    for i in [0, ciphertext.len() / 2, ciphertext.len() - 1] {
        let mut changed = ciphertext.clone();
        changed[i] ^= 0x80;
        assert_eq!(
            open(&keys.credential, "row-1", &nonce, &changed),
            Err(CryptoError::Tampered),
            "byte {i}"
        );
    }
    // Another key.
    assert_eq!(
        open(&keys.cookie, "row-1", &nonce, &ciphertext),
        Err(CryptoError::Tampered)
    );
    // A nonce of the wrong length.
    assert_eq!(
        open(&keys.credential, "row-1", &nonce[..12], &ciphertext),
        Err(CryptoError::Tampered)
    );
    assert!(!CryptoError::Tampered.to_string().is_empty());
}

#[test]
fn two_seals_of_one_payload_differ() {
    let keys = MasterKey::from_bytes_for_test([4u8; KEY_LEN]).keys();
    let (n1, c1) = seal(&keys.credential, "row", b"same");
    let (n2, c2) = seal(&keys.credential, "row", b"same");
    assert_ne!(n1, n2, "a fresh nonce per write");
    assert_ne!(c1, c2);
}
