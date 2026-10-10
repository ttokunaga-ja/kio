use super::*;

impl GcPlanner {
    pub fn bind_current() -> Result<Self> {
        Self::bind(std::env::current_dir().map_err(|e| ioerr(e, "."))?)
    }
    pub fn bind(root: impl Into<PathBuf>) -> Result<Self> {
        let requested = root.into();
        if !requested.is_absolute() {
            return Err(KioError::invalid_usage(
                "GC planner scope root must be absolute",
            ));
        }
        // Bind the caller's actual path component-by-component before asking
        // the OS for a canonical diagnostic name. This rejects a symlink or
        // reparse component instead of silently following it during bind.
        let scope = open_bound_absolute(&requested)?;
        let public = requested.canonicalize().map_err(|e| ioerr(e, "scope"))?;
        if id_file(&scope)? != id_path(&public)? {
            return Err(corrupt("scope root changed while binding"));
        }
        let kio = open_optional_dir(&scope, ".kio")?
            .ok_or_else(|| KioError::invalid_usage("current directory is not a Kio scope"))?;
        Ok(Self {
            root: public,
            scope,
            kio,
            limits: GcPlanLimits::default(),
        })
    }
    pub fn with_limits(mut self, limits: GcPlanLimits) -> Self {
        self.limits = limits;
        self
    }
}

pub fn read_active_marker(kio_dir: &Path) -> Result<Option<GcInProgressMarker>> {
    let kio = open_bound_absolute(&absolute_lexical_path(kio_dir)?)?;
    read_active_marker_bound(&kio)
}

pub fn ensure_no_active_sweep(kio_dir: &Path) -> Result<()> {
    if let Some(marker) = read_active_marker(kio_dir)? {
        return Err(active_sweep_error(&marker));
    }
    Ok(())
}
