use super::*;

impl GcSweepSession {
    pub fn bind(root: impl Into<PathBuf>) -> Result<Self> {
        Self::bind_with_control(root, crate::control::ControlHandle::default())
    }

    pub fn bind_with_control(
        root: impl Into<PathBuf>,
        control: crate::control::ControlHandle,
    ) -> Result<Self> {
        Self::bind_in_session(root, control, &crate::scope::LockSession::default())
    }

    pub fn bind_in_session(
        root: impl Into<PathBuf>,
        control: crate::control::ControlHandle,
        locks: &crate::scope::LockSession,
    ) -> Result<Self> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(KioError::invalid_usage(
                "GC sweep scope root must be absolute",
            ));
        }
        let scope = open_bound_absolute(&root)?;
        // A genuinely absent `.kio` is ordinary CLI misuse, not store
        // corruption.  Keep every other no-follow/open failure observable so
        // a symlink, reparse point, or non-directory can never masquerade as
        // an uninitialized scope.
        let kio = open_optional_dir(&scope, ".kio")?
            .ok_or_else(|| KioError::invalid_usage("current directory is not a Kio scope"))?;
        let canonical = root.canonicalize().map_err(|e| ioerr(e, &root))?;
        let kio_identity = id_file(&kio)?;
        if id_file(&scope)? != id_path(&canonical)?
            || kio_identity != id_child(&scope, ".kio", &kio_identity)?
        {
            return Err(corrupt("scope changed while binding GC sweep"));
        }
        let kio_directory = StoreDirectory::from_retained_with_control(
            kio.try_clone().map_err(|error| ioerr(error, &canonical))?,
            canonical.join(".kio"),
            control.clone(),
        )?;
        reject_pending_managed_restore(&kio_directory)?;
        Ok(Self {
            reads: CasReadAccess::Unmetered,
            lock_session: locks.clone(),
            root: canonical,
            scope,
            kio,
            control,
        })
    }
    /// Bind an automatic sweep to the exact repository capability already
    /// used by an internal child index. Ordinary repositories have no retained
    /// handles and use the same public no-follow bind as [`Self::bind`].
    pub fn bind_repository(repository: &Repository) -> Result<Self> {
        let mut session = Self::bind_in_session(
            repository.canonical_root().to_path_buf(),
            repository.control().clone(),
            repository.lock_session(),
        )?;
        session.reads = repository.object_store().read_access().clone();
        session.reads.charge(0)?;
        match (
            repository.bound_root_handle(),
            repository.bound_kio_handle(),
        ) {
            (Some(scope), Some(kio)) => {
                if id_file(&session.scope)? != id_file(scope)?
                    || id_file(&session.kio)? != id_file(kio)?
                {
                    return Err(corrupt(
                        "automatic GC scope differs from bound child repository",
                    ));
                }
            }
            (None, None) => {}
            _ => return Err(corrupt("incomplete bound child repository capability")),
        }
        Ok(session)
    }

    pub fn with_verified_reads(
        mut self,
        reads: &crate::cas::VerifiedCasReadCapability,
    ) -> Result<Self> {
        self.reads = self.reads.bind(reads)?;
        Ok(self)
    }

    #[must_use]
    pub fn control(&self) -> &crate::control::ControlHandle {
        &self.control
    }
}

pub fn read_active_marker(kio_dir: &Path) -> Result<Option<GcInProgressMarker>> {
    read_active_marker_with_control(kio_dir, &crate::control::ControlHandle::default())
}

pub fn ensure_no_active_sweep(kio_dir: &Path) -> Result<()> {
    ensure_no_active_sweep_with_control(kio_dir, &crate::control::ControlHandle::default())
}

pub fn validated_final_shallow_receipts(kio_dir: &Path) -> Result<BTreeMap<String, String>> {
    validated_final_shallow_receipts_with_control(
        kio_dir,
        &crate::control::ControlHandle::default(),
    )
}

pub fn validate_final_shallow_tree(
    kio_dir: &Path,
    commit_hash: &str,
    tree_hash: &str,
) -> Result<()> {
    validate_final_shallow_tree_with_control(
        kio_dir,
        commit_hash,
        tree_hash,
        &crate::control::ControlHandle::default(),
    )
}
