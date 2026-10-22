use kio_core::scope::Repository;

#[test]
fn documented_embedding_external_send_values_are_accepted_without_rewriting_other_config() {
    for policy in ["text_only", "text_and_images"] {
        let root = tempfile::tempdir().unwrap();
        let repo = Repository::init(root.path()).unwrap();
        let original = repo.read_config_document().unwrap();
        let replacement = format!("{original}\n[embedding]\nexternal_send = \"{policy}\"\n");
        repo.compare_replace_config_document(&original, &replacement)
            .expect("03 §11 permits this explicit image egress policy");
        assert_eq!(repo.read_config_document().unwrap(), replacement);
        assert_eq!(
            repo.image_external_send_allowed().unwrap(),
            policy == "text_and_images"
        );
    }
}

#[test]
fn unknown_embedding_external_send_policy_is_rejected_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    let repo = Repository::init(root.path()).unwrap();
    let original = repo.read_config_document().unwrap();
    assert!(repo.image_external_send_allowed().unwrap());
    let replacement = format!("{original}\n[embedding]\nexternal_send = \"anything\"\n");
    assert!(
        repo.compare_replace_config_document(&original, &replacement)
            .is_err()
    );
    assert_eq!(repo.read_config_document().unwrap(), original);
}
