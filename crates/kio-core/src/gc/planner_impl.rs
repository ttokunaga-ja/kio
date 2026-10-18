use super::*;

impl GcPlanner {
    pub fn plan_at(&self, now: i64) -> Result<GcPlan> {
        self.plan_at_inner(now, || {})
    }

    pub(super) fn plan_at_inner(
        &self,
        now: i64,
        before_stability_check: impl FnOnce(),
    ) -> Result<GcPlan> {
        self.reads.charge(0)?;
        let root_id = id_file(&self.scope)?;
        let kio_id = id_file(&self.kio)?;
        self.require_layout()?;
        let mut st = GcPlanStats::default();
        let mut observations = BTreeMap::new();
        let scope_bytes = read_accounted_observed(
            &self.kio,
            "scope.json",
            MAX_METADATA,
            &mut st,
            &self.limits,
            &mut observations,
            "scope.json",
        )?;
        let scope_digest = validate_scope_bytes(&scope_bytes)?;
        let config_bytes = read_accounted_observed(
            &self.kio,
            "config.toml",
            MAX_METADATA,
            &mut st,
            &self.limits,
            &mut observations,
            "config.toml",
        )?;
        let (policy, config_digest) = read_policy_bytes(&config_bytes)?;
        let refs = self.read_refs(&mut st, &mut observations)?;
        let receipts = self.read_receipts(&mut st, &mut observations)?;
        let all = self.inventory_commits(&mut st, &mut observations)?;
        validate_commit_links(&all)?;
        validate_receipt_links(&receipts, &all)?;
        self.validate_markerless_shallow_state(&refs, &receipts, &all)?;
        let mut sizes = HashMap::new();
        for (h, c) in &all {
            if !receipts.contains_key(h) && !sizes.contains_key(&c.tree) {
                sizes.insert(
                    c.tree.clone(),
                    self.verify_tree(&c.tree, &mut st, &mut observations)?,
                );
            }
        }
        let mut ex = BTreeMap::new();
        let pairs = retention_candidate_pairs(
            &policy,
            &refs,
            &all,
            &receipts,
            now,
            &mut st,
            &self.limits,
            &mut ex,
        )?;
        let mut candidates = Vec::with_capacity(pairs.len());
        for (h, tree_hash) in pairs {
            let c = &all[&h];
            let size_bytes = sizes
                .get(&tree_hash)
                .copied()
                .ok_or_else(|| corrupt("candidate tree was not verified"))?;
            candidates.push(GcCandidate {
                commit_hash: h,
                tree_hash,
                commit_type: c.commit_type,
                created_at: c.created_at.clone(),
                policy: if c.commit_type == CommitType::Auto {
                    "auto_retention".into()
                } else {
                    "derived_retention".into()
                },
                size_bytes,
            });
        }
        candidates.sort_by(|a, b| a.commit_hash.cmp(&b.commit_hash));
        let unique: BTreeSet<_> = candidates.iter().map(|c| c.tree_hash.clone()).collect();
        let estimated = unique.iter().try_fold(0u64, |n, t| {
            n.checked_add(sizes.get(t).copied().unwrap_or(0))
                .ok_or_else(|| limit("estimated bytes"))
        })?;
        before_stability_check();
        let stability_check_stats = self.require_stable_truth(&TruthSnapshot {
            scope_digest: scope_digest.clone(),
            config_digest: config_digest.clone(),
            policy: policy.clone(),
            refs: refs.clone(),
            receipts: receipts.clone(),
            commits: all.clone(),
            tree_sizes: sizes.clone(),
            observations: observations.clone(),
        })?;
        self.recheck(root_id, kio_id)?;
        let truth_digest = semantic_truth_digest(
            &scope_digest,
            &config_digest,
            &policy,
            &refs,
            &receipts,
            &all,
            &sizes,
            &observations,
        )?;
        let stable_truth_digest = semantic_stable_truth_digest(
            &scope_digest,
            &config_digest,
            &policy,
            &refs,
            &all,
            &observations,
        )?;
        let baseline_receipts_digest = receipt_observation_digest(&receipts, &observations)?;
        let plan_digest = semantic_plan_digest(&truth_digest, &policy, &candidates)?;
        Ok(GcPlan {
            status: "dry_run".into(),
            as_of: format_utc_seconds(now),
            scope_path: self.root.display().to_string(),
            policy,
            limits: self.limits.clone(),
            stats: st,
            stability_check_stats,
            candidate_count: candidates.len() as u64,
            candidate_tree_count: unique.len() as u64,
            estimated_bytes: estimated,
            candidates,
            exclusions: ex
                .into_iter()
                .map(|(reason, count)| GcExclusion { reason, count })
                .collect(),
            object_kinds_planned: vec!["tree".into()],
            truth_digest,
            stable_truth_digest,
            baseline_receipts_digest,
            plan_digest,
        })
    }
    pub(super) fn recheck(&self, r: Identity, k: Identity) -> Result<()> {
        if id_file(&self.scope)? != r
            || id_file(&self.kio)? != k
            || id_path(&self.root)? != r
            || id_child(&self.scope, ".kio")? != k
        {
            Err(corrupt("scope root changed while planning"))
        } else {
            Ok(())
        }
    }
    pub(super) fn require_layout(&self) -> Result<()> {
        for d in [
            "refs",
            "refs/tags-v1",
            "objects",
            "objects/commits",
            "objects/trees",
        ] {
            let _ = open_path(&self.kio, d)?;
        }
        Ok(())
    }
    pub(super) fn read_refs(
        &self,
        s: &mut GcPlanStats,
        observations: &mut BTreeMap<String, FileObservation>,
    ) -> Result<BTreeMap<String, String>> {
        let mut o = BTreeMap::new();
        let h = String::from_utf8(read_accounted_observed(
            &self.kio,
            "HEAD",
            MAX_REF,
            s,
            &self.limits,
            observations,
            "HEAD",
        )?)
        .map_err(|_| corrupt("ref is not utf8"))?;
        if h == "unborn\n" {
            // The only valid empty-history HEAD encoding.
        } else if let Some(hash) = h.strip_suffix('\n') {
            add_ref(&mut o, "HEAD".into(), hash, s, &self.limits)?
        } else {
            return Err(corrupt("HEAD is not newline-terminated"));
        }
        let refs = open_path(&self.kio, "refs")?;
        for (dir, tag) in [("tags-v1", true)] {
            let d = open_dir(&refs, dir)?;
            for n in names(&d, s, &self.limits, 2)? {
                if tag && n == "names.jsonl" {
                    let _ = read_accounted_observed(
                        &d,
                        &n,
                        MAX_METADATA,
                        s,
                        &self.limits,
                        observations,
                        &format!("refs/{dir}/{n}"),
                    )?;
                    continue;
                }
                if tag && !(n.len() == 68 && n.starts_with("tag-") && hex(&n[4..])) {
                    return Err(corrupt("invalid tag ref leaf"));
                }
                let v = String::from_utf8(read_accounted_observed(
                    &d,
                    &n,
                    MAX_REF,
                    s,
                    &self.limits,
                    observations,
                    &format!("refs/{dir}/{n}"),
                )?)
                .map_err(|_| corrupt("ref is not utf8"))?;
                if v.trim().is_empty() {
                    return Err(corrupt("ref is empty"));
                } else {
                    add_ref(&mut o, format!("{dir}/{n}"), v.trim(), s, &self.limits)?;
                }
            }
        }
        Ok(o)
    }
    pub(super) fn read_receipts(
        &self,
        s: &mut GcPlanStats,
        observations: &mut BTreeMap<String, FileObservation>,
    ) -> Result<HashMap<String, String>> {
        let gc = match open_optional_dir(&self.kio, "gc")? {
            Some(directory) => directory,
            None => return Ok(HashMap::new()),
        };
        let d = match open_optional_dir(&gc, "shallowed")? {
            Some(directory) => directory,
            None => return Ok(HashMap::new()),
        };
        let mut o = HashMap::new();
        for n in names(&d, s, &self.limits, 3)? {
            if n.len() != 64 || !hex(&n) {
                return Err(corrupt("invalid shallow receipt leaf"));
            }
            let bytes = read_accounted_observed(
                &d,
                &n,
                MAX_METADATA,
                s,
                &self.limits,
                observations,
                &format!("gc/shallowed/{n}"),
            )?;
            let r = ShallowReceipt::parse_canonical(&bytes, &n)?;
            if o.insert(r.commit_hash, r.tree_hash).is_some() {
                return Err(corrupt("invalid shallow receipt"));
            }
            s.receipts = checked(s.receipts, 1, "receipts")?;
            if s.receipts > self.limits.max_receipts {
                return Err(limit("receipts"));
            }
        }
        Ok(o)
    }
    pub(super) fn inventory_commits(
        &self,
        s: &mut GcPlanStats,
        observations: &mut BTreeMap<String, FileObservation>,
    ) -> Result<HashMap<String, CommitObject>> {
        self.reads.charge(0)?;
        let b = open_path(&self.kio, "objects/commits")?;
        let mut o = HashMap::new();
        for a in names(&b, s, &self.limits, 2)? {
            if a.len() != 2 || !hex(&a) {
                return Err(corrupt("invalid commit fanout"));
            }
            let ad = open_dir(&b, &a)?;
            for bb in names(&ad, s, &self.limits, 3)? {
                if bb.len() != 2 || !hex(&bb) {
                    return Err(corrupt("invalid commit fanout"));
                }
                let bd = open_dir(&ad, &bb)?;
                for n in names(&bd, s, &self.limits, 4)? {
                    if n.len() != 64 || !hex(&n) || n[..2] != a || n[2..4] != bb {
                        return Err(corrupt("invalid commit leaf"));
                    }
                    let x = read_cas_accounted_observed(
                        &self.reads,
                        (&bd, &n, MAX_COMMIT_OBJECT_BYTES),
                        s,
                        &self.limits,
                        observations,
                        &format!("commit/{a}/{bb}/{n}"),
                    )?;
                    let h = format!("sha256:{n}");
                    if hash_bytes(&x) != h {
                        return Err(corrupt("commit hash mismatch"));
                    }
                    let c: CommitObject =
                        serde_json::from_slice(&x).map_err(|_| corrupt("invalid commit object"))?;
                    c.validate().map_err(|_| corrupt("invalid commit object"))?;
                    if o.insert(h, c).is_some() {
                        return Err(corrupt("duplicate commit"));
                    }
                    s.commits = checked(s.commits, 1, "commits")?;
                    if s.commits > self.limits.max_commits {
                        return Err(limit("commits"));
                    }
                }
            }
        }
        Ok(o)
    }
    fn verify_tree(
        &self,
        h: &str,
        s: &mut GcPlanStats,
        observations: &mut BTreeMap<String, FileObservation>,
    ) -> Result<u64> {
        self.reads.charge(0)?;
        let raw = h
            .strip_prefix("sha256:")
            .ok_or_else(|| corrupt("invalid tree hash"))?;
        if raw.len() != 64 || !hex(raw) {
            return Err(corrupt("invalid tree hash"));
        }
        let b = open_path(&self.kio, "objects/trees")?;
        let a = open_required_dir(&b, &raw[..2], "tree object fanout is missing")?;
        let d = open_required_dir(&a, &raw[2..4], "tree object fanout is missing")?;
        let x = read_required_accounted_observed(
            &self.reads,
            (&d, raw, MAX_TREE_OBJECT_BYTES),
            "tree object is missing",
            s,
            &self.limits,
            observations,
            &format!("tree/{}/{}/{}", &raw[..2], &raw[2..4], raw),
        )?;
        if hash_bytes(&x) != h {
            return Err(corrupt("tree hash mismatch"));
        }
        let t: TreeObject =
            serde_json::from_slice(&x).map_err(|_| corrupt("invalid tree object"))?;
        if t.entries.len() > MAX_TREE_ENTRIES {
            return Err(corrupt("tree entry limit exceeded"));
        }
        t.validate().map_err(|_| corrupt("invalid tree object"))?;
        s.trees_verified = checked(s.trees_verified, 1, "trees")?;
        s.tree_entries = checked(s.tree_entries, t.entries.len() as u64, "tree entries")?;
        if s.tree_entries > self.limits.max_tree_entries {
            return Err(limit("tree entries"));
        }
        Ok(x.len() as u64)
    }

    /// Outside an active, strictly validated operation marker, a receipt must
    /// explain an already-removed tree and may never name a current ref tip.
    /// This prevents a handcrafted receipt from suppressing a live tree.
    fn validate_markerless_shallow_state(
        &self,
        refs: &BTreeMap<String, String>,
        receipts: &HashMap<String, String>,
        commits: &HashMap<String, CommitObject>,
    ) -> Result<()> {
        self.reads.charge(0)?;
        if read_active_marker_bound(&self.kio, &self.control)?.is_some() {
            return Ok(());
        }
        for (commit, tree) in receipts {
            if refs.values().any(|tip| tip == commit) {
                return Err(corrupt(
                    "shallow receipt names a current ref tip without active GC marker",
                ));
            }
            let raw = tree
                .strip_prefix("sha256:")
                .ok_or_else(|| corrupt("invalid shallow receipt tree"))?;
            let trees = open_path(&self.kio, "objects/trees")?;
            // Preserve the Rust 2021 lifetime of the directory capabilities used
            // for this object read instead of relying on tail-expression drops.
            let present = {
                let first = open_optional_dir(&trees, &raw[..2])?;
                let second = match first.as_ref() {
                    Some(first) => open_optional_dir(first, &raw[2..4])?,
                    None => None,
                };
                match second.as_ref() {
                    Some(dir) => match read_cas_regular_observed(
                        &self.reads,
                        dir,
                        raw,
                        MAX_TREE_OBJECT_BYTES,
                    ) {
                        Ok(_) => true,
                        Err(error) if is_io_not_found(&error) => false,
                        Err(error) => return Err(error),
                    },
                    None => false,
                }
            };
            if present {
                return Err(corrupt(
                    "markerless shallow receipt coexists with tree object",
                ));
            }
            let current = commits
                .get(commit)
                .ok_or_else(|| corrupt("shallow receipt commit is missing"))?;
            if &current.tree != tree {
                return Err(corrupt("shallow receipt tree differs from commit"));
            }
        }
        Ok(())
    }

    fn require_stable_truth(&self, expected: &TruthSnapshot) -> Result<GcPlanStats> {
        let mut stats = GcPlanStats::default();
        let mut observations = BTreeMap::new();
        let scope_bytes = read_accounted_observed(
            &self.kio,
            "scope.json",
            MAX_METADATA,
            &mut stats,
            &self.limits,
            &mut observations,
            "scope.json",
        )?;
        let scope_digest = validate_scope_bytes(&scope_bytes)?;
        let config_bytes = read_accounted_observed(
            &self.kio,
            "config.toml",
            MAX_METADATA,
            &mut stats,
            &self.limits,
            &mut observations,
            "config.toml",
        )?;
        let (policy, config_digest) = read_policy_bytes(&config_bytes)?;
        let refs = self.read_refs(&mut stats, &mut observations)?;
        let receipts = self.read_receipts(&mut stats, &mut observations)?;
        let commits = self.inventory_commits(&mut stats, &mut observations)?;
        validate_commit_links(&commits)?;
        validate_receipt_links(&receipts, &commits)?;
        self.validate_markerless_shallow_state(&refs, &receipts, &commits)?;

        let mut tree_sizes = HashMap::new();
        for (commit_hash, commit) in &commits {
            if !receipts.contains_key(commit_hash) && !tree_sizes.contains_key(&commit.tree) {
                tree_sizes.insert(
                    commit.tree.clone(),
                    self.verify_tree(&commit.tree, &mut stats, &mut observations)?,
                );
            }
        }
        if scope_digest != expected.scope_digest
            || config_digest != expected.config_digest
            || policy != expected.policy
            || refs != expected.refs
            || receipts != expected.receipts
            || commits != expected.commits
            || tree_sizes != expected.tree_sizes
            || observations != expected.observations
        {
            return Err(corrupt("store truth changed while planning GC"));
        }
        Ok(stats)
    }
}
