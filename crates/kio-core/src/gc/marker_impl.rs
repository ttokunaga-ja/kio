use super::*;

impl GcInProgressMarker {
    pub fn from_plan(
        plan: &GcPlan,
        sweep_id: String,
        started_at: String,
        index_initial: GcIndexState,
    ) -> Result<Self> {
        let candidates: Vec<_> = plan
            .candidates
            .iter()
            .map(|candidate| GcMarkerCandidate {
                commit_hash: candidate.commit_hash.clone(),
                tree_hash: candidate.tree_hash.clone(),
                size_bytes: candidate.size_bytes,
            })
            .collect();
        let trees = candidates
            .iter()
            .map(|candidate| candidate.tree_hash.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let marker = Self {
            version: 1,
            sweep_id,
            started_at,
            phase: GcSweepPhase::Prepared,
            plan_digest: plan.plan_digest.clone(),
            truth_digest: plan.truth_digest.clone(),
            stable_truth_digest: plan.stable_truth_digest.clone(),
            baseline_receipts_digest: plan.baseline_receipts_digest.clone(),
            operation_receipts_digest: None,
            candidates,
            trees,
            estimated_bytes: plan.estimated_bytes,
            index_initial,
            index_pre_sweep: None,
            index_final: None,
            index_rotation: None,
        };
        marker.validate()?;
        Ok(marker)
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != 1
            || !is_hash(&self.plan_digest)
            || !is_hash(&self.truth_digest)
            || !is_hash(&self.stable_truth_digest)
            || !is_hash(&self.baseline_receipts_digest)
            || !is_canonical_utc_timestamp(&self.started_at)
            || self.sweep_id.is_empty()
            || !is_canonical_ulid(&self.sweep_id)
            || self.candidates.len() > MAX_SWEEP_CANDIDATES
            || self.trees.len() > MAX_SWEEP_CANDIDATES
            || self.estimated_bytes > MAX_SWEEP_ESTIMATED_BYTES
        {
            return Err(corrupt("invalid GC in-progress marker"));
        }
        if self
            .operation_receipts_digest
            .as_ref()
            .is_some_and(|digest| !is_hash(digest))
        {
            return Err(corrupt("invalid GC operation receipt digest"));
        }
        let mut previous = "";
        let mut candidate_trees = BTreeSet::new();
        let mut tree_sizes = BTreeMap::new();
        for candidate in &self.candidates {
            if !is_hash(&candidate.commit_hash)
                || !is_hash(&candidate.tree_hash)
                || candidate.commit_hash.as_str() <= previous
                || candidate.size_bytes > MAX_TREE_OBJECT_BYTES
            {
                return Err(corrupt("GC marker candidates are invalid or unsorted"));
            }
            previous = &candidate.commit_hash;
            candidate_trees.insert(candidate.tree_hash.clone());
            match tree_sizes.insert(candidate.tree_hash.clone(), candidate.size_bytes) {
                Some(previous_size) if previous_size != candidate.size_bytes => {
                    return Err(corrupt("GC marker shared tree sizes differ"));
                }
                _ => {}
            }
        }
        let trees: BTreeSet<_> = self.trees.iter().cloned().collect();
        if trees.len() != self.trees.len()
            || self.trees.windows(2).any(|x| x[0] >= x[1])
            || trees != candidate_trees
            || trees.iter().any(|tree| !is_hash(tree))
        {
            return Err(corrupt("GC marker tree set is invalid"));
        }
        let committed_bytes = tree_sizes.values().try_fold(0u64, |total, size| {
            total
                .checked_add(*size)
                .ok_or_else(|| corrupt("GC marker estimated bytes overflow"))
        })?;
        if committed_bytes != self.estimated_bytes {
            return Err(corrupt("GC marker estimated bytes do not bind candidates"));
        }
        let worst_case = (tree_sizes.len() as u64)
            .checked_mul(MAX_TREE_OBJECT_BYTES)
            .ok_or_else(|| corrupt("GC marker tree bound overflow"))?;
        if worst_case > MAX_SWEEP_ESTIMATED_BYTES {
            return Err(corrupt("GC marker tree count exceeds sweep byte bound"));
        }
        if let GcIndexState::Present {
            generation,
            identity,
        } = &self.index_initial
            && (!is_canonical_ulid(generation) || !is_canonical_gc_index_identity(identity))
        {
            return Err(corrupt("invalid GC marker initial index generation"));
        }
        if let Some(GcIndexState::Present {
            generation,
            identity,
        }) = &self.index_pre_sweep
            && (!is_canonical_ulid(generation) || !is_canonical_gc_index_identity(identity))
        {
            return Err(corrupt("invalid GC marker pre-sweep index generation"));
        }
        if let Some(GcIndexState::Present {
            generation,
            identity,
        }) = &self.index_final
            && (!is_canonical_ulid(generation) || !is_canonical_gc_index_identity(identity))
        {
            return Err(corrupt("invalid GC marker final index generation"));
        }
        let valid_pre_sweep = match (&self.index_initial, &self.index_pre_sweep) {
            (_, None) => matches!(
                self.phase,
                GcSweepPhase::Prepared | GcSweepPhase::Receipting | GcSweepPhase::Sweeping
            ),
            (
                GcIndexState::Present {
                    generation: initial,
                    identity: initial_identity,
                },
                Some(GcIndexState::Present {
                    generation: target,
                    identity: target_identity,
                }),
            ) => initial != target && initial_identity != target_identity,
            (GcIndexState::Absent, Some(GcIndexState::Absent)) => true,
            _ => false,
        };
        let valid_final = matches!(
            (&self.index_initial, &self.index_final),
            (_, None)
                | (GcIndexState::Absent, Some(GcIndexState::Absent))
                | (
                    GcIndexState::Present { .. },
                    Some(GcIndexState::Present { .. })
                )
        );
        let valid_phase_index_state = match self.phase {
            GcSweepPhase::Prepared | GcSweepPhase::Receipting => {
                self.index_pre_sweep.is_none() && self.index_final.is_none()
            }
            GcSweepPhase::Sweeping => self.index_final.is_none(),
            GcSweepPhase::Finalizing => self.index_pre_sweep.is_some(),
        };
        let valid_receipt_binding = match self.phase {
            GcSweepPhase::Prepared | GcSweepPhase::Receipting => {
                self.operation_receipts_digest.is_none()
            }
            GcSweepPhase::Sweeping | GcSweepPhase::Finalizing => {
                self.operation_receipts_digest.is_some()
            }
        };
        if !valid_pre_sweep || !valid_final || !valid_phase_index_state || !valid_receipt_binding {
            return Err(corrupt("GC marker index rotation state is invalid"));
        }
        if let Some(rotation) = &self.index_rotation {
            if !is_valid_gc_index_temp_leaf(&rotation.temp_leaf)
                || !is_canonical_gc_index_identity(&rotation.private_dir_identity)
                || !is_hash(&rotation.source_state_digest)
                || matches!(rotation.source, GcIndexState::Absent)
                || matches!(rotation.target, GcIndexState::Absent)
                || rotation.source == rotation.target
                || !validate_gc_index_state(&rotation.source)
                || !validate_gc_index_state(&rotation.target)
            {
                return Err(corrupt("GC marker index rotation is invalid"));
            }
            let role_matches_phase = matches!(
                (&rotation.role, &self.phase),
                (GcIndexRotationRole::PreSweep, GcSweepPhase::Sweeping)
                    | (GcIndexRotationRole::Final, GcSweepPhase::Finalizing)
            );
            if !role_matches_phase {
                return Err(corrupt("GC marker index rotation role is invalid"));
            }
            let expected_source = match rotation.role {
                GcIndexRotationRole::PreSweep => {
                    self.index_pre_sweep.as_ref().or(Some(&self.index_initial))
                }
                GcIndexRotationRole::Final => {
                    self.index_final.as_ref().or(self.index_pre_sweep.as_ref())
                }
            };
            if expected_source != Some(&rotation.source) {
                return Err(corrupt("GC marker index rotation source is invalid"));
            }
        }
        // Bound the actual canonical on-disk representation, not merely the
        // cardinalities which can vary in JSON escaping/number encoding.
        if serde_jcs::to_vec(self)
            .map_err(|error| corrupt(&error.to_string()))?
            .len()
            .saturating_add(1) as u64
            > MAX_MARKER_BYTES
        {
            return Err(corrupt("GC in-progress marker exceeds byte limit"));
        }
        Ok(())
    }
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut b = canonical_json_bytes(
            &serde_json::to_value(self).map_err(|e| corrupt(&e.to_string()))?,
        )?;
        b.push(b'\n');
        Ok(b)
    }
    pub fn parse_canonical(bytes: &[u8]) -> Result<Self> {
        if bytes.len() as u64 > MAX_MARKER_BYTES
            || !bytes.ends_with(b"\n")
            || bytes[..bytes.len().saturating_sub(1)].contains(&b'\n')
        {
            return Err(corrupt("GC marker is not canonical JCS+LF"));
        }
        let marker: Self = serde_json::from_slice(bytes)
            .map_err(|_| corrupt("malformed GC in-progress marker"))?;
        marker.validate()?;
        if marker.canonical_bytes()? != bytes {
            return Err(corrupt("GC marker is not canonical JCS+LF"));
        }
        Ok(marker)
    }
    #[must_use]
    pub fn is_frozen_pair(&self, commit_hash: &str, tree_hash: &str) -> bool {
        self.candidates
            .iter()
            .any(|c| c.commit_hash == commit_hash && c.tree_hash == tree_hash)
    }
}
