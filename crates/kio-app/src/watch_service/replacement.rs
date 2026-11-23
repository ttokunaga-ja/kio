use super::*;

pub(super) const MAX_INTENT_BYTES: u64 = 2 * MAX_MANIFEST_BYTES + 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplacementIntent {
    old: ServiceManifest,
    new: ServiceManifest,
    phase: ReplacementPhase,
    restart: bool,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ReplacementPhase {
    Removing,
    Installing,
    Starting,
}

pub(super) fn intent_leaf(leaf: &Path) -> PathBuf {
    leaf.with_extension("replace.json")
}

fn save_intent(
    store: &StoreDirectory,
    leaf: &Path,
    intent: &ReplacementIntent,
    publication: Publication,
) -> Result<()> {
    let bytes = serde_json::to_vec(intent).map_err(service_error)?;
    if bytes.len() as u64 > MAX_INTENT_BYTES {
        return Err(invalid(
            "watch service replacement intent exceeds its metadata limit",
        ));
    }
    store.write_atomic(&intent_leaf(leaf), &bytes, publication)?;
    store.ensure_owner_private(&intent_leaf(leaf))
}

fn validate_intent(intent: &ReplacementIntent) -> Result<()> {
    validate_manifest(&intent.old)?;
    validate_manifest(&intent.new)?;
    if intent.old.phase != ServicePhase::Installed
        || intent.new.phase != ServicePhase::Installed
        || intent.old.id != intent.new.id
        || intent.old.root != intent.new.root
        || intent.old.scope_id != intent.new.scope_id
    {
        return Err(service_error("invalid watch service replacement identity"));
    }
    Ok(())
}

// The mutation lock is retained by the caller through inspection, confirmation,
// intent publication and recovery. A durable intent is the approval record: a
// retry may finish only that exact target, without prompting for a new change.
#[allow(clippy::too_many_arguments)]
pub(super) fn replace_existing<S: Scheduler>(
    store: &StoreDirectory,
    leaf: &Path,
    old: ServiceManifest,
    new: &ServiceManifest,
    replace: bool,
    confirm: impl FnOnce(&str) -> Result<bool>,
    scheduler: &S,
) -> Result<WatchResponse> {
    validate_manifest(new)?;
    if let Some(bytes) = store.read_optional(&intent_leaf(leaf), MAX_INTENT_BYTES)? {
        let intent: ReplacementIntent = serde_json::from_slice(&bytes).map_err(service_error)?;
        validate_intent(&intent)?;
        if !replace
            || !same_installation(&intent.new, new)
            || !(same_installation(&old, &intent.old) || same_installation(&old, &intent.new))
            || (intent.phase == ReplacementPhase::Removing && !same_installation(&old, &intent.old))
            || (intent.phase == ReplacementPhase::Starting && !same_installation(&old, &intent.new))
        {
            return Err(service_error(
                "resume watch service install --replace with the exact approved definition",
            ));
        }
        return recover_replacement(store, leaf, intent, scheduler);
    }
    if same_installation(&old, new) {
        return recover_install(store, leaf, old, scheduler);
    }
    if !replace {
        return Err(service_error(
            "watch service definition differs; use install --replace to review and confirm the update",
        ));
    }
    if old.phase != ServicePhase::Installed {
        return Err(service_error(
            "finish the interrupted installation before replacing it",
        ));
    }
    let registration = scheduler.inspect(&old)?;
    if registration != NativeRegistration::Owned {
        return Err(service_error(
            "refusing to replace an absent or foreign native service registration",
        ));
    }
    let restart = scheduler.running(&old)?;
    let summary = format!(
        "Replace watch service {}?\nOld: {}\nNew: {}\nImpact: stop the old watcher; replace and verify the registration; restart: {}. [y/N]",
        old.id,
        serde_json::to_string(&old).map_err(service_error)?,
        serde_json::to_string(new).map_err(service_error)?,
        restart,
    );
    if !confirm(&summary)? {
        return Err(service_error(
            "watch service replacement requires explicit confirmation",
        ));
    }
    // Recheck after the user has reviewed the diff. Nothing is persisted before
    // confirmation, and a changed native definition cannot acquire our intent.
    if scheduler.inspect(&old)? != NativeRegistration::Owned {
        return Err(service_error("native service changed during confirmation"));
    }
    let mut target = new.clone();
    target.phase = ServicePhase::Installed;
    let intent = ReplacementIntent {
        old,
        new: target,
        phase: ReplacementPhase::Removing,
        restart,
    };
    save_intent(store, leaf, &intent, Publication::CreateOnly)?;
    recover_replacement(store, leaf, intent, scheduler)
}

fn recover_replacement<S: Scheduler>(
    store: &StoreDirectory,
    leaf: &Path,
    mut intent: ReplacementIntent,
    scheduler: &S,
) -> Result<WatchResponse> {
    ensure_installation_locator(&intent.old, &intent.new, store, leaf)?;
    if intent.phase == ReplacementPhase::Removing {
        scheduler.recover_replacement_definition(&intent.old, &intent.new)?;
        match scheduler.inspect(&intent.old)? {
            NativeRegistration::Owned => {
                scheduler.stop(&intent.old)?;
                // Revalidate after stopping as well: a concurrent native edit
                // must not be deleted just because stopping once succeeded.
                if scheduler.inspect(&intent.old)? != NativeRegistration::Owned {
                    return Err(service_error("native service changed while stopping"));
                }
                scheduler.replace_definition(&intent.old, &intent.new)?;
            }
            NativeRegistration::Absent => {}
            NativeRegistration::Foreign => {
                if scheduler.inspect(&intent.new)? != NativeRegistration::Owned {
                    return Err(service_error("foreign native service during replacement"));
                }
            }
        }
        if scheduler.inspect(&intent.old)? != NativeRegistration::Absent
            && scheduler.inspect(&intent.new)? != NativeRegistration::Owned
        {
            return Err(service_error("old watch service was not removed"));
        }
        intent.phase = ReplacementPhase::Installing;
        save_intent(store, leaf, &intent, Publication::Replace)?;
    }
    if intent.phase == ReplacementPhase::Installing {
        if scheduler.inspect(&intent.new)? == NativeRegistration::Foreign {
            return Err(service_error(
                "foreign native service during replacement install",
            ));
        }
        // Idempotent even if publication succeeded and scheduler enable failed.
        scheduler.install(&intent.new)?;
        if scheduler.inspect(&intent.new)? != NativeRegistration::Owned {
            return Err(service_error(
                "replacement watch service failed verification",
            ));
        }
        write_manifest(store, leaf, &intent.new, Publication::Replace)?;
        store.ensure_owner_private(leaf)?;
        intent.phase = ReplacementPhase::Starting;
        save_intent(store, leaf, &intent, Publication::Replace)?;
    }
    if scheduler.inspect(&intent.new)? != NativeRegistration::Owned {
        return Err(service_error(
            "replacement watch service no longer matches its intent",
        ));
    }
    if !intent.restart && scheduler.running(&intent.new)? {
        // Preserve a login/start that arrived after the original confirmation.
        // Save this before stopping it, so another interruption cannot lose the
        // user's newly active service.
        intent.restart = true;
        save_intent(store, leaf, &intent, Publication::Replace)?;
    }
    // A login can have launched a runner from the previous manifest while
    // registration was being published. Liveness alone proves no settings.
    // After publishing the target manifest, stop every such instance and only
    // then launch from that manifest. Repeating this approved restart is safe.
    scheduler.stop(&intent.new)?;
    // Stop disables automatic activation. Re-enable only after the target
    // manifest is durable; future logins must retain install's registration.
    scheduler.install(&intent.new)?;
    if intent.restart {
        scheduler.start(&intent.new)?;
    }
    if scheduler.inspect(&intent.new)? != NativeRegistration::Owned {
        return Err(service_error(
            "replacement watch service changed after restart",
        ));
    }
    let bytes = serde_json::to_vec(&intent).map_err(service_error)?;
    store.quarantine_then_remove(&intent_leaf(leaf), &bytes, MAX_INTENT_BYTES)?;
    Ok(installed_response(
        intent.new,
        "replaced",
        if intent.restart {
            "restart_requested"
        } else {
            "enabled_not_started"
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    // A scheduler with separate cached and loaded definitions. This models the
    // partial publication shared by systemd, launchd and Task Scheduler.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum SchedulerKind {
        Systemd,
        Launchd,
        TaskScheduler,
    }

    struct SchedulerFixture {
        kind: SchedulerKind,
        cached: RefCell<Option<String>>,
        loaded: RefCell<Option<String>>,
        active: Cell<bool>,
        enabled: Cell<bool>,
        calls: Cell<usize>,
        fail_after: Cell<Option<usize>>,
        runner_manifest: PathBuf,
        child: RefCell<Option<ServiceManifest>>,
        drop_ins: RefCell<String>,
    }

    impl SchedulerFixture {
        fn new(old: &ServiceManifest, active: bool, manifest: &Path) -> Self {
            Self::for_kind(old, active, SchedulerKind::Systemd, manifest)
        }
        fn for_kind(
            old: &ServiceManifest,
            active: bool,
            kind: SchedulerKind,
            manifest: &Path,
        ) -> Self {
            Self {
                kind,
                cached: RefCell::new(Some(old.definition.clone())),
                loaded: RefCell::new(if kind == SchedulerKind::Launchd && !active {
                    None
                } else {
                    Some(Self::native_definition_for(kind, old, manifest))
                }),
                active: Cell::new(active),
                enabled: Cell::new(true),
                calls: Cell::new(0),
                fail_after: Cell::new(None),
                runner_manifest: manifest.to_path_buf(),
                child: RefCell::new(active.then(|| old.clone())),
                drop_ins: RefCell::new(String::new()),
            }
        }
        fn native_definition_for(
            kind: SchedulerKind,
            m: &ServiceManifest,
            manifest: &Path,
        ) -> String {
            if kind == SchedulerKind::TaskScheduler {
                // The OS task points to a runner and an immutable manifest
                // locator. Interval and watcher environment live in that file.
                format!(
                    "<Task><Principals><UserId>fixture-current-user</UserId></Principals><Triggers><LogonTrigger/></Triggers><Actions><Exec><Command>{}</Command><Arguments>{}</Arguments></Exec></Actions><Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy></Settings></Task>",
                    m.binary.display(),
                    manifest.display()
                )
            } else {
                m.definition.clone()
            }
        }
        fn native_definition(&self, m: &ServiceManifest) -> String {
            Self::native_definition_for(self.kind, m, &self.runner_manifest)
        }
        fn checkpoint(&self) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            if self.fail_after.get() == Some(self.calls.get()) {
                Err(service_error("injected interruption"))
            } else {
                Ok(())
            }
        }
    }
    impl Scheduler for SchedulerFixture {
        fn inspect(&self, m: &ServiceManifest) -> Result<NativeRegistration> {
            let cache = self.cached.borrow();
            let loaded = self.loaded.borrow();
            if cache.is_none() && loaded.is_none() {
                Ok(NativeRegistration::Absent)
            } else if cache.as_deref().is_none_or(|d| d == m.definition)
                && loaded.as_deref().is_none_or(|actual| {
                    let expected = self.native_definition(m);
                    if self.kind == SchedulerKind::TaskScheduler {
                        crate::platform::watch_task_definition::task_definition_matches(
                            actual, &expected,
                        )
                    } else {
                        actual == expected
                    }
                })
                && (self.kind != SchedulerKind::Systemd
                    || crate::platform::watch_systemd_unit::unit_definition_matches(
                        &format!(
                            "# {}\n{}{}",
                            m.native_path.display(),
                            cache.as_deref().unwrap_or(&m.definition),
                            self.drop_ins.borrow()
                        ),
                        &m.native_path,
                        &m.definition,
                    ))
            {
                Ok(NativeRegistration::Owned)
            } else {
                Ok(NativeRegistration::Foreign)
            }
        }
        fn install(&self, m: &ServiceManifest) -> Result<()> {
            assert_ne!(self.inspect(m)?, NativeRegistration::Foreign);
            *self.cached.borrow_mut() = Some(m.definition.clone());
            self.checkpoint()?;
            if self.kind != SchedulerKind::Launchd {
                *self.loaded.borrow_mut() = Some(self.native_definition(m));
            }
            self.enabled.set(true);
            self.checkpoint()
        }
        fn stop(&self, _: &ServiceManifest) -> Result<()> {
            self.active.set(false);
            if self.kind != SchedulerKind::Launchd {
                self.enabled.set(false);
            }
            *self.child.borrow_mut() = None;
            if self.kind == SchedulerKind::Launchd {
                *self.loaded.borrow_mut() = None;
            }
            self.checkpoint()
        }
        fn recover_replacement_definition(
            &self,
            old: &ServiceManifest,
            new: &ServiceManifest,
        ) -> Result<()> {
            if self.kind == SchedulerKind::Systemd
                && old.definition != new.definition
                && self.cached.borrow().as_deref() == Some(new.definition.as_str())
            {
                if !self.drop_ins.borrow().is_empty() || self.active.get() {
                    return Err(service_error(
                        "foreign or active definition during publication recovery",
                    ));
                }
                *self.loaded.borrow_mut() = Some(self.native_definition(new));
                self.checkpoint()?;
            }
            Ok(())
        }
        fn replace_definition(&self, old: &ServiceManifest, new: &ServiceManifest) -> Result<()> {
            assert_eq!(self.inspect(old)?, NativeRegistration::Owned);
            if self.kind != SchedulerKind::Systemd {
                *self.loaded.borrow_mut() = None;
            }
            self.checkpoint()?;
            // Native replacement retains the cache until an atomic publication
            // replaces it, rather than creating a missing-fragment interval.
            *self.cached.borrow_mut() = Some(new.definition.clone());
            self.checkpoint()?;
            if self.kind == SchedulerKind::Systemd {
                *self.loaded.borrow_mut() = Some(self.native_definition(new));
            }
            self.checkpoint()
        }
        fn uninstall(&self, _: &ServiceManifest) -> Result<()> {
            if self.kind == SchedulerKind::Systemd {
                *self.cached.borrow_mut() = None;
                self.checkpoint()?;
                *self.loaded.borrow_mut() = None;
            } else {
                *self.loaded.borrow_mut() = None;
                self.checkpoint()?;
                *self.cached.borrow_mut() = None;
            }
            self.checkpoint()
        }
        fn start(&self, m: &ServiceManifest) -> Result<()> {
            if self.inspect(m)? != NativeRegistration::Owned {
                return Err(service_error("foreign native definition before activation"));
            }
            self.enabled.set(true);
            let child = if self.kind == SchedulerKind::TaskScheduler {
                let observed: ServiceManifest = serde_json::from_slice(
                    &fs::read(&self.runner_manifest).map_err(service_error)?,
                )
                .map_err(service_error)?;
                validate_manifest(&observed)?;
                if observed.id != m.id
                    || observed.root != m.root
                    || observed.scope_id != m.scope_id
                    || observed.binary != m.binary
                {
                    return Err(service_error(
                        "runner manifest does not bind its native task",
                    ));
                }
                observed
            } else {
                // systemd and launchd execute the complete owned native argv /
                // environment directly; they do not consult a runner manifest.
                m.clone()
            };
            *self.child.borrow_mut() = Some(child);
            if self.loaded.borrow().is_none() {
                *self.loaded.borrow_mut() = Some(self.native_definition(m));
            }
            self.active.set(true);
            self.checkpoint()
        }

        fn state(&self, _: &ServiceManifest, r: NativeRegistration) -> Result<NativeServiceState> {
            Ok(NativeServiceState {
                registered: r == NativeRegistration::Owned,
                active: self.active.get(),
            })
        }
    }
    fn definitions(base: &Path) -> (ServiceManifest, ServiceManifest) {
        let mut old = super::super::tests::manifest();
        old.phase = ServicePhase::Installed;
        old.root = base.join("root");
        old.binary = base.join("kio-old");
        old.environment = ServiceEnvironment {
            home: base.to_path_buf(),
            userprofile: base.to_path_buf(),
            xdg_config_home: base.join("config"),
            xdg_data_home: base.join("data"),
            xdg_cache_home: base.join("cache"),
        };
        old.id = stable_id(&old.scope_id, &old.root);
        // Keep the fixture private while retaining the Task Scheduler cache anchor.
        old.native_path = crate::platform::watch_task_definition::native_task_cache_path(
            &old.environment.xdg_data_home,
            &format!("{}.{NATIVE_DEFINITION_EXTENSION}", old.id),
        );
        old.definition = render_definition(
            &old.id,
            &old.binary,
            &old.root,
            old.reconcile_interval_seconds,
            &old.environment,
            &manifest_locator(&old.id, &old.environment).unwrap(),
        )
        .unwrap();
        let mut new = old.clone();
        new.binary = base.join("kio-updated");
        new.reconcile_interval_seconds = 600;
        new.environment.xdg_cache_home = base.join("updated-cache");
        new.definition = render_definition(
            &new.id,
            &new.binary,
            &new.root,
            new.reconcile_interval_seconds,
            &new.environment,
            &registered_manifest_locator(&new).unwrap(),
        )
        .unwrap();
        (old, new)
    }

    #[test]
    fn atomic_publication_recovers_before_manager_reload_and_preserves_new_dropins() {
        for foreign in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let store = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
            let leaf = Path::new("service.json");
            let (old, new) = definitions(&temp.path().canonicalize().unwrap());
            write_manifest(&store, leaf, &old, Publication::CreateOnly).unwrap();
            let scheduler = SchedulerFixture::new(&old, false, &temp.path().join(leaf));
            *scheduler.cached.borrow_mut() = Some(new.definition.clone());
            if foreign {
                *scheduler.drop_ins.borrow_mut() =
                    "\n# foreign.conf\n[Service]\nExecStop=foreign\n".into();
            }
            let intent = ReplacementIntent {
                old: old.clone(),
                new: new.clone(),
                phase: ReplacementPhase::Removing,
                restart: true,
            };
            save_intent(&store, leaf, &intent, Publication::CreateOnly).unwrap();
            let native_before = scheduler.loaded.borrow().clone();
            let result = replace_existing(
                &store,
                leaf,
                old,
                &new,
                true,
                |_| panic!("already approved"),
                &scheduler,
            );
            if foreign {
                assert!(result.is_err());
                assert_eq!(scheduler.calls.get(), 0);
                assert_eq!(*scheduler.loaded.borrow(), native_before);
            } else {
                result.unwrap();
                assert_eq!(scheduler.inspect(&new).unwrap(), NativeRegistration::Owned);
                assert!(scheduler.active.get());
            }
        }
    }

    #[test]
    fn review2_3_os_only_definition_changes_never_confirm_or_mutate() {
        for kind in [SchedulerKind::Systemd, SchedulerKind::TaskScheduler] {
            let temp = tempfile::tempdir().unwrap();
            let store = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
            let leaf = Path::new("service.json");
            let (old, new) = definitions(&temp.path().canonicalize().unwrap());
            write_manifest(&store, leaf, &old, Publication::CreateOnly).unwrap();
            let scheduler = SchedulerFixture::for_kind(&old, true, kind, &temp.path().join(leaf));
            if kind == SchedulerKind::Systemd {
                *scheduler.drop_ins.borrow_mut() = "\n# /run/systemd/user.control/foreign.conf\n[Service]\nExecStop=/bin/foreign\n".into();
            } else {
                let os_only = scheduler.loaded.borrow().as_ref().unwrap().replace(
                    "</Exec>",
                    "<WorkingDirectory>foreign</WorkingDirectory></Exec>",
                );
                *scheduler.loaded.borrow_mut() = Some(os_only);
            }
            let before = fs::read(temp.path().join(leaf)).unwrap();
            let native_before = scheduler.loaded.borrow().clone();
            assert!(
                replace_existing(
                    &store,
                    leaf,
                    old,
                    &new,
                    true,
                    |_| panic!("foreign definition must not reach confirmation"),
                    &scheduler
                )
                .is_err()
            );
            assert_eq!(scheduler.calls.get(), 0);
            assert_eq!(fs::read(temp.path().join(leaf)).unwrap(), before);
            assert_eq!(*scheduler.loaded.borrow(), native_before);
            assert!(
                store
                    .read_optional(&intent_leaf(leaf), MAX_INTENT_BYTES)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn review4_data_home_change_keeps_a_single_anchor_through_registration_absence() {
        for kind in [
            SchedulerKind::Systemd,
            SchedulerKind::Launchd,
            SchedulerKind::TaskScheduler,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let base = temp.path().canonicalize().unwrap();
            let (mut old, _) = definitions(&base);
            old.environment.xdg_data_home = base.join("old-data");
            old.definition = render_definition(
                &old.id,
                &old.binary,
                &old.root,
                old.reconcile_interval_seconds,
                &old.environment,
                &registered_manifest_locator(&old).unwrap(),
            )
            .unwrap();
            let anchor = registered_manifest_locator(&old).unwrap();
            fs::create_dir_all(anchor.parent().unwrap()).unwrap();
            let store = StoreDirectory::open(anchor.parent().unwrap().parent().unwrap()).unwrap();
            let leaf = manifest_leaf(&old.id).unwrap();
            write_manifest(&store, &leaf, &old, Publication::CreateOnly).unwrap();
            let mut new = old.clone();
            new.environment.xdg_data_home = base.join("new-data");
            new.definition = render_definition(
                &new.id,
                &new.binary,
                &new.root,
                new.reconcile_interval_seconds,
                &new.environment,
                &anchor,
            )
            .unwrap();
            let scheduler = SchedulerFixture::for_kind(&old, true, kind, &anchor);
            assert!(
                replace_existing(
                    &store,
                    &leaf,
                    old.clone(),
                    &new,
                    true,
                    |_| Ok(false),
                    &scheduler
                )
                .is_err()
            );
            assert!(!new.environment.xdg_data_home.exists());
            scheduler.fail_after.set(Some(2));
            assert!(
                replace_existing(&store, &leaf, old, &new, true, |_| Ok(true), &scheduler).is_err()
            );
            let located = installation_locator(&new.id, &new.environment, |_| {
                Ok(
                    if kind == SchedulerKind::TaskScheduler && scheduler.loaded.borrow().is_none() {
                        None
                    } else {
                        Some(anchor.clone())
                    },
                )
            })
            .unwrap()
            .unwrap();
            assert_eq!(located, anchor);
            let (canonical, leaf, current) =
                load_installation(&located, &ControlHandle::default()).unwrap();
            scheduler.fail_after.set(None);
            replace_existing(
                &canonical,
                &leaf,
                current,
                &new,
                true,
                |_| panic!("already approved"),
                &scheduler,
            )
            .unwrap();
            assert!(same_installation(
                scheduler.child.borrow().as_ref().unwrap(),
                &new
            ));
            let mut later = new.environment.clone();
            later.xdg_data_home = base.join("third-data");
            assert_eq!(
                installation_locator(&new.id, &later, |_| {
                    if kind == SchedulerKind::TaskScheduler {
                        Ok(Some(anchor.clone()))
                    } else {
                        manifest_locator(&new.id, &new.environment).map(Some)
                    }
                })
                .unwrap(),
                Some(anchor)
            );
        }
    }

    #[test]
    fn review1_login_between_registration_and_manifest_publication_restarts_old_configuration() {
        for restart in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let store = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
            let leaf = Path::new("service.json");
            let (old, mut new) = definitions(&temp.path().canonicalize().unwrap());
            new.binary = old.binary.clone();
            new.definition = render_definition(
                &new.id,
                &new.binary,
                &new.root,
                new.reconcile_interval_seconds,
                &new.environment,
                &registered_manifest_locator(&new).unwrap(),
            )
            .unwrap();
            write_manifest(&store, leaf, &old, Publication::CreateOnly).unwrap();
            assert_eq!(
                SchedulerFixture::native_definition_for(
                    SchedulerKind::TaskScheduler,
                    &old,
                    &temp.path().join(leaf)
                ),
                SchedulerFixture::native_definition_for(
                    SchedulerKind::TaskScheduler,
                    &new,
                    &temp.path().join(leaf)
                )
            );
            let scheduler = SchedulerFixture::for_kind(
                &new,
                false,
                SchedulerKind::TaskScheduler,
                &temp.path().join(leaf),
            );
            // Registration has committed. A subsequent login runs the actual
            // runner protocol: read the still-old private manifest from disk.
            let login: ServiceManifest =
                serde_json::from_slice(&fs::read(temp.path().join(leaf)).unwrap()).unwrap();
            *scheduler.child.borrow_mut() = Some(login);
            scheduler.active.set(true);
            let intent = ReplacementIntent {
                old: old.clone(),
                new: new.clone(),
                phase: ReplacementPhase::Installing,
                restart,
            };
            save_intent(&store, leaf, &intent, Publication::CreateOnly).unwrap();
            replace_existing(
                &store,
                leaf,
                old,
                &new,
                true,
                |_| panic!("already confirmed"),
                &scheduler,
            )
            .unwrap();
            assert!(
                scheduler.active.get(),
                "login-started watcher was left stopped"
            );
            let child = scheduler.child.borrow();
            assert!(
                same_installation(child.as_ref().unwrap(), &new),
                "old runner settings survived recovery"
            );
        }
    }

    #[test]
    fn unreadable_target_is_rejected_before_confirmation_or_scheduler_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let store = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
        let leaf = Path::new("service.json");
        let (old, mut new) = definitions(&temp.path().canonicalize().unwrap());
        write_manifest(&store, leaf, &old, Publication::CreateOnly).unwrap();
        new.definition = "x".repeat(MAX_MANIFEST_BYTES as usize + 1);
        let scheduler = SchedulerFixture::new(&old, true, &temp.path().join(leaf));
        assert!(
            replace_existing(
                &store,
                leaf,
                old,
                &new,
                true,
                |_| panic!("invalid target must not reach confirmation"),
                &scheduler
            )
            .is_err()
        );
        assert_eq!(scheduler.calls.get(), 0);
    }

    #[test]
    fn identical_install_never_requests_confirmation_or_rewrites_files() {
        let temp = tempfile::tempdir().unwrap();
        let store = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
        let leaf = Path::new("service.json");
        let (old, _) = definitions(&temp.path().canonicalize().unwrap());
        write_manifest(&store, leaf, &old, Publication::CreateOnly).unwrap();
        let before = fs::read(temp.path().join(leaf)).unwrap();
        let scheduler = SchedulerFixture::new(&old, true, &temp.path().join(leaf));
        for replace in [false, true] {
            replace_existing(
                &store,
                leaf,
                old.clone(),
                &old,
                replace,
                |_| panic!("identical installation needs no confirmation"),
                &scheduler,
            )
            .unwrap();
            assert_eq!(fs::read(temp.path().join(leaf)).unwrap(), before);
            assert_eq!(scheduler.calls.get(), 0);
            assert!(scheduler.active.get());
        }
    }

    #[test]
    fn durable_replacement_phases_recover_partial_native_publications() {
        // systemd can retain a loaded unit after unlink; launchd can retain
        // only a plist; Task Scheduler can retain only the private XML cache.
        for (phase, cached, loaded, new_manifest) in [
            (ReplacementPhase::Removing, true, true, false),
            (ReplacementPhase::Removing, true, false, false),
            (ReplacementPhase::Removing, false, true, false),
            (ReplacementPhase::Removing, false, false, false),
            (ReplacementPhase::Installing, false, false, false),
            (ReplacementPhase::Installing, true, false, false),
            (ReplacementPhase::Installing, true, true, false),
            (ReplacementPhase::Installing, true, true, true),
            (ReplacementPhase::Starting, true, true, true),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let store = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
            let leaf = Path::new("service.json");
            let (old, new) = definitions(&temp.path().canonicalize().unwrap());
            let current = if new_manifest {
                new.clone()
            } else {
                old.clone()
            };
            write_manifest(&store, leaf, &current, Publication::CreateOnly).unwrap();
            let native = if phase == ReplacementPhase::Removing {
                &old
            } else {
                &new
            };
            let scheduler = SchedulerFixture::new(native, false, &temp.path().join(leaf));
            if !cached {
                *scheduler.cached.borrow_mut() = None;
            }
            if !loaded {
                *scheduler.loaded.borrow_mut() = None;
            }
            let intent = ReplacementIntent {
                old,
                new: new.clone(),
                phase,
                restart: true,
            };
            save_intent(&store, leaf, &intent, Publication::CreateOnly).unwrap();
            let before_calls = scheduler.calls.get();
            let mut wrong = new.clone();
            wrong.reconcile_interval_seconds += 1;
            assert!(
                replace_existing(
                    &store,
                    leaf,
                    current.clone(),
                    &wrong,
                    true,
                    |_| panic!("wrong resume"),
                    &scheduler
                )
                .is_err()
            );
            assert!(
                replace_existing(
                    &store,
                    leaf,
                    current.clone(),
                    &new,
                    false,
                    |_| panic!("missing replace"),
                    &scheduler
                )
                .is_err()
            );
            assert_eq!(scheduler.calls.get(), before_calls);
            replace_existing(
                &store,
                leaf,
                current,
                &new,
                true,
                |_| panic!("already approved"),
                &scheduler,
            )
            .unwrap();
            assert_eq!(
                scheduler.loaded.borrow().as_deref(),
                Some(new.definition.as_str())
            );
            assert!(scheduler.active.get());
            assert!(
                store
                    .read_optional(&intent_leaf(leaf), MAX_INTENT_BYTES)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn foreign_changes_during_confirmation_do_not_publish_an_intent() {
        let temp = tempfile::tempdir().unwrap();
        let store = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
        let leaf = Path::new("service.json");
        let (old, new) = definitions(&temp.path().canonicalize().unwrap());
        write_manifest(&store, leaf, &old, Publication::CreateOnly).unwrap();
        let scheduler = SchedulerFixture::new(&old, true, &temp.path().join(leaf));
        assert!(
            replace_existing(
                &store,
                leaf,
                old,
                &new,
                true,
                |_| {
                    *scheduler.loaded.borrow_mut() = Some("foreign".into());
                    Ok(true)
                },
                &scheduler
            )
            .is_err()
        );
        assert_eq!(scheduler.calls.get(), 0);
        assert!(
            store
                .read_optional(&intent_leaf(leaf), MAX_INTENT_BYTES)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn replacement_requires_flag_and_confirmation_before_any_mutation() {
        for replace in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let store = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
            let leaf = Path::new("service.json");
            let (old, new) = definitions(&temp.path().canonicalize().unwrap());
            write_manifest(&store, leaf, &old, Publication::CreateOnly).unwrap();
            let before = fs::read(temp.path().join(leaf)).unwrap();
            let entry_count = fs::read_dir(temp.path()).unwrap().count();
            let scheduler = SchedulerFixture::new(&old, true, &temp.path().join(leaf));
            let confirmed = Cell::new(false);
            let result = replace_existing(
                &store,
                leaf,
                old,
                &new,
                replace,
                |summary| {
                    confirmed.set(true);
                    assert!(summary.contains("kio-old"));
                    assert!(summary.contains("kio-updated"));
                    assert!(summary.contains("300"));
                    assert!(summary.contains("600"));
                    assert!(summary.contains("updated-cache"));
                    assert!(summary.contains("restart"));
                    assert_eq!(fs::read(temp.path().join(leaf)).unwrap(), before);
                    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), entry_count);
                    assert_eq!(scheduler.calls.get(), 0);
                    Ok(false)
                },
                &scheduler,
            );
            assert!(result.is_err());
            assert_eq!(confirmed.get(), replace);
            assert_eq!(scheduler.calls.get(), 0);
            assert_eq!(fs::read(temp.path().join(leaf)).unwrap(), before);
            assert_eq!(fs::read_dir(temp.path()).unwrap().count(), entry_count);
        }
    }

    #[test]
    fn replacement_recovers_every_scheduler_interruption_and_preserves_running_state() {
        for kind in [
            SchedulerKind::Systemd,
            SchedulerKind::Launchd,
            SchedulerKind::TaskScheduler,
        ] {
            for active in [false, true] {
                for failure in 0..=10 {
                    let temp = tempfile::tempdir().unwrap();
                    let store = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
                    let leaf = Path::new("service.json");
                    let (old, new) = definitions(&temp.path().canonicalize().unwrap());
                    write_manifest(&store, leaf, &old, Publication::CreateOnly).unwrap();
                    let entry_count = fs::read_dir(temp.path()).unwrap().count();
                    let scheduler =
                        SchedulerFixture::for_kind(&old, active, kind, &temp.path().join(leaf));
                    scheduler.fail_after.set(Some(failure));
                    let first = replace_existing(
                        &store,
                        leaf,
                        old.clone(),
                        &new,
                        true,
                        |_| Ok(true),
                        &scheduler,
                    );
                    if first.is_err() {
                        scheduler.fail_after.set(None);
                        let bytes = store
                            .read_optional(leaf, MAX_MANIFEST_BYTES)
                            .unwrap()
                            .unwrap();
                        let current = serde_json::from_slice(&bytes).unwrap();
                        replace_existing(
                            &store,
                            leaf,
                            current,
                            &new,
                            true,
                            |_| panic!("intent already confirmed"),
                            &scheduler,
                        )
                        .unwrap();
                    }
                    assert_eq!(scheduler.inspect(&new).unwrap(), NativeRegistration::Owned);
                    let native = scheduler.native_definition(&new);
                    assert_eq!(
                        scheduler.loaded.borrow().as_deref(),
                        if kind == SchedulerKind::Launchd && !active {
                            None
                        } else {
                            Some(native.as_str())
                        }
                    );
                    assert_eq!(scheduler.active.get(), active);
                    assert!(
                        scheduler.enabled.get(),
                        "replacement must remain registered for login"
                    );
                    let stored: ServiceManifest =
                        serde_json::from_slice(&fs::read(temp.path().join(leaf)).unwrap()).unwrap();
                    assert!(same_installation(&stored, &new));
                    assert_eq!(stored.phase, ServicePhase::Installed);
                    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), entry_count);
                }
            }
        }
    }

    #[test]
    fn foreign_registration_is_unchanged_before_and_during_recovery() {
        for interrupted in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let store = StoreDirectory::open(&temp.path().canonicalize().unwrap()).unwrap();
            let leaf = Path::new("service.json");
            let (old, new) = definitions(&temp.path().canonicalize().unwrap());
            write_manifest(&store, leaf, &old, Publication::CreateOnly).unwrap();
            let scheduler = SchedulerFixture::new(&old, true, &temp.path().join(leaf));
            if interrupted {
                scheduler.fail_after.set(Some(1));
                assert!(
                    replace_existing(
                        &store,
                        leaf,
                        old.clone(),
                        &new,
                        true,
                        |_| Ok(true),
                        &scheduler
                    )
                    .is_err()
                );
            }
            *scheduler.loaded.borrow_mut() = Some("foreign".into());
            let before = scheduler.calls.get();
            assert!(
                replace_existing(
                    &store,
                    leaf,
                    old,
                    &new,
                    true,
                    |_| panic!("foreign must be rejected before confirmation"),
                    &scheduler
                )
                .is_err()
            );
            assert_eq!(scheduler.calls.get(), before);
            assert_eq!(scheduler.loaded.borrow().as_deref(), Some("foreign"));
        }
    }
}
