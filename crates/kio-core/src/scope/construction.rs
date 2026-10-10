use super::*;

impl Repository {
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        Self::init_with_control(path, crate::control::ControlHandle::default())
    }

    pub fn init_with_control(
        path: impl AsRef<Path>,
        control: crate::control::ControlHandle,
    ) -> Result<Self> {
        let root = path.as_ref();
        if !root.exists() {
            return Err(KioError::invalid_usage("init path does not exist"));
        }
        if !root.is_dir() {
            return Err(KioError::invalid_usage("init path must be a directory"));
        }

        let root = root.canonicalize().kio_io(root)?;
        let scope = StoreDirectory::open_with_control(&root, control.clone())?;
        let retained_root = scope
            .root_handle()
            .as_ref()
            .try_clone()
            .map_err(|error| KioError::io(error.to_string(), "."))?;
        match Self::create_bound_with_control(root.clone(), retained_root, control.clone()) {
            Ok(repo) => Ok(repo),
            // Preserve `init`'s existing-store behavior, but make the second
            // open a fresh retained binding rather than trusting the failed
            // create attempt's public name.
            Err(error) if error.error_code() == "KIO-E-STORE-EXISTS-001" => {
                Self::open_with_control(root, control)
            }
            Err(error) => Err(error),
        }
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_control(path, crate::control::ControlHandle::default())
    }

    pub fn open_with_control(
        path: impl AsRef<Path>,
        control: crate::control::ControlHandle,
    ) -> Result<Self> {
        let repo = Self::open_for_recovery_with_control(path, control)?;
        repo.ensure_no_pending_managed_restore()?;
        // A normal open is observational when no interrupted publication is
        // present. This preserves read access to a legitimate read-only scope;
        // the store lock is needed only to replay a durable journal.
        if repo.publication_journal()?.is_some() {
            repo.recover_publication()?;
        }
        repo.validate()?;
        // A journal which appeared after the initial observation must never be
        // hidden behind a freshly validated mutable projection.
        repo.ensure_no_pending_publication()?;
        Ok(repo)
    }

    /// Open only after proving no incomplete mutable publication exists.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_read_only_with_control(path, crate::control::ControlHandle::default())
    }

    pub fn open_read_only_with_control(
        path: impl AsRef<Path>,
        control: crate::control::ControlHandle,
    ) -> Result<Self> {
        let repo = Self::open_for_recovery_with_control(path, control)?;
        repo.ensure_no_pending_managed_restore()?;
        repo.ensure_no_pending_publication()?;
        repo.validate()?;
        Ok(repo)
    }

    /// Open the minimal durable authority required to replay a publication.
    /// This is intentionally not a general read API: callers must lock and
    /// call [`Self::recover_publication`] before observing mutable projections.
    pub fn open_for_recovery(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_for_recovery_with_control(path, crate::control::ControlHandle::default())
    }

    pub fn open_for_recovery_with_control(
        path: impl AsRef<Path>,
        control: crate::control::ControlHandle,
    ) -> Result<Self> {
        let root = path.as_ref().canonicalize().kio_io(path.as_ref())?;
        Self::open_retained_for_recovery(root, control)
    }

    pub fn open_current() -> Result<Self> {
        let cwd = std::env::current_dir().map_err(|err| KioError::io(err.to_string(), "."))?;
        Self::open(cwd)
    }

    pub fn open_current_read_only() -> Result<Self> {
        let cwd = std::env::current_dir().map_err(|err| KioError::io(err.to_string(), "."))?;
        Self::open_read_only(cwd)
    }

    /// Initialize a scope from a retained root directory. The process current
    /// directory is never observed or changed.
    pub fn init_bound(canonical_root: PathBuf, scope: File) -> Result<Self> {
        Self::init_bound_with_control(
            canonical_root,
            scope,
            crate::control::ControlHandle::default(),
        )
    }

    pub fn init_bound_with_control(
        canonical_root: PathBuf,
        scope: File,
        control: crate::control::ControlHandle,
    ) -> Result<Self> {
        let scope_directory = StoreDirectory::from_retained_with_control(
            scope,
            canonical_root.clone(),
            control.clone(),
        )?;
        crate::management::validate_controlled_root_with_control(
            scope_directory.root_handle().as_ref(),
            &canonical_root,
            control.clone(),
        )?;
        let existing = scope_directory.contains_entry(Path::new(".kio"))?;
        let kio = if existing {
            scope_directory.open_directory(Path::new(".kio"))?
        } else {
            let created = scope_directory.create_directory(Path::new(".kio"))?;
            crate::store_dir::restrict_new_private_directory(&created)?;
            let directory = StoreDirectory::from_retained_with_control(
                created
                    .try_clone()
                    .map_err(|error| KioError::io(error.to_string(), ".kio"))?,
                canonical_root.join(".kio"),
                control.clone(),
            )?;
            crate::management::validate_controlled_root_with_control(
                scope_directory.root_handle().as_ref(),
                &canonical_root,
                control.clone(),
            )?;
            initialize_bound_kio_layout_directory(&directory, &canonical_root)?;
            created
        };
        Self::open_bound_existing_with_control(
            canonical_root,
            scope_directory
                .root_handle()
                .as_ref()
                .try_clone()
                .map_err(|error| KioError::io(error.to_string(), "."))?,
            kio,
            control,
        )
    }

    /// Create a new retained scope and reject an existing `.kio` leaf.
    pub fn create_bound(canonical_root: PathBuf, scope: File) -> Result<Self> {
        Self::create_bound_with_control(
            canonical_root,
            scope,
            crate::control::ControlHandle::default(),
        )
    }

    pub fn create_bound_with_control(
        canonical_root: PathBuf,
        scope: File,
        control: crate::control::ControlHandle,
    ) -> Result<Self> {
        let scope_directory = StoreDirectory::from_retained_with_control(
            scope,
            canonical_root.clone(),
            control.clone(),
        )?;
        crate::management::validate_controlled_root_with_control(
            scope_directory.root_handle().as_ref(),
            &canonical_root,
            control.clone(),
        )?;
        let kio = scope_directory
            .create_directory(Path::new(".kio"))
            .map_err(|error| {
                KioError::new(
                    "KIO-E-STORE-EXISTS-001",
                    "refusing to create over an existing .kio store",
                    json!({"cause": error.error_code()}),
                    ExitCode::InvalidUsage,
                )
            })?;
        crate::store_dir::restrict_new_private_directory(&kio)?;
        let directory = StoreDirectory::from_retained_with_control(
            kio.try_clone()
                .map_err(|error| KioError::io(error.to_string(), ".kio"))?,
            canonical_root.join(".kio"),
            control.clone(),
        )?;
        crate::management::validate_controlled_root_with_control(
            scope_directory.root_handle().as_ref(),
            &canonical_root,
            control.clone(),
        )?;
        initialize_bound_kio_layout_directory(&directory, &canonical_root)?;
        Self::open_bound_existing_with_control(
            canonical_root,
            scope_directory
                .root_handle()
                .as_ref()
                .try_clone()
                .map_err(|error| KioError::io(error.to_string(), "."))?,
            kio,
            control,
        )
    }

    /// Open an initialized scope exclusively through retained root and `.kio`
    /// handles. `ManagementBinding` compares the named child to the retained
    /// handle on every supported platform before any operational I/O begins.
    pub fn open_bound_existing(canonical_root: PathBuf, scope: File, kio: File) -> Result<Self> {
        Self::open_bound_existing_with_control(
            canonical_root,
            scope,
            kio,
            crate::control::ControlHandle::default(),
        )
    }

    pub fn open_bound_existing_with_control(
        canonical_root: PathBuf,
        scope: File,
        kio: File,
        control: crate::control::ControlHandle,
    ) -> Result<Self> {
        let repo = Self::open_bound_for_recovery_with_control(canonical_root, scope, kio, control)?;
        wait_at_bound_snapshot_auto_layout_barrier(repo.control());
        repo.validate()?;
        Ok(repo)
    }

    /// Bind retained scope authority for publication recovery without reading
    /// mutable projections. Callers may lock and replay a pending journal
    /// before invoking the normal validating open path.
    pub fn open_bound_for_recovery(
        canonical_root: PathBuf,
        scope: File,
        kio: File,
    ) -> Result<Self> {
        Self::open_bound_for_recovery_with_control(
            canonical_root,
            scope,
            kio,
            crate::control::ControlHandle::default(),
        )
    }

    pub fn open_bound_for_recovery_with_control(
        canonical_root: PathBuf,
        scope: File,
        kio: File,
        control: crate::control::ControlHandle,
    ) -> Result<Self> {
        let (scope, kio) = crate::platform::scope_semantics::normalize_repository_handles(
            scope,
            kio,
            &canonical_root,
            control.clone(),
        )?;
        crate::management::ManagementBinding::from_retained_with_control(
            scope
                .try_clone()
                .map_err(|error| KioError::io(error.to_string(), "."))?,
            kio.try_clone()
                .map_err(|error| KioError::io(error.to_string(), ".kio"))?,
            canonical_root.clone(),
            control.clone(),
        )?;
        let repo = Self {
            root: canonical_root.clone(),
            kio_dir: canonical_root.join(".kio"),
            canonical_root,
            store: ObjectStore::from_bound_kio_with_control(&kio, control)?,
            bound_root: Some(Arc::new(scope)),
            bound_kio: Some(Arc::new(kio)),
        };
        repo.validate_config()?;
        repo.validate_scope()?;
        Ok(repo)
    }

    /// Bind an ordinary public-path request to retained root and `.kio`
    /// directories before any repository read.  The displayed paths remain
    /// useful diagnostics only; operational metadata and CAS access use the
    /// retained handles constructed here.
    fn open_retained_for_recovery(
        canonical_root: PathBuf,
        control: crate::control::ControlHandle,
    ) -> Result<Self> {
        let scope = StoreDirectory::open_with_control(&canonical_root, control.clone())?;
        let kio = scope.open_directory(Path::new(".kio"))?;
        Self::open_bound_for_recovery_with_control(
            canonical_root,
            scope
                .root_handle()
                .as_ref()
                .try_clone()
                .map_err(|error| KioError::io(error.to_string(), "."))?,
            kio,
            control,
        )
    }

    /// Open a scope for immutable CAS/index search while treating
    /// `manifest.json` as a derived acceleration artifact. Search history is
    /// committed tree truth, so a malformed mutable manifest must not invalidate a
    /// signed cursor replay. Config, scope identity, store directory, and HEAD are
    /// validated exactly as in [`Self::open`].
    pub fn open_for_search(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_for_search_with_control(path, crate::control::ControlHandle::default())
    }

    pub fn open_for_search_with_control(
        path: impl AsRef<Path>,
        control: crate::control::ControlHandle,
    ) -> Result<Self> {
        let root = path.as_ref().canonicalize().kio_io(path.as_ref())?;
        let repo = Self::open_retained_for_recovery(root, control)?;
        repo.ensure_no_pending_managed_restore()?;
        repo.head_commit_hash()?;
        repo.ensure_no_pending_publication()?;
        Ok(repo)
    }

    /// The read-only counterpart to [`Self::open_for_search`]. It validates the
    /// same immutable store and scope state but never repairs HEAD or refs.
    pub fn open_for_search_read_only(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_for_search_read_only_with_control(path, crate::control::ControlHandle::default())
    }

    pub fn open_for_search_read_only_with_control(
        path: impl AsRef<Path>,
        control: crate::control::ControlHandle,
    ) -> Result<Self> {
        Self::open_for_search_with_control(path, control)
    }

    pub fn open_current_for_search() -> Result<Self> {
        let cwd = std::env::current_dir().map_err(|err| KioError::io(err.to_string(), "."))?;
        Self::open_for_search(cwd)
    }
}
