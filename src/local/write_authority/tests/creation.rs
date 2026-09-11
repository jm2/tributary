//! Directory-creation regressions for the mounted write authority: the
//! identity recorded for a created component must be the one captured from
//! the creation product itself — under a private name, before the final
//! component name is exposed — so a concurrent writer's replacement of a
//! just-created directory is never recorded (and later removed) as
//! transfer-owned.
//!
//! The historical mkdir-then-open split left a window in which a
//! replacement could land between the creation and the identity capture;
//! the private-name discipline closes that window by construction (nothing
//! can replace a creation nobody can name). These tests pin the ownership
//! property the discipline guarantees: the recorded identity names the
//! created object, so a later replacement is refused fail-closed and the
//! newcomer survives.

use std::path::Path;

use super::authority;
use crate::local::write_authority::{ConflictPolicy, ReversalOutcome};

#[test]
fn created_directory_identity_refuses_a_later_replacement() {
    let root = tempfile::tempdir().expect("temporary root");
    let authority = authority(&root);

    let (_, created) = authority
        .create_relative_directory(Path::new("imported"), ConflictPolicy::Preserve)
        .expect("create imported directory");
    assert_eq!(created.len(), 1, "the leaf component was created");
    let entry = &created[0];
    assert_eq!(entry.relative_path, Path::new("imported"));
    let identity = entry.identity.expect("creation must bind an identity");

    // A concurrent writer replaces the just-created directory with a
    // pre-built newcomer holding foreign data. The recorded identity names
    // the object the creation call produced — not a post-hoc lookup of the
    // path — so the identity-verified reversal must refuse the newcomer.
    let newcomer = root.path().join("newcomer");
    std::fs::create_dir(&newcomer).expect("create newcomer directory");
    std::fs::write(newcomer.join("foreign.txt"), b"foreign").expect("write foreign entry");
    std::fs::remove_dir(root.path().join("imported")).expect("swap in the newcomer");
    std::fs::rename(&newcomer, root.path().join("imported")).expect("rename newcomer over");

    let outcome = authority
        .remove_relative_directory_verified(Path::new("imported"), Some(identity))
        .expect("a refusal is an outcome, not an error");
    assert_eq!(outcome, ReversalOutcome::RefusedForeignLeaf);
    assert_eq!(
        std::fs::read(root.path().join("imported/foreign.txt")).expect("read foreign"),
        b"foreign",
        "the replacement must survive the refused reversal untouched"
    );
}

#[test]
fn adopted_directory_is_never_reported_as_created() {
    let root = tempfile::tempdir().expect("temporary root");
    let authority = authority(&root);
    std::fs::create_dir(root.path().join("imported")).expect("pre-create imported");
    std::fs::write(root.path().join("imported/foreign.txt"), b"foreign")
        .expect("write foreign entry");

    let (_, created) = authority
        .create_relative_directory(Path::new("imported"), ConflictPolicy::Preserve)
        .expect("adopt the existing directory");
    assert!(
        created.is_empty(),
        "a pre-existing component is adopted, not owned: {created:?}"
    );
    assert_eq!(
        std::fs::read(root.path().join("imported/foreign.txt")).expect("read foreign"),
        b"foreign"
    );
}
