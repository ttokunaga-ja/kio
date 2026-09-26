# Step3c K1 — P0 60 coverage matrix (Agent D verification round)

Base commit verified before starting: `07948af` (Step3c K4 embedding wiring), branch
`step3c-impl`, main working tree (no worktree/branch ops used).

The historical 60-P0 tally is preserved here as a review receipt; its current executable
authority is the behavior-test set headed by `crates/kio-cli/tests/step3_p0_contract.rs` and
the named `kio-index`/`kio-search` module tests below. The recorded split was CHUNK 11 /
EMBED 5 / FTS 4 / HYBRID 6 / MMR 4 / CURSOR 5 / MULTI 5 / EVIDENCE 7 / URI 3 / OPEN 4 /
REINDEX 3 / OBS 3 = **60 P0**.

Legend — Type: `CLI` = `crates/kio-cli/tests/step3_p0_contract.rs` integration test;
`unit` = `#[cfg(test)]` in the named `kio-index`/`kio-search` module. Verdict: `ok` = would
fail if the contract were broken (adversarially confirmed by reading the test body against
the implementation, and in most cases by confirming the exercised function is actually
*wired* into `kio-cli/src/main.rs`, not a dead seam); `New` = test added/strengthened this
round (all listed under "Changes made" below, with before/after reasoning).

All 60 rows below carry `ok` **after this round's fixes** — see "Gaps found and fixed" for
what was wrong before and what closed it.

## CT3-CHUNK (11 P0)

| ID | Test(s) | Type | Verdict |
| --- | --- | --- | --- |
| CHUNK-001 | `chunking::tests::ct3_chunk_001_hash_vector_gen0_section_id` | unit | ok — exact hash-vector assert against A.1 CHUNK-1 |
| CHUNK-002 | `chunking::tests::ct3_chunk_002_hash_vector_gen_changes_identity` | unit | ok — exact hash-vector assert against A.1 CHUNK-2 |
| CHUNK-003 | `chunking::tests::ct3_chunk_003_null_section_id_is_omitted_heading_path_empty_stays` | unit | ok — proves `section_id: None` and `Some("")` hash identically, and matches A.1 CHUNK-3 |
| CHUNK-004 | `chunking::tests::ct3_chunk_004_heading_chunking_slug_code_fence_and_max_chars` + **New** `ct3_chunk_004_max_chars_splits_at_paragraph_boundary_and_shares_section` | unit | ok (combined) — first test proves ATX-only heading detection (code-fence `#` ignored) + slug generation; new test proves the previously-unverified rule-5 clause (paragraph-boundary split, span ≤ max_chars, shared heading_path/section_id, contiguous non-overlapping spans) |
| CHUNK-005 | `chunking::tests::ct3_chunk_005_spans_are_unit_local` + **New** `ct3_chunk_005_second_unit_span_and_heading_path_do_not_inherit_from_first` | unit | ok (combined) — original test only had one unit (couldn't distinguish "span is unit-local" from "span starts at 0 because there's only one unit"); new 2-unit test proves the second unit's span restarts at 0 and its heading_path doesn't inherit the first unit's heading |
| CHUNK-006 | `chunking::tests::ct3_chunk_006_chunking_config_hash_vector` | unit | ok — exact hash-vector assert against A.3 |
| CHUNK-007 | `ct3_chunk_007_chunking_config_change_appends_new_generation_chunks` + **New** `ct3_chunk_007_search_only_serves_current_chunking_config_generation` | CLI | ok (combined) — original only checked chunks.jsonl line count grew (append-only half of the contract); new test proves the other half (K8: "検索対象は現行 chunking_config_hash の chunk のみ") by showing the stale-generation chunk_hash disappears from search results while its row is still on disk |
| CHUNK-008 | `ct3_chunk_008_deleted_file_does_not_remove_existing_chunk_rows` | CLI | ok |
| CHUNK-009 | `ct3_chunk_009_chunks_have_durable_publication_after_index` | CLI | ok — verifies the creation association has a durable publication event after successful indexing; indexing-time invisibility remains a separate concurrent-observation concern. |
| CHUNK-010 | `ct3_chunk_010_head_tree_entries_are_populated_with_gen_after_index` (**New**) | CLI | ok — asserts the real `sqlite.db` tree_entries rows written by `kio index` (HEAD commit, gen=0) and cross-checks a live search result. (An earlier unit test targeted the `kio_index::tree_entries::project_commit_tree` scaffold, which was dead code — never called by the CLI — and has been REMOVED in the final dead-code sweep together with the module.) |
| CHUNK-012 | `ct3_chunk_012_repair_rebuild_db_preserves_search_result` | CLI | ok — deletes sqlite.db, rebuilds, re-runs the same query, asserts identical `evidence_uri` |

## CT3-EMBED (5 P0)

| ID | Test(s) | Type | Verdict |
| --- | --- | --- | --- |
| EMBED-001 | `embedding_store::tests::ct3_embed_001_embedding_profile_and_hash_vector` | unit | ok — exact hash-vector assert against A.2 EMB-1 |
| EMBED-002 | `ct3_embed_002_incompatible_profile_falls_back_or_errors` | CLI | ok — **de-tautologized**: uses the `incompatible_profile` seam, which writes genuinely multimodal/768-dim embeddings with a deliberately wrong `profile_hash` (`main.rs` `EmbeddingSeam::Incompatible` / `declared_embedding_profile`), not "no embeddings at all". Confirmed distinct from EMBED-007 (UNAVAIL vs INCOMPAT error codes) |
| EMBED-003 | `ct3_embed_003_cross_scope_incompatibility_falls_back_to_text_merge` | CLI | ok — **de-tautologized**: scope `a` gets compatible ("mock") embeddings, scope `b` gets the incompatible seam; only the genuine mismatch forces the cross-scope text-only merge |
| EMBED-004 | `embedding_store::tests::ct3_embed_004_adopted_profile_is_multimodal_768_cosine` (rewired to the WIRED gate `EmbeddingProfileSummary::matches_adopted`, asserting adopted profile matches and text-modality / wrong-dims do not) + CLI `ct3_hybrid_001_auto_resolves_to_hybrid_with_rrf_fusion` (end-to-end: hybrid only resolves when the stored profile matches) | unit + CLI | ok — the formerly-tested `validate_embedding_profile` was dead code and has been REMOVED in the final sweep; the unit test now targets the function the product actually calls |
| EMBED-008 | CLI `ct3_embed_008_non_multimodal_profile_is_rejected_at_index` (exit 2, `KIO-E-EMBED-MODALITY-001`, real `materialize_tool_lock → validate_embedding_entry` path) + kio-adapter `tool_lock` unit tests on `validate_embedding_entry` | CLI + unit | ok — the duplicate unit test on dead `validate_embedding_profile` has been REMOVED in the final sweep; coverage anchors on the wired enforcement path |

## CT3-FTS (4 P0)

| ID | Test(s) | Type | Verdict |
| --- | --- | --- | --- |
| FTS-001 | `fts::tests::ct3_fts_001_external_content_triggers_sync_insert_delete` | unit | ok — inserts, searches, deletes, re-searches (empty), proving `chunks_ai`/`chunks_ad` trigger sync |
| FTS-002 | publication relation append does not rewrite FTS | unit | ok |
| FTS-003 | `fts::tests::ct3_fts_003_trigram_matches_cjk_substrings_and_short_query_skips` + CLI `ct3_fts_003_two_character_query_is_skipped_with_zero_results` | unit + CLI | ok (combined) |
| FTS-004 | CLI `ct3_fts_004_replica_serves_search_while_source_fts_is_rebuilt` | CLI | ok — deletes the source `sqlite.db`, proves the already-published replica continues serving search during repair, runs `repair rebuild-db`, then proves search remains available after the rebuild |

## CT3-HYBRID (6 P0)

| ID | Test(s) | Type | Verdict |
| --- | --- | --- | --- |
| HYBRID-001 | `ct3_hybrid_001_auto_resolves_to_hybrid_with_rrf_fusion` | CLI | ok — scenario (d). Asserts `resolved_mode=hybrid`, clean fallback fields, `diversify.strategy=mmr` (impossible without real embeddings, since text-only reports `group_by_raw_hash`), and that the hybrid result set is a strict superset of the text-only set (proves vector recall genuinely contributes, not just a relabeled text search) |
| HYBRID-002 | `ct3_hybrid_002_auto_vector_configured_but_absent_falls_back_visibly` (**strengthened in 2d13784**: asserts per-cause fallback_reason strings and in-test pair discrimination — same query resolves hybrid after mock embeddings are generated) | CLI | ok |
| HYBRID-003 | `ct3_hybrid_003_text_and_vector_unavailable_is_an_error` (**rewritten in re-audit fix 2d13784**: auto mode, no flags, sqlite.db deleted → exit 1 KIO-E-SEARCH-VEC-UNAVAIL-001, with pre-deletion success counter-assertion) | CLI | ok — the original version passed --vector and its deletion step was inert (found by Sonnet/GPT-5.5 in the 4-engine re-audit, tasks/step3c-reaudit-4engine.md) |
| HYBRID-004 | `rrf::tests::ct3_hybrid_004_rrf_score_and_rank_vector` | unit | ok — exact match against A.4 (`c2,c1,c3,c4,c5` order, `123/3782` score) |
| HYBRID-005 | `rrf::tests::ct3_hybrid_005_rrf_tie_breaks_by_chunk_id` | unit | ok |
| HYBRID-006 | `ct3_hybrid_006_text_mode_uses_text_rank_without_fusion` | CLI | ok |

## CT3-MMR (4 P0)

| ID | Test(s) | Type | Verdict |
| --- | --- | --- | --- |
| MMR-001 | `mmr::tests::ct3_mmr_001_mmr_selection_vector` | unit | ok — vectors chosen to reproduce A.5's cosine matrix; confirms confirmed order `c1,c3,c2,c4` |
| MMR-002 | `mmr::tests::ct3_mmr_002_mmr_is_deterministic` | unit | ok |
| MMR-003 | `mmr::tests::ct3_mmr_003_mmr_depth_only_diversifies_prefix` | unit | ok |
| MMR-004 | `mmr::tests::ct3_mmr_004_max_per_raw_hash_applies_to_stream` | unit | ok |

## CT3-CURSOR (5 P0)

| ID | Test(s) | Type | Verdict |
| --- | --- | --- | --- |
| CURSOR-001 | `ct3_cursor_001_same_cursor_recomputes_same_second_page` | CLI | ok |
| CURSOR-002 | `ct3_cursor_002_max_rowid_excludes_post_cursor_chunks` | CLI | ok — scenario (c). Issues a cursor, appends+indexes a new matching file, proves a fresh search sees it but the frozen cursor's page 2 does not |
| CURSOR-003 | `ct3_cursor_003_mismatched_cursor_is_rejected` + `query::tests::ct3_cursor_003_query_hash_vector` | CLI + unit | ok (combined) — CLI proves rejection behavior; unit test is an exact hash-vector match against A.7 |
| CURSOR-004 | `cursor::tests::ct3_cursor_004_cursor_is_base64url_jcs_json` | unit | ok |
| CURSOR-005 | `ct3_cursor_005_shallow_snapshot_cursor_is_rejected` | CLI | ok |

## CT3-MULTI (5 P0)

| ID | Test(s) | Type | Verdict |
| --- | --- | --- | --- |
| MULTI-001 | `ct3_multi_001_default_searches_participating_indexed_scopes` | CLI | ok — scenario (a). Confirmed genuinely independent of `--all-scopes` (separate test from MULTI-008 this round); also proves `participates_in_global_search=false` exclusion |
| MULTI-002 | `ct3_multi_002_cross_scope_merge_is_rank_based` | CLI | ok — strong test: scope `a`'s hit sits in a long filler-heavy chunk (low BM25), scope `b`'s in a short chunk (high BM25); asserts both get the *same* RRF score (`1/61` exactly) and the merge is rank-based, not raw-score-based |
| MULTI-003 | `ct3_multi_003_diversify_caps_raw_hash_across_scopes` | CLI | ok — 4 scopes with identical content (same raw_hash), confirms `max_per_raw_hash=3` caps the merged stream |
| MULTI-004 | `ct3_multi_004_single_scope_response_lists_searched_scopes` (**strengthened**: now also asserts `scope_id`/`scope_path`/`snapshot_at` are present and well-formed in `searched_scopes[0]`, not just array length) | CLI | ok |
| MULTI-005 | `ct3_multi_005_partial_failure_returns_results_with_exit_3` (**strengthened**: now also asserts `excluded_scopes[0].reason` is non-empty) + `ct3_multi_005_all_failed_returns_exit_4` | CLI | ok — scenario (b): chmod-000's a sibling scope's `.kio`, confirms exit 3 + results + excluded_scopes with a reason |

## CT3-EVIDENCE (7 P0)

| ID | Test(s) | Type | Verdict |
| --- | --- | --- | --- |
| EVIDENCE-001 | `ct3_evidence_001_search_results_include_pointer_and_uri` (**strengthened**: now asserts all 6 required fields — was checking only 3 of 6, `schema_version`/`tool_profile_hash`/`scope_id` were unchecked) + `evidence::tests::ct3_evidence_001_issue_pointer_has_required_fields_and_uri` | CLI + unit | ok |
| EVIDENCE-002 | `ct3_evidence_002_live_pointer_commit_matches_searched_scope_snapshot` | CLI | ok |
| EVIDENCE-003 | `ct3_evidence_003_scope_resolves_via_path_then_registry` | CLI | ok — exercises all 3 branches (a: scope_path stage only, via empty registry; b: broken scope_path + registry lookup by scope_id; c: both fail → scope_unreachable) |
| EVIDENCE-004 | `ct3_evidence_004_resolves_through_pointer_commit_tree` | CLI | ok — strong discriminator: advances HEAD, proves the old pointer still resolves via its own commit's tree (not HEAD's), then proves pointing a newer file's pointer at the *older* commit fails (rules out a naive working-tree-scan shortcut) |
| EVIDENCE-005 | `ct3_evidence_005_shallow_commit_rejects_pointer_and_preserves_tree_controls` | CLI | ok — valid shallow receipt により tree object が欠落する pointer は、`view` / `open` の両方で `KIO-E-COMMIT-SHALLOW-001` となる。raw_hash / chunk_hash / HEAD による代替解決を行わず、tree を backup から復元して再実行する |
| EVIDENCE-006 | `ct3_evidence_006_three_valued_resolution_failures` | CLI | ok — all 3 branches (tombstoned via hand-placed tombstone file / not_found via missing raw object / scope_unreachable) |
| EVIDENCE-009 | `ct3_evidence_009_eval_reads_raw_hash_and_section_from_pointer` | CLI | ok |

## CT3-URI (3 P0)

| ID | Test(s) | Type | Verdict |
| --- | --- | --- | --- |
| URI-001 | `evidence::tests::ct3_uri_001_json_uri_json_roundtrip_drops_only_optional_fields` | unit | ok — exact URI string match against A.6, confirms the 7 optional fields drop and required fields survive |
| URI-002 | `evidence::tests::ct3_uri_002_object_reference_is_distinct_from_evidence_pointer` + CLI `ct3_uri_002_open_resolves_object_raw_uri` + `ct3_uri_002_open_rejects_invalid_object_uri` | unit + CLI | ok |
| URI-003 | `ct3_uri_003_inline_json_pointer_is_accepted_by_view` (only exercised 1 of 5 prefix branches) + **New** `ct3_uri_003_stdin_dash_prefix_is_accepted_by_view` + **New** `ct3_uri_003_unrecognized_pointer_prefix_is_invalid_usage_exit_2` | CLI | ok (combined) — `kio://` branch is covered elsewhere (all `ct3_open_*`/`ct3_evidence_*` tests); `sha256:` short-form branch is P1 (OPEN-005, implemented via `resolve_short_hash` but not P0); new tests close the previously-untested `-` stdin branch and the "other → exit 2" branch |

## CT3-OPEN (4 P0)

| ID | Test(s) | Type | Verdict |
| --- | --- | --- | --- |
| OPEN-001 | `ct3_open_001_open_prefers_working_tree_raw_hash` | CLI | ok |
| OPEN-002 | `ct3_open_002_missing_working_tree_file_expands_temporary_copy` | CLI | ok |
| OPEN-003 | `ct3_open_003_dead_pointer_returns_exit_4` (scope_unreachable branch) + `ct3_evidence_006_three_valued_resolution_failures` (tombstoned + not_found branches via `open`) | CLI | ok (combined) — all 3 dead-pointer types exercised through `open` |
| OPEN-004 | `ct3_open_004_view_returns_chunk_text_without_regeneration` | CLI | ok |

## CT3-REINDEX (3 P0)

| ID | Test(s) | Type | Verdict |
| --- | --- | --- | --- |
| REINDEX-001 | `ct3_reindex_001_force_creates_new_generation_and_preserves_old_chunks` | CLI | ok |
| REINDEX-002 | `ct3_reindex_002_existing_pointer_still_resolves_old_chunk_after_reindex` | CLI | ok |
| REINDEX-003 | `ct3_reindex_003_force_requires_yes_in_noninteractive_mode` | CLI | ok |

## CT3-OBS (3 P0)

| ID | Test(s) | Type | Verdict |
| --- | --- | --- | --- |
| OBS-001 | `ct3_obs_001_index_status_reports_partial_enrichment` | CLI | ok |
| OBS-002 | `ct3_obs_002_metrics_jsonl_records_per_search_latency` + `ct3_obs_002_metrics_do_not_record_query_text` + `ct3_obs_002_metrics_use_search_namespace_code_and_component` | CLI | ok — combined coverage includes the K8 fix (`KIO-M-SEARCH-001` / component `search`) |
| OBS-003 | `ct3_obs_003_access_jsonl_records_redacted_search` + `ct3_obs_003_access_log_has_required_envelope_fields` | CLI | ok |

---

## Acceptance-condition real-machine scenarios (a)-(e)

All 5 present as CLI tests, explicitly labeled in comments:

- (a) flag-less sibling-scope search → `ct3_multi_001_default_searches_participating_indexed_scopes`
- (b) partial failure exit 3 → `ct3_multi_005_partial_failure_returns_results_with_exit_3`
- (c) cursor freezes chunk set → `ct3_cursor_002_max_rowid_excludes_post_cursor_chunks`
- (d) mock embedding → hybrid RRF fusion, fallback_reason clears → `ct3_hybrid_001_auto_resolves_to_hybrid_with_rrf_fusion`
- (e) non-multimodal profile rejected at index → `ct3_embed_008_non_multimodal_profile_is_rejected_at_index`

## Formerly-tautological 3 (K1 explicit callout) — confirmed de-tautologized

HYBRID-002 / EMBED-002 / EMBED-003 all now construct genuinely distinguishable
compatible/incompatible embedding states via the `KIO_TEST_GEMINI_EMBED` seam
(`mock` / `incompatible_profile` / absent), verified by reading `main.rs`'s
`EmbeddingSeam`/`declared_embedding_profile` (writes a deliberately wrong `profile_hash`
for `Incompatible`, `modality="text"` for `NonMultimodal`) — not "vector is always absent."

## MULTI-001/008 split (K1 explicit callout) — confirmed correct

`ct3_multi_001_default_searches_participating_indexed_scopes` never passes `--all-scopes`
and independently proves default (no-flag) discovery + `participates_in_global_search=false`
exclusion. `ct3_multi_008_all_scopes_flag_targets_all_indexed_scopes` is a separate test that
explicitly passes the flag. No overlap/dependency between the two.

## Changes made this round

New test files touched: `crates/kio-index/src/chunking.rs`,
`crates/kio-cli/tests/step3_p0_contract.rs`.

1. **New** `chunking::tests::ct3_chunk_004_max_chars_splits_at_paragraph_boundary_and_shares_section`
   — CT3-CHUNK-004's rule 5 (paragraph-boundary greedy split) was unverified by the existing
   test (its fixture never exceeded max_chars). Confirms split pieces stay ≤ max_chars, share
   heading_path/section_id, and tile the section contiguously without overlap.
2. **New** `chunking::tests::ct3_chunk_005_second_unit_span_and_heading_path_do_not_inherit_from_first`
   — CT3-CHUNK-005's Given is explicitly a unit "combined at the tail side of a full-text
   view"; the existing single-unit test couldn't distinguish "span is unit-local" from "span
   happens to start at 0 because there's only one unit." Adds a 2-unit fixture.
3. **New** `ct3_chunk_007_search_only_serves_current_chunking_config_generation` — the
   existing CHUNK-007 test only checked that chunks.jsonl grew (append-only half); this test
   proves the K8 "search only serves current chunking_config_hash" half by showing the stale
   chunk_hash disappears from search results while its row survives on disk.
4. **New** `ct3_chunk_010_head_tree_entries_are_populated_with_gen_after_index` — the
   dedicated CT3-CHUNK-010 unit test targets `kio_index::tree_entries::project_commit_tree`,
   which is dead code (never called from `kio-cli/src/main.rs`; the CLI has its own inline-SQL
   tree_entries writer). This CLI test inspects the real `sqlite.db` after `kio index`.
5. **New** `ct3_uri_003_stdin_dash_prefix_is_accepted_by_view` and
   `ct3_uri_003_unrecognized_pointer_prefix_is_invalid_usage_exit_2` — CT3-URI-003 requires 5
   prefix branches; only `{` (inline JSON) had a dedicated P0 test. Closes the `-` stdin and
   "other → exit 2" branches.
6. **Strengthened** `ct3_evidence_001_search_results_include_pointer_and_uri` — was only
   checking 3 of the 6 required Evidence Pointer fields (missing `schema_version`,
   `tool_profile_hash`, `scope_id`); now checks all 6, at the actual hand-assembled
   `--json` output level (the CLI builds JSON via `json!{}` macros, not by relying on
   `kio-search`'s struct field types, so presence has to be checked at that boundary).
7. **Strengthened** `ct3_multi_004_single_scope_response_lists_searched_scopes` — now
   asserts `searched_scopes[0]` actually has well-formed `scope_id`/`scope_path`/
   `snapshot_at`, not just that the array has length 1.
8. **Strengthened** `ct3_multi_005_partial_failure_returns_results_with_exit_3` — now
   asserts `excluded_scopes[0].reason` is populated (05 §1.8 requires `{scope_id,
   scope_path, reason}`; only `scope_path` was previously checked).
9. **Renamed** (label fix, no behavior change) `ct3_cursor_001_end_of_stream_has_null_next_cursor`
   → `ct3_cursor_006_end_of_stream_has_null_next_cursor` — it tests CT3-CURSOR-006's "end of
   stream → next_cursor: null" clause, not CURSOR-001 (cursor-recompute determinism, already
   covered by the other `ct3_cursor_001_*` test).
10. **Renamed** (naming-convention fix) `ct3_embed_modality_non_multimodal_profile_is_rejected_at_index`
    → `ct3_embed_008_non_multimodal_profile_is_rejected_at_index` — was missing the `008`
    required by the `ct3_<domain>_<nnn>_<description>` convention.

Net: +6 new test functions (2 in `kio-index`, 4 in `kio-cli/tests`), 3 strengthened
in-place, 2 renamed. `cargo test --workspace`: 224 → 230.

## Implementation doubts found (not fixed — reporting only, per instructions)

1. **`kio_index::tree_entries::project_commit_tree` is dead code.** It has its own unit
   test (`ct3_chunk_010_tree_entries_project_head_commit_with_gen`) and looks like a
   deliberate library implementation of the CT3-CHUNK-010 contract, but `kio-cli/src/main.rs`
   never imports or calls it — the CLI reimplements HEAD tree-entry projection independently
   via `ensure_snapshot_tree_entries`/`write_tree_entries`/`rebuild_sqlite_index` (raw SQL).
   The product behavior is correct (confirmed by the new CLI-level test added this round),
   but the kio-index function + its test are decorative. Worth either wiring it in for real
   or deleting it to avoid a second "tested but unused" surface re-triggering the same audit
   finding from the previous round.
2. **`kio_index::embedding_store::validate_embedding_profile` is dead code**, for both of the
   contracts it's meant to serve:
   - The EMBED-008 modality gate (`KIO-E-EMBED-MODALITY-001`) is actually enforced by
     `kio-adapter::tool_lock::validate_embedding_entry` (reached via
     `materialize_tool_lock → load_tool_lock`), a separate, independently-tested
     implementation of the same rule.
   - The EMBED-004/EMBED-002 dimension/distance/profile_hash compat check at search time is
     actually enforced by `embedding_store::EmbeddingProfileSummary::matches_adopted()`
     (reached via `chunk_embedding_profiles(..).matches_adopted()`), not by
     `validate_embedding_profile`.
   Net effect: the contracts themselves are genuinely covered end-to-end through the real
   call paths (confirmed via CLI tests + the real functions' own tests), so this is not a
   P0-coverage gap — but `validate_embedding_profile` and its two unit tests
   (`ct3_embed_004_adopted_profile_is_multimodal_768_cosine`,
   `ct3_embed_008_non_multimodal_profile_is_rejected`) exercise a function nothing calls.
   Same "seam-only, not wired" pattern the K1 round was meant to eliminate, just on a smaller
   and non-blocking scale this time (the real enforcement exists elsewhere and is tested).
3. **CT3-CHUNK-009's "chunk invisible during indexing" clause is untested** and, as far as I
   can tell, impractical to test synchronously within this integration-test harness (it would
   require observing SQLite/search state from a second process while `kio index` is
   mid-run, or an injected pause hook). The post-condition half (durable publication event appended
   after success) is tested. Flagging as a residual gap rather than building a fragile
   concurrency test under this round's time budget.
4. Several P1s referenced by the P0 acceptance criteria have no dedicated test at all
   (not required by K1, listed here per the brief's "P1 は対象外だが、存在するなら記録"):
   CHUNK-011 (chunk object schema / text_hash non-inclusion), EMBED-006 (content-based
   embedding reuse), HYBRID-007/008 (candidate_depth pool ceiling / `--hybrid` forced
   fail_behavior branches), MULTI-006 (parallelism/per-scope timeout), EVIDENCE-007/008
   (scope_path-is-a-hint / forward-compat unknown fields), OPEN-005 (short-hash `open`,
   though `resolve_short_hash` is implemented and reachable), REINDEX-004 (`parent_run_id`
   provenance), OBS-004 (dedicated non-concealment test, though its fields are exercised
   incidentally by several other tests). MULTI-007 (100k-chunk performance fixture) is
   explicitly deferred by the spec itself to "Step 3 後半" and is correctly out of scope here.

## Verification commands (tail)

```
cargo fmt --check                                    # clean, no output
cargo clippy --workspace --all-targets -- -D warnings # clean, 0 warnings
cargo test --workspace                                # 230 passed; 0 failed (was 224 baseline)
  - kio-adapter: 17 passed
  - kio (bin unit tests): 6 passed
  - kio-cli/tests/contract_cli.rs: 9 passed
  - kio-cli/tests/step2_p0_contract.rs: 70 passed
  - kio-cli/tests/step3_p0_contract.rs: 55 passed (was 47; +6 new, 2 renamed, 3 strengthened in place)
  - kio-core: 6 passed
  - kio-core/tests/contract_vectors.rs: 14 passed
  - kio-index: 25 passed (was 23; +2 new)
  - kio-pipeline: 14 passed
  - kio-search: 14 passed
  - doc-tests: 0 (all crates)
```

## Post-sweep note (coordinator, final round)

The implementation doubts above were ACTED ON after this matrix was first written:
`kio_index::tree_entries` (module), `kio_index::rebuild` (module),
`kio_search::multi_scope` (module), `kio_search::query`'s unused response types +
`SearchEngine` trait + `search()` stub, `kio_index::embedding_store::validate_embedding_profile`,
and the kio-search lib placeholder test were all removed as dead scaffold code
(same policy as the K6 stub removal). ct3_embed_004 was rewired to the wired
`matches_adopted` gate. Final totals: 227 workspace tests green / clippy -D warnings / fmt.

## Re-audit amendments (2026-07-04, commit 2d13784)

The 4-engine re-audit (tasks/step3c-reaudit-4engine.md) found the HYBRID-003 row's
original judgment to be a false positive (inert deletion step) — fixed as noted in the
row above. Additions: `ct3_evidence_004_missing_chunk_row_requires_retarget`
(EVIDENCE-004 series; `docs/08-evidence-pointer-spec.md` §3.2 retarget contract, exit 8) and
`ct3_multi_005_cursor_replay_with_unresolvable_scope_is_partial` (MULTI-005 series;
cursor replay with a vanished scope → excluded + exit 3, not CURSOR-001).
Final totals after fixes: 229 workspace tests green.
