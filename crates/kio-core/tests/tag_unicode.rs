use std::fs;

use kio_core::{
    portable::{portable_tag_collision_key, portable_tag_leaf},
    scope::Repository,
};

#[test]
fn unicode_simple_fold_controls_tag_creation_resolution_and_deletion() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init(root.path()).unwrap();
    fs::write(root.path().join("note.md"), "tag target").unwrap();
    let head = repo
        .snapshot(Some("head"), None)
        .unwrap()
        .commit_hash
        .unwrap();

    for (original, alias) in [("Σ", "ς"), ("Ꭰ", "ꭰ"), ("ß", "ẞ"), ("Cafe\u{301}", "Café")]
    {
        assert_eq!(repo.tag(original, None).unwrap(), head);
        assert_eq!(repo.resolve_commit(alias).unwrap(), head);
        assert_eq!(
            repo.tag(alias, None).unwrap_err().error_code(),
            "KIO-E-COMMIT-TAG-001"
        );
        assert_eq!(repo.delete_tag(alias).unwrap(), head);
    }

    assert_ne!(
        portable_tag_collision_key("ß"),
        portable_tag_collision_key("ss")
    );
    assert_ne!(portable_tag_leaf("İ"), portable_tag_leaf("i"));
    assert_ne!(portable_tag_leaf("I"), portable_tag_leaf("ı"));
    assert_eq!(repo.tag("ß", None).unwrap(), head);
    assert_eq!(repo.tag("ss", None).unwrap(), head);
    assert_eq!(repo.tag("İ", None).unwrap(), head);
    assert_eq!(repo.tag("i", None).unwrap(), head);
}

#[test]
fn unassigned_unicode_is_rejected_for_tag_create_and_delete() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init(root.path()).unwrap();
    fs::write(root.path().join("note.md"), "tag target").unwrap();
    repo.snapshot(Some("head"), None).unwrap();
    let name = "future\u{0378}";
    assert_eq!(
        repo.tag(name, None).unwrap_err().error_code(),
        "KIO-E-CONFIG-USAGE-001"
    );
    assert_eq!(
        repo.delete_tag(name).unwrap_err().error_code(),
        "KIO-E-CONFIG-USAGE-001"
    );
    assert!(
        !repo
            .kio_dir()
            .join("refs/tags-v1")
            .join(portable_tag_leaf(name))
            .exists()
    );
}
