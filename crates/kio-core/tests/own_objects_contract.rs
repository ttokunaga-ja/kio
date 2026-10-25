//! W1/ST01: external storage permissions are not Kio admission authority.

#[cfg(unix)]
mod unix {
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};

    use kio_core::{
        management::ManagementBinding,
        private_fs::{
            read_private_file, read_private_file_at, verify_private_creation_parent,
            verify_private_directory,
        },
        scope::Repository,
    };

    fn fixture() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("kio-w1-own-objects-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap()
    }

    fn set_mode(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn private_directory_under_writable_external_parent_remains_usable_without_repair() {
        let temp = fixture();
        let external = temp.path().join("external");
        let own = external.join("private");
        fs::create_dir(&external).unwrap();
        fs::create_dir(&own).unwrap();
        set_mode(&external, 0o777);
        set_mode(&own, 0o700);

        let retained = verify_private_directory(&own).expect("only the Kio object must be private");
        assert_eq!(retained.path(), own);
        assert_eq!(
            mode(&external),
            0o777,
            "external permissions must not be repaired"
        );
        assert_eq!(mode(&own), 0o700);

        set_mode(&own, 0o755);
        assert!(
            verify_private_directory(&own).is_err(),
            "unsafe own object stays refused"
        );
        assert_eq!(
            mode(&own),
            0o755,
            "unsafe existing object must not be adopted by repair"
        );
    }

    #[test]
    fn external_creation_parent_can_be_writable_and_verification_creates_nothing() {
        let temp = fixture();
        let external = temp.path().join("external");
        fs::create_dir(&external).unwrap();
        set_mode(&external, 0o777);

        let retained = verify_private_creation_parent(&external)
            .expect("a writable external directory is a valid creation parent");
        assert_eq!(retained.path(), external);
        assert_eq!(mode(&external), 0o777);
        assert_eq!(fs::read_dir(&external).unwrap().count(), 0);
    }

    #[test]
    fn private_credential_in_external_directory_checks_the_leaf_without_repairing_parent() {
        let temp = fixture();
        let external = temp.path().join("external");
        let credential = external.join("tools.toml");
        fs::create_dir(&external).unwrap();
        fs::write(&credential, b"private credential fixture").unwrap();
        set_mode(&external, 0o777);
        set_mode(&credential, 0o600);

        assert_eq!(
            read_private_file(&credential, 1024).expect("private leaf in user-owned parent"),
            b"private credential fixture"
        );
        assert_eq!(mode(&external), 0o777);
        assert_eq!(mode(&credential), 0o600);

        set_mode(&credential, 0o644);
        assert!(read_private_file(&credential, 1024).is_err());
        assert_eq!(mode(&credential), 0o644);
    }

    #[test]
    fn managed_scope_root_permissions_can_change_without_invalidating_private_store() {
        let temp = fixture();
        let root = temp.path().join("scope");
        fs::create_dir(&root).unwrap();
        set_mode(&root, 0o700);
        Repository::init(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let binding = ManagementBinding::bind(&root).unwrap();
        let scope_before = fs::read(root.join(".kio/scope.json")).unwrap();

        set_mode(&root, 0o777);
        binding
            .revalidate()
            .expect("the user's scope root need not be owner-private");
        assert_eq!(mode(&root), 0o777);
        assert_eq!(
            fs::read(root.join(".kio/scope.json")).unwrap(),
            scope_before
        );
        verify_private_directory(&root.join(".kio")).expect("Kio's own store remains private");
    }

    #[test]
    fn unsafe_kio_directory_is_refused_by_repository_open_and_retained_binding() {
        let temp = fixture();
        let root = temp.path().join("scope");
        fs::create_dir(&root).unwrap();
        set_mode(&root, 0o700);
        Repository::init(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let binding = ManagementBinding::bind(&root).unwrap();
        set_mode(&root, 0o777);
        binding
            .revalidate()
            .expect("external scope root is allowed");
        let own = root.join(".kio");
        let before = fs::read(own.join("scope.json")).unwrap();
        set_mode(&own, 0o755);

        assert!(
            binding.revalidate().is_err(),
            "retained Kio directory must remain private"
        );
        assert!(ManagementBinding::bind(&root).is_err());
        assert!(Repository::open(&root).is_err());
        assert!(Repository::open_read_only(&root).is_err());
        assert_eq!(
            mode(&own),
            0o755,
            "unsafe existing store must not be repaired"
        );
        assert_eq!(mode(&root), 0o777);
        assert_eq!(fs::read(own.join("scope.json")).unwrap(), before);
    }

    #[test]
    fn unsafe_scope_authority_files_are_refused_without_permission_repair() {
        for leaf in ["scope.json", "config.toml"] {
            let temp = fixture();
            let root = temp.path().join("scope");
            fs::create_dir(&root).unwrap();
            set_mode(&root, 0o700);
            Repository::init(&root).unwrap();
            let root = root.canonicalize().unwrap();
            set_mode(&root, 0o777);
            let own_file = root.join(".kio").join(leaf);
            let before = fs::read(&own_file).unwrap();
            set_mode(&own_file, 0o644);

            assert!(
                Repository::open(&root).is_err(),
                "unsafe {leaf} must be refused"
            );
            assert!(
                Repository::open_read_only(&root).is_err(),
                "read-only open must refuse unsafe {leaf}"
            );
            assert_eq!(mode(&own_file), 0o644, "unsafe {leaf} must not be repaired");
            assert_eq!(fs::read(&own_file).unwrap(), before);
            assert_eq!(mode(&root), 0o777);
        }
    }

    #[test]
    fn retained_external_parent_reads_only_the_private_leaf_after_path_replacement() {
        let temp = fixture();
        let external = temp.path().join("external");
        fs::create_dir(&external).unwrap();
        set_mode(&external, 0o777);
        let file = external.join("credential");
        fs::write(&file, b"original").unwrap();
        set_mode(&file, 0o600);
        let retained = verify_private_creation_parent(&external).unwrap();
        let displaced = temp.path().join("displaced");
        fs::rename(&external, &displaced).unwrap();
        fs::create_dir(&external).unwrap();
        fs::write(external.join("credential"), b"replacement").unwrap();

        assert_eq!(
            read_private_file_at(&retained, "credential", 64).unwrap(),
            b"original"
        );
        assert_eq!(mode(&displaced), 0o777);
        set_mode(&displaced.join("credential"), 0o644);
        assert!(read_private_file_at(&retained, "credential", 64).is_err());
        assert_eq!(mode(&displaced.join("credential")), 0o644);
        assert_eq!(
            fs::read(external.join("credential")).unwrap(),
            b"replacement"
        );
    }
}
