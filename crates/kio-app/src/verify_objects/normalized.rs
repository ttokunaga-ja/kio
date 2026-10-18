use super::*;

/// Extend the exact-unit index with immutable closures retained after a final
/// shallow sweep.  This intentionally does not consult `chunks.jsonl`: its
/// introduction fields are not authenticated bindings between a chunk and a
/// discarded tree.  Each accepted unit instead comes from a content-addressed
/// manifest and content-addressed body whose full schema/identity closure has
/// been validated by the same loader used for live tree references.
pub(super) fn index_retained_manifest_units(
    store: &ObjectStore,
    kio_dir: &Path,
    explained: &BTreeMap<String, (String, NormalizeRef)>,
    units_by_content_hash: &mut BTreeMap<String, Vec<ValidatedNormalizedInstanceUnit>>,
    state: &mut State,
) -> Result<()> {
    let manifest_hashes = inventory(kio_dir, ContentObjectKind::Manifest.directory(), state)?;
    if state.exceeded_bounds || state.unsafe_namespace {
        return Ok(());
    }
    for manifest_hash in manifest_hashes {
        state.count_object();
        if state.exceeded_bounds {
            return Ok(());
        }
        let bytes = match store.read_content_object_bytes(
            ContentObjectKind::Manifest,
            &manifest_hash,
            MAX_MANIFEST_OBJECT_BYTES,
        ) {
            Ok(bytes) => bytes,
            Err(error) => {
                state.finding(
                    "normalized_closure_corrupt",
                    &manifest_hash,
                    &error.to_string(),
                    &[],
                );
                continue;
            }
        };
        state.add_bytes(bytes.len() as u64);
        if state.exceeded_bounds {
            return Ok(());
        }
        let manifest: NormalizedInstanceManifest = match serde_json::from_slice(&bytes) {
            Ok(manifest) => manifest,
            Err(error) => {
                state.finding(
                    "normalized_closure_corrupt",
                    &manifest_hash,
                    &format!("manifest schema: {error}"),
                    &[],
                );
                continue;
            }
        };
        let canonical = match serde_json::to_value(&manifest)
            .map_err(|error| KioError::schema(error.to_string()))
            .and_then(|value| canonical_json_bytes(&value))
        {
            Ok(canonical) => canonical,
            Err(error) => {
                state.finding(
                    "normalized_closure_corrupt",
                    &manifest_hash,
                    &error.to_string(),
                    &[],
                );
                continue;
            }
        };
        if canonical != bytes {
            state.finding(
                "normalized_closure_corrupt",
                &manifest_hash,
                "manifest is not canonical JSON or has unknown fields",
                &[],
            );
            continue;
        }
        let reference = NormalizeRef {
            tool_profile_hash: manifest.tool_profile_hash.clone(),
            r#gen: manifest.r#gen,
            manifest_hash,
        };
        let instance = match load_pinned_normalized_instance(
            store,
            kio_dir,
            &manifest.raw_hash,
            &reference,
            state,
        ) {
            Ok(instance) => instance,
            Err(PinnedNormalizedError::Missing)
                if explained
                    .get(&reference.manifest_hash)
                    .is_some_and(|(raw, exact)| {
                        raw == &manifest.raw_hash && exact == &reference
                    }) =>
            {
                match validate_purge_explained_missing_closure(
                    store,
                    kio_dir,
                    &manifest.raw_hash,
                    &reference,
                    state,
                ) {
                    Ok(()) => continue,
                    Err(error) => {
                        state.finding(
                            "normalized_closure_corrupt",
                            &reference.manifest_hash,
                            &error.to_string(),
                            &[],
                        );
                        continue;
                    }
                }
            }
            Err(error) => {
                state.finding(
                    "normalized_closure_corrupt",
                    &reference.manifest_hash,
                    &error.to_string(),
                    &[],
                );
                continue;
            }
        };
        for unit in instance.units {
            let content_hash = hash_bytes(unit.markdown.as_bytes());
            let candidates = units_by_content_hash.entry(content_hash).or_default();
            if !candidates.iter().any(|candidate| {
                candidate.raw_hash == instance.manifest.raw_hash
                    && candidate.tool_profile_hash == instance.manifest.tool_profile_hash
                    && candidate.r#gen == instance.manifest.r#gen
                    && candidate.unit.unit_key == unit.unit_key
            }) {
                candidates.push(ValidatedNormalizedInstanceUnit {
                    raw_hash: instance.manifest.raw_hash.clone(),
                    tool_profile_hash: instance.manifest.tool_profile_hash.clone(),
                    r#gen: instance.manifest.r#gen,
                    unit,
                });
            }
        }
    }
    Ok(())
}

/// Verify the immutable closure named by one tree `NormalizeRef`. This never
/// opens `objects/normalized_units/`: that path-named representation is a
/// mutable current cache and cannot authenticate a historical snapshot.
fn read_pinned_normalized_manifest(
    store: &ObjectStore,
    kio_dir: &Path,
    raw_hash: &str,
    normalize: &NormalizeRef,
    state: &mut State,
) -> std::result::Result<
    (NormalizedInstanceManifest, NormalizedInstanceIdentity, u64),
    PinnedNormalizedError,
> {
    state.count_object();
    if state.exceeded_bounds {
        return Err(PinnedNormalizedError::Corrupt(
            "verification object bound exceeded".to_owned(),
        ));
    }
    // This path is diagnostic only. The retained store is the read authority.
    let manifest_path = kio_dir
        .join("objects/manifests")
        .join(&normalize.manifest_hash);
    let manifest_bytes = store
        .read_content_object_bytes(
            ContentObjectKind::Manifest,
            &normalize.manifest_hash,
            MAX_MANIFEST_OBJECT_BYTES,
        )
        .map_err(|error| {
            if is_store_not_found(&error) {
                PinnedNormalizedError::Missing
            } else {
                PinnedNormalizedError::Corrupt(error.to_string())
            }
        })?;
    state.add_bytes(manifest_bytes.len() as u64);
    if state.exceeded_bounds {
        return Err(PinnedNormalizedError::Corrupt(
            "verification byte bound exceeded".to_owned(),
        ));
    }
    if hash_bytes(&manifest_bytes) != normalize.manifest_hash {
        return Err(PinnedNormalizedError::Corrupt(
            "pinned manifest bytes do not match their CAS hash".to_owned(),
        ));
    }
    let manifest: NormalizedInstanceManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| PinnedNormalizedError::Corrupt(format!("manifest schema: {error}")))?;
    // CAS manifests are canonical JCS. Comparing the exact bytes both proves
    // canonical serialization and makes serde's otherwise permissive unknown
    // field behavior fail closed for this durable schema.
    let canonical_manifest = canonical_json_bytes(
        &serde_json::to_value(&manifest)
            .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?,
    )
    .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
    if canonical_manifest != manifest_bytes {
        return Err(PinnedNormalizedError::Corrupt(
            "manifest is not canonical JSON or has unknown fields".to_owned(),
        ));
    }
    let identity = NormalizedInstanceIdentity {
        raw_hash: raw_hash.to_owned(),
        tool_profile_hash: normalize.tool_profile_hash.clone(),
        r#gen: normalize.r#gen,
    };
    if manifest.raw_hash != identity.raw_hash
        || manifest.tool_profile_hash != identity.tool_profile_hash
        || manifest.r#gen != identity.r#gen
    {
        return Err(PinnedNormalizedError::Corrupt(
            "manifest identity does not match its tree normalization reference".to_owned(),
        ));
    }
    kio_pipeline::markdownize::validate_normalized_manifest(&manifest_path, &identity, &manifest)
        .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
    Ok((manifest, identity, manifest_bytes.len() as u64))
}

pub(super) fn load_pinned_normalized_instance(
    store: &ObjectStore,
    kio_dir: &Path,
    raw_hash: &str,
    normalize: &NormalizeRef,
    state: &mut State,
) -> std::result::Result<ValidatedNormalizedInstance, PinnedNormalizedError> {
    let (manifest, identity, manifest_bytes) =
        read_pinned_normalized_manifest(store, kio_dir, raw_hash, normalize, state)?;
    let manifest_path = kio_dir
        .join("objects/manifests")
        .join(&normalize.manifest_hash);
    // Identify an actual absence before entering the shared loader so only a
    // missing historical closure, not any other loader error, can receive the
    // narrow lifecycle explanation below.
    for entry in manifest
        .units
        .iter()
        .filter(|entry| entry.status == UnitStatus::Done)
    {
        let hash = entry.unit_object_hash.as_deref().ok_or_else(|| {
            PinnedNormalizedError::Corrupt(
                "done manifest entry has no normalized unit object hash".to_owned(),
            )
        })?;
        state.count_object();
        if state.exceeded_bounds {
            return Err(PinnedNormalizedError::Corrupt(
                "verification object bound exceeded".to_owned(),
            ));
        }
        match store.inspect_content_accounted(ContentObjectKind::NormalizedUnit, hash) {
            Ok(metadata) => state.add_bytes(metadata.size_bytes),
            Err(error) => {
                state.add_bytes(error.consumed_bytes);
                return Err(if is_store_not_found(&error.error) {
                    PinnedNormalizedError::Missing
                } else {
                    PinnedNormalizedError::Corrupt(error.error.to_string())
                });
            }
        }
        if state.exceeded_bounds {
            return Err(PinnedNormalizedError::Corrupt(
                "verification byte bound exceeded".to_owned(),
            ));
        }
    }
    let units = load_validated_normalized_units_from_manifest(store, &manifest)
        .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
    // The loader has already compared each stored body with canonical JSON.
    // Count those exact canonical bytes for this tree-reachable closure.
    let mut verified_unit_bytes = 0_u64;
    for unit in &units {
        let canonical = canonical_json_bytes(
            &serde_json::to_value(unit)
                .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?,
        )
        .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
        let bytes = canonical.len() as u64;
        verified_unit_bytes = verified_unit_bytes.saturating_add(bytes);
        // The shared loader performs digest inspection and a bounded body read.
        state.add_bytes(bytes.saturating_mul(2));
        if state.exceeded_bounds {
            return Err(PinnedNormalizedError::Corrupt(
                "verification byte bound exceeded".to_owned(),
            ));
        }
    }
    validate_normalized_instance(&manifest_path, &identity, &manifest, &units)
        .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
    Ok(ValidatedNormalizedInstance {
        manifest,
        units,
        verified_bytes: manifest_bytes + verified_unit_bytes,
    })
}

/// Validate all surviving bodies even when an earlier body was purged.
/// Whole-manifest validation precedes the absence exception.
pub(super) fn load_surviving_normalized_units(
    store: &ObjectStore,
    manifest: &NormalizedInstanceManifest,
    state: &mut State,
) -> std::result::Result<
    (Vec<kio_pipeline::markdownize::NormalizedUnitObject>, bool),
    PinnedNormalizedError,
> {
    let identity = NormalizedInstanceIdentity {
        raw_hash: manifest.raw_hash.clone(),
        tool_profile_hash: manifest.tool_profile_hash.clone(),
        r#gen: manifest.r#gen,
    };
    kio_pipeline::markdownize::validate_normalized_manifest(
        "historical manifest",
        &identity,
        manifest,
    )
    .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
    let mut single = manifest.clone();
    single.units.clear();
    let mut units = Vec::new();
    let mut missing = false;
    for entry in manifest
        .units
        .iter()
        .filter(|entry| entry.status == UnitStatus::Done)
    {
        state.count_object();
        if state.exceeded_bounds {
            return Err(PinnedNormalizedError::Corrupt(
                "historical closure verification bound exceeded".into(),
            ));
        }
        let hash = entry
            .unit_object_hash
            .as_deref()
            .expect("validated manifest pin");
        match store.inspect_content_accounted(ContentObjectKind::NormalizedUnit, hash) {
            Ok(metadata) => state.add_bytes(metadata.size_bytes),
            Err(error) => {
                state.add_bytes(error.consumed_bytes);
                if is_store_not_found(&error.error) {
                    missing = true;
                    continue;
                }
                return Err(PinnedNormalizedError::Corrupt(error.error.to_string()));
            }
        }
        if state.exceeded_bounds {
            return Err(PinnedNormalizedError::Corrupt(
                "historical closure verification bound exceeded".into(),
            ));
        }
        single.units.clear();
        single.units.push(entry.clone());
        let loaded = load_validated_normalized_units_from_manifest(store, &single)
            .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
        for unit in &loaded {
            let bytes = canonical_json_bytes(
                &serde_json::to_value(unit)
                    .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?,
            )
            .map_err(|error| PinnedNormalizedError::Corrupt(error.to_string()))?;
            state.add_bytes((bytes.len() as u64).saturating_mul(2));
        }
        if state.exceeded_bounds {
            return Err(PinnedNormalizedError::Corrupt(
                "historical closure verification bound exceeded".into(),
            ));
        }
        units.extend(loaded);
    }
    Ok((units, missing))
}

pub(super) fn validate_purge_explained_missing_closure(
    store: &ObjectStore,
    kio_dir: &Path,
    raw_hash: &str,
    normalize: &NormalizeRef,
    state: &mut State,
) -> std::result::Result<(), PinnedNormalizedError> {
    let (manifest, _, _) =
        match read_pinned_normalized_manifest(store, kio_dir, raw_hash, normalize, state) {
            Ok(value) => value,
            Err(PinnedNormalizedError::Missing) => return Ok(()),
            Err(error) => return Err(error),
        };
    let (_, missing) = load_surviving_normalized_units(store, &manifest, state)?;
    if !missing {
        return Err(PinnedNormalizedError::Corrupt(
            "historical closure changed during verification".into(),
        ));
    }
    Ok(())
}

/// Only exact closures whose every visible appearance predates the explaining
/// purge can borrow its absence authority. Generation numbers may be reused.
pub(super) fn explained_historical_manifests(
    purge: &PurgeState,
    commits: &BTreeMap<String, CommitObject>,
    trees: &BTreeMap<String, TreeObject>,
    reachable: &BTreeSet<String>,
    head: Option<&str>,
    now: &str,
) -> BTreeMap<String, (String, NormalizeRef)> {
    let mut eligible = BTreeMap::new();
    let mut rejected = BTreeSet::new();
    for hash in reachable {
        let Some(tree) = commits.get(hash).and_then(|commit| trees.get(&commit.tree)) else {
            continue;
        };
        for entry in &tree.entries {
            let Some(reference) = &entry.normalize else {
                continue;
            };
            let key = &reference.manifest_hash;
            let same_identity =
                eligible
                    .get(key)
                    .is_none_or(|(raw, previous): &(String, NormalizeRef)| {
                        raw == &entry.raw_hash && previous == reference
                    });
            if head == Some(hash.as_str())
                || !same_identity
                || !purge_explains_old_closure_gap(
                    purge,
                    &entry.raw_hash,
                    hash,
                    commits,
                    trees,
                    reachable,
                    now,
                )
            {
                rejected.insert(key.clone());
                eligible.remove(key);
            } else if !rejected.contains(key) {
                eligible.insert(key.clone(), (entry.raw_hash.clone(), reference.clone()));
            }
        }
    }
    eligible
}

/// Read-only missing-closure classification shared by replica materialization.
/// A successful exception grants no image/unit ownership from the omitted closure.
pub(crate) fn purge_explains_missing_retained_instance(
    repo: &Repository,
    raw_hash: &str,
    normalize: &NormalizeRef,
) -> Result<bool> {
    let session = GcSweepSession::bind_repository(repo)?;
    let directory = kio_core::store_dir::StoreDirectory::from_retained_with_control(
        session.retained_kio_handle()?,
        repo.kio_dir().to_path_buf(),
        repo.control().clone(),
    )?;
    let purge = PurgeState::from_directory(directory);
    if purge.barrier_blocks(raw_hash)? {
        return Ok(false);
    }
    let shallow = session.validated_final_shallow_receipts()?;
    let store = repo.object_store();
    let mut state = State::default();
    let mut commits = BTreeMap::new();
    let mut trees = BTreeMap::new();
    let mut reachable = BTreeSet::new();
    let mut queue: VecDeque<_> = repo.current_ref_targets()?.into_iter().collect();
    while let Some(hash) = queue.pop_front() {
        if !reachable.insert(hash.clone()) {
            continue;
        }
        state.count_object();
        let commit = repo.read_commit(&hash)?;
        state.add_bytes(
            canonical_json_bytes(
                &serde_json::to_value(&commit)
                    .map_err(|error| KioError::schema(error.to_string()))?,
            )?
            .len() as u64,
        );
        queue.extend(commit.parent.iter().cloned());
        match repo.read_tree(&commit.tree) {
            Ok(tree) => {
                if shallow.contains_key(&hash) {
                    return Err(KioError::schema("shallow receipt coexists with tree"));
                }
                state.add_bytes(
                    canonical_json_bytes(
                        &serde_json::to_value(&tree)
                            .map_err(|error| KioError::schema(error.to_string()))?,
                    )?
                    .len() as u64,
                );
                trees.insert(commit.tree.clone(), tree);
            }
            Err(error)
                if is_store_not_found(&error) && shallow.get(&hash) == Some(&commit.tree) => {}
            Err(error) => return Err(error),
        }
        commits.insert(hash, commit);
        if state.exceeded_bounds {
            return Err(KioError::schema(
                "historical authority verification bound exceeded",
            ));
        }
    }
    let authority = explained_historical_manifests(
        &purge,
        &commits,
        &trees,
        &reachable,
        repo.head_commit_hash()?.as_deref(),
        &repo.control().now_utc_seconds(),
    );
    if !authority
        .get(&normalize.manifest_hash)
        .is_some_and(|(raw, reference)| raw == raw_hash && reference == normalize)
    {
        return Ok(false);
    }
    let missing = match read_pinned_normalized_manifest(
        &store,
        repo.kio_dir(),
        raw_hash,
        normalize,
        &mut state,
    ) {
        Err(PinnedNormalizedError::Missing) => true,
        Err(error) => return Err(KioError::schema(error.to_string())),
        Ok((manifest, _, _)) => {
            load_surviving_normalized_units(&store, &manifest, &mut state)
                .map_err(|error| KioError::schema(error.to_string()))?
                .1
        }
    };
    session.assert_public_identity()?;
    Ok(missing)
}

/// A retired marker proves that the closure which existed before its verified
/// resurrection may have been physically removed by purge. It does *not*
/// excuse the resurrection snapshot itself, nor any unrelated later branch.
fn purge_explains_old_closure_gap(
    purge: &PurgeState,
    raw_hash: &str,
    commit_hash: &str,
    commits: &BTreeMap<String, CommitObject>,
    trees: &BTreeMap<String, TreeObject>,
    reachable: &BTreeSet<String>,
    invocation_time: &str,
) -> bool {
    let lookup = canonical_lookup(purge, raw_hash, commits, trees, reachable, invocation_time);
    if lookup.tombstone_error.is_some() || lookup.receipt_error.is_some() {
        return false;
    }
    let Some(canonical) = lookup.canonical else {
        return false;
    };
    if canonical.event.kind != EventKind::Retired {
        return false;
    }
    let Some(resurrection) = canonical.event.resurrection_commit.as_deref() else {
        return false;
    };
    if commit_hash == resurrection || !is_ancestor(commits, commit_hash, resurrection) {
        return false;
    }
    lookup
        .explaining_purge_commit
        .is_some_and(|cutoff| commit_hash == cutoff || is_ancestor(commits, commit_hash, &cutoff))
}
