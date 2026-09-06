//! Tests for the mounted write authority.

use super::*;

fn unique_root(label: &str) -> PathBuf {
    tempfile::Builder::new()
        .prefix(&format!("tributary-write-authority-{label}-"))
        .tempdir()
        .expect("create temp root")
        .keep()
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
}

#[test]
fn fresh_write_commits_atomically() {
    let root = unique_root("fresh");
    let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");

    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Fail)
        .expect("prepare staged file");
    staged.write_all(b"audio payload").expect("write payload");

    let outcome = staged.commit().expect("commit staged file");
    assert_eq!(outcome.resolution, ConflictResolution::Fresh);
    assert_eq!(
        std::fs::read(root.join("song.flac")).expect("read final"),
        b"audio payload"
    );

    cleanup(&root);
}

#[test]
fn skip_policy_rejects_when_destination_exists() {
    let root = unique_root("skip");
    std::fs::write(root.join("song.flac"), b"existing").expect("write existing");

    let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");
    let error = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Skip)
        .expect_err("skip policy must reject existing destination");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

    cleanup(&root);
}

#[test]
fn overwrite_policy_replaces_final_file() {
    let root = unique_root("overwrite");
    std::fs::write(root.join("song.flac"), b"old").expect("write existing");

    let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");
    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Overwrite)
        .expect("prepare overwrite");
    staged.write_all(b"new").expect("write new");
    staged.commit().expect("commit overwrite");

    assert_eq!(
        std::fs::read(root.join("song.flac")).expect("read final"),
        b"new"
    );

    cleanup(&root);
}

#[test]
fn preserve_policy_writes_to_disambiguated_name() {
    let root = unique_root("preserve");
    std::fs::write(root.join("song.flac"), b"first").expect("write first");

    let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");
    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Preserve)
        .expect("prepare preserve");
    staged.write_all(b"second").expect("write second");
    let outcome = staged.commit().expect("commit preserve");

    assert_eq!(outcome.resolution, ConflictResolution::Preserved);
    assert_eq!(
        std::fs::read(root.join("song.flac")).expect("read original"),
        b"first"
    );
    assert_eq!(
        std::fs::read(root.join(&outcome.relative_path)).expect("read preserved"),
        b"second"
    );

    cleanup(&root);
}

#[test]
fn rollback_removes_staged_file() {
    let root = unique_root("rollback");
    let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");

    let mut staged = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Fail)
        .expect("prepare staged");
    staged.write_all(b"partial").expect("write partial");
    let staged_path = staged.staged_path.clone();
    // Sanity: the staged file actually exists before rollback.
    assert!(staged_path.exists());
    staged.rollback().expect("rollback staged");
    assert!(!staged_path.exists());
    assert!(!root.join("song.flac").exists());

    cleanup(&root);
}

#[test]
fn cross_mount_path_is_rejected() {
    let root = unique_root("cross-mount");
    let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");

    let error = authority
        .prepare_write_relative_file(Path::new("../outside.flac"), ConflictPolicy::Fail)
        .expect_err("parent path must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

    let error = authority
        .prepare_write_relative_file(Path::new("/etc/passwd"), ConflictPolicy::Fail)
        .expect_err("absolute path must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

    cleanup(&root);
}

#[test]
fn directory_creation_and_file_writes_combine() {
    let root = unique_root("dirs");
    let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");

    let bound = authority
        .create_relative_directory(Path::new("album"), ConflictPolicy::Fail)
        .expect("create album dir");
    assert_eq!(bound.relative_path(), Path::new("album"));

    let mut staged = bound
        .prepare_write_in_directory("song.flac", ConflictPolicy::Fail)
        .expect("prepare file under dir");
    staged.write_all(b"nested").expect("write nested");
    staged.commit().expect("commit nested");

    assert_eq!(
        std::fs::read(root.join("album/song.flac")).expect("read nested"),
        b"nested"
    );

    cleanup(&root);
}

#[test]
fn prepared_target_resolves_only_one_preserved_name() {
    let root = unique_root("preserve-twice");
    std::fs::write(root.join("song.flac"), b"original").expect("write original");

    let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");

    let mut first = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Preserve)
        .expect("prepare first preserve");
    first.write_all(b"a").expect("write first");
    let first_outcome = first.commit().expect("commit first");

    let mut second = authority
        .prepare_write_relative_file(Path::new("song.flac"), ConflictPolicy::Preserve)
        .expect("prepare second preserve");
    second.write_all(b"b").expect("write second");
    let second_outcome = second.commit().expect("commit second");

    assert_ne!(first_outcome.relative_path, second_outcome.relative_path);
    let names: Vec<String> = std::fs::read_dir(&root)
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    assert_eq!(names.len(), 3);
    assert!(names.iter().any(|name| name == "song.flac"));
    assert!(names.iter().any(|name| name == "song (1).flac"));
    assert!(names.iter().any(|name| name == "song (2).flac"));

    cleanup(&root);
}

#[test]
fn remove_relative_file_only_accepts_regular_files() {
    let root = unique_root("remove-file");
    std::fs::write(root.join("song.flac"), b"data").expect("write file");
    std::fs::create_dir(root.join("album")).expect("create album");

    let authority = MountedWriteAuthority::acquire(&root).expect("acquire write authority");
    authority
        .remove_relative_file(Path::new("song.flac"))
        .expect("remove file");
    assert!(!root.join("song.flac").exists());

    let error = authority
        .remove_relative_file(Path::new("album"))
        .expect_err("directory must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

    cleanup(&root);
}
