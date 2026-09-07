//! The two helpers every file under `~/.mezame` is written through:
//! `ensure_private_dir` and `write_private_atomic` (Requirement 15
//! criterion 9 and Requirement 14 criterion 4, as amended 2026-09-07). The
//! mode assertions are Unix-only; the helpers compile everywhere.
//!
//! Two cases chmod a directory to `0500` to force a write failure. Root
//! ignores modes, so those return early when the test runs as root, which
//! the CI runners do not. Root is detected by who owns a directory the
//! test just created, not by the `USER` variable, which `docker run` and
//! `su` leave unset.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Barrier};

use mezame::config::{ensure_private_dir, temp_sibling, write_private_atomic};
use tempfile::TempDir;

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("the path exists")
        .permissions()
        .mode()
        & 0o777
}

/// Whether this process is root, read from the owner of a directory the
/// test itself just created.
fn running_as_root(owned_by_us: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(owned_by_us).is_ok_and(|m| m.uid() == 0)
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// The temporary siblings left in `dir`, if any.
fn temps_in(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read the directory")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect()
}

#[test]
fn ensure_private_dir_creates_an_owner_only_directory() {
    // Requirement 15 criterion 9: the directory is created before the
    // config is written, and (amended) owner-only.
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".mezame");
    ensure_private_dir(&dir).expect("create");
    assert!(dir.is_dir());
    assert_eq!(mode(&dir), 0o700, "owner-only");
}

#[test]
fn ensure_private_dir_creates_missing_parents_owner_only() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("a").join("b").join(".mezame");
    ensure_private_dir(&dir).expect("create");
    for created in [tmp.path().join("a"), tmp.path().join("a").join("b"), dir] {
        assert_eq!(mode(&created), 0o700, "{}", created.display());
    }
}

#[test]
fn ensure_private_dir_leaves_an_existing_directory_mode_alone() {
    // A directory a 0.13.x release created stays as its owner left it;
    // nothing chmods on start. The fixture sets 0755 explicitly so the
    // case asserts something under every umask.
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join(".mezame");
    std::fs::create_dir(&dir).unwrap();
    set_mode(&dir, 0o755);
    ensure_private_dir(&dir).expect("an existing directory is fine");
    assert_eq!(mode(&dir), 0o755, "the existing mode is kept");
}

#[test]
fn ensure_private_dir_fails_when_the_path_is_a_file() {
    // Requirement 14 criterion 10's pre-condition: a regular file where
    // the directory should be is an error, so `PUT /state` answers 500.
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join(".mezame");
    std::fs::write(&path, b"not a directory").unwrap();
    assert!(ensure_private_dir(&path).is_err());
}

#[test]
fn write_private_atomic_writes_an_owner_only_file_with_the_given_bytes() {
    let tmp = TempDir::new().unwrap();
    for (name, durable) in [("durable.json", true), ("plain.json", false)] {
        let target = tmp.path().join(name);
        write_private_atomic(&target, b"{\"a\":1}", durable).expect("write");
        assert_eq!(std::fs::read(&target).unwrap(), b"{\"a\":1}");
        assert_eq!(mode(&target), 0o600, "owner-only: {name}");
    }
    assert!(temps_in(tmp.path()).is_empty(), "no sibling is left behind");
}

#[test]
fn write_private_atomic_replaces_a_symlink_instead_of_following_it() {
    // The target is never opened for writing: a symlink planted at the
    // path is replaced by the rename, and the file it pointed at is
    // untouched.
    let tmp = TempDir::new().unwrap();
    let outside = tmp.path().join("outside.json");
    std::fs::write(&outside, b"B").unwrap();
    let target = tmp.path().join("state.json");
    std::os::unix::fs::symlink(&outside, &target).unwrap();

    write_private_atomic(&target, b"A", false).expect("write");
    assert_eq!(
        std::fs::read(&outside).unwrap(),
        b"B",
        "the link's target is untouched"
    );
    let meta = std::fs::symlink_metadata(&target).unwrap();
    assert!(
        meta.file_type().is_file(),
        "the symlink was replaced by a file"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"A");
}

#[test]
fn write_private_atomic_leaves_the_target_alone_and_no_temp_on_failure() {
    // Requirement 14 criterion 10: a failed write leaves the existing file
    // as it was. The parent is made unwritable so the sibling cannot be
    // created.
    let tmp = TempDir::new().unwrap();
    if running_as_root(tmp.path()) {
        return;
    }
    let dir = tmp.path().join(".mezame");
    std::fs::create_dir(&dir).unwrap();
    let target = dir.join("state.json");
    std::fs::write(&target, b"B").unwrap();
    set_mode(&dir, 0o500);

    let outcome = write_private_atomic(&target, b"A", false);
    set_mode(&dir, 0o700);
    assert!(
        outcome.is_err(),
        "the write fails in an unwritable directory"
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"B",
        "the target is as it was"
    );
    assert!(temps_in(&dir).is_empty(), "no sibling is left behind");
}

#[test]
fn temp_sibling_paths_are_unique_and_sit_beside_the_target() {
    let target = Path::new("/some/dir/state.json");
    let mut seen = std::collections::HashSet::new();
    for _ in 0..1000 {
        let sibling = temp_sibling(target).expect("a sibling");
        assert_eq!(sibling.parent(), target.parent(), "same directory");
        let name = sibling.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(".state.json."), "{name}");
        assert!(name.ends_with(".tmp"), "{name}");
        assert!(seen.insert(name.clone()), "a sibling name repeated: {name}");
    }
}

#[test]
fn concurrent_writers_never_share_a_sibling() {
    // Eight threads write the same target as fast as they can from behind
    // one barrier. Each write gets its own sibling, so every write
    // succeeds and the file is always one whole document. With one fixed
    // sibling name the `O_EXCL` open fails with `AlreadyExists` under this
    // contention, and without `O_EXCL` a torn file could be renamed into
    // place, which is the race the review reproduced against `PUT /state`.
    let tmp = TempDir::new().unwrap();
    let target = Arc::new(tmp.path().join("state.json"));
    let barrier = Arc::new(Barrier::new(8));
    let writers: Vec<_> = (0..8u8)
        .map(|writer| {
            let target = Arc::clone(&target);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let payload = vec![b'a' + writer; 64 * 1024];
                barrier.wait();
                for _ in 0..100 {
                    write_private_atomic(&target, &payload, false)?;
                }
                Ok::<(), std::io::Error>(())
            })
        })
        .collect();
    for writer in writers {
        writer
            .join()
            .expect("a writer thread panicked")
            .expect("every write succeeds");
    }

    let bytes = std::fs::read(&*target).unwrap();
    assert_eq!(bytes.len(), 64 * 1024, "the file is one whole write");
    assert!(
        bytes.iter().all(|b| *b == bytes[0]),
        "the file holds one writer's payload, not a mix"
    );
    assert!(temps_in(tmp.path()).is_empty(), "no sibling is left behind");
}
