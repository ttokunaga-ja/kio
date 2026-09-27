# V1 orphan prune recovery contract

Status: implementation and focused macOS regression passed on 2026-09-27; full workspace, new-candidate security review and native Actions acceptance remain pending.

The authoritative behavior is [10-operations.md §7.5.1](../docs/10-operations.md#751-kio-repair-verify-objects-fsck-相当);
[PB14–PB17](step4b-contract-tests-p2b.md#pb14-staging-root-と-normalized-private-stage-の分類-p0) records the focused cases.

`kio repair verify-objects --prune-orphans` must reject the entire plan for scope-wide in-flight ledger rows,
non-terminal tasks, active publication / atomic / purge work, or unfinalized projections. Published, valid
retryable failed manifests protect their references without independently implying unfinished publication.
Unknown-task staging retains both the nonempty all-generations-terminal exit and explicit confirmed locked
repair with all global blockers absent. Malformed or foreign descriptors and unsafe filesystem entries block.

Ordinary `kio repair verify-objects` and prune share strict classification of purge-explained missing
pre-resurrection normalized history. The serialized `purge_explained_missing_history_count` counts each old
reachable commit normalize reference, not distinct objects or units. With no other findings or incomplete
state, status is `ok`; a positive count still sets `external_pointers_may_be_affected=true` and does not
promise that all history is recoverable. The exception requires validated canonical purge / lifecycle evidence
and the exact old NormalizeRef, including manifest_hash. Its cutoff is at or before the actual validated
purged / erased event.in_commit, not the later resurrection. Any HEAD / current or post-purge appearance of
the same exact closure blocks the exception; resurrection commits and newer generations are not covered.
A historical tag root alone does not disqualify old history. Validate the entire available manifest structure
and all surviving bodies so the first missing unit cannot hide later corruption. Genuine missing / corruption
and unverified markers remain findings. Replica and image authority reuse the shared classification so
purge → re-ingest can restore search from the valid current closure; erased old closure grants no image
ownership. No prune-only validation bypass is permitted. Public-path regression and shallow-partial runtime
tests for this correction passed in the 71-case CLI contract suite on macOS,
including successful search before and after pruning and rejection of corrupt surviving bodies.

The retained scope lock and opaque removal pins bind preview to apply. Before the first deletion, recheck all
blockers, current live proof, and every still-eligible selected pin. Delete only the originally displayed, still-eligible subset.
Abandoned normalized private stages may be reclaimed; their valid references stay live for that plan, so CAS
reclamation may require a later invocation. Crash quarantine requires a fresh preview, proof, and confirmation;
a quarantine filename is not deletion authority. Errors remain visible; multi-target deletion is not transactional.

Current core RemovalBudget limits are 1,024 retained handles / pins, depth 32, and 1 GiB aggregate
captured physical file sizes. Hash checks reread these files across multiple passes. Proof walks allow
1,000,000 entries; the 1 GiB verification-byte ceiling accounts for direct descriptor, projected manifest / unit,
and immutable-manifest reads plus canonical sizes of successfully loaded units / commits / trees.
Neither byte ceiling guarantees a 1 GiB cumulative physical I/O cap. Shared CAS loaders can hash or reread
data under existing per-object / per-instance limits; failures and repeated validation passes are separately
bounded. The shallow receipt helper uses two bounded passes. Limit failures reject the plan without silent
truncation; automatic batching is not provided.

Focused validation passed: 14 core removal/binding tests, two ledger tests, 33 Markdown validation tests,
16 verifier tests, two image-authority tests and all 71 CLI contract cases. Formatting, diff checks and
all-target strict Clippy for core/pipeline/app/CLI passed. Integrated functional review addressed the
shallow-history regression. Full workspace and new-candidate Daybreak review remain required.
Windows metadata compilation is separate from runtime acceptance; no Windows runtime pass,
production activation or security acceptance is asserted here.
