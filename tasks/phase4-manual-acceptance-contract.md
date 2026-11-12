# Phase 4 manual acceptance contract (`v0.1.0-rc.1`)

Status: **historical (2026-11-13, [docs/decisions.md](../docs/decisions.md) D-20261113-03)**. This RC.1 procedure is not a v1 completion condition and is not run again. No manual acceptance was established under it.

This document fixes the procedure for collecting new, distribution-bound manual
evidence for Phase 4 milestones M1--M8. It does not amend the normative product
contracts in `docs/`, authorize Kio internal Full, or convert any historical or
automated result into manual acceptance. `tasks/phase4-current-evidence-audit.md`,
`docs/09-mvp-scope.md`, `docs/10-operations.md`, and
`tasks/manual-full-cold-gates.md` remain authoritative for their respective
boundaries.

## 1. Fixed candidate and distribution binding

Every run must first read and record all of the following values. Any mismatch,
missing object, changed remote ref, archive/sidecar schema failure, size mismatch,
or SHA-256 mismatch is a **binding failure**: create the failure receipt only and
do not extract, execute, or accept any stage.

| field | fixed value |
| --- | --- |
| live `origin/main` | `34d4107aece6aca0350295d68645d511b7388766` |
| tag name | `v0.1.0-rc.1` |
| annotated tag object (`type=tag`) | `8895d0e8eece48b3a99e4d67f2c8d3098edee531` |
| peeled tag target / product candidate commit | `b95efd86d1ee738378edb7171509ae7ca81e8661` |
| candidate Git tree | `a4183c874799ab55d2471b726f9b5dc4dd3eb8d8` |
| candidate `Cargo.lock` SHA-256 | `74059079ef8e69ce3e35c31214c0587616bd4eb6c3199553d5339389fc9ece21` |
| public Release | `https://github.com/ttokunaga-ja/kio/releases/tag/v0.1.0-rc.1` |
| distribution platform | macOS arm64 / `aarch64-apple-darwin` |
| archive name | `kio-0.1.0-rc.1-aarch64-apple-darwin.tar.gz` |
| archive bytes / SHA-256 | `8083094` / `590c41518b83eac8b3ba5dba4006ca5afdffd014ebc521817e804f3e77ddfd8c` |
| sidecar name | `kio-0.1.0-rc.1-aarch64-apple-darwin.checksums.json` |
| sidecar bytes / SHA-256 | `509` / `0ea4bbf4e26ac587653c59408dda65a704c6d075582a0f0eef2730eae20ec45b` |
| extracted product binary path | `bin/kio` |
| extracted binary bytes / SHA-256 | `20603712` / `4bdc913150ecf839f05bac1237360ea2bc1cd48757009e077ead3689c806d02c` |

The archive and sidecar must be re-downloaded from the public GitHub Release into
a newly-created temporary directory for this run. The sidecar's archive and
binary entries, the independently calculated archive/sidecar/binary digests, and
the fixed values above must all agree. The binary extracted from that verified
archive is the sole product under test. A source-tree binary, a Cargo build,
another release asset, an installed copy, or a historical receipt is not a
substitute.

Before extracting the archive, run the canonical `kio-eval release verify` and
save its successful receipt. Its exact command must use the downloaded archive
and sidecar, `--expected-archive-sha256` with the fixed archive SHA-256,
`--source-repo` with a fresh clean checkout at the exact candidate commit,
`--expected-commit` with that commit, and `--expected-lock-sha256` with the fixed
lockfile SHA-256. The receipt records those inputs, the verifier command/exit and
output digests, and successful checks of archive-internal provenance, SBOM,
checksum manifest, and binary binding. `kio-eval` is a release-engineering
dev-only verifier, not the product under test; it cannot replace execution of the
verified public `bin/kio` binary.

## 2. Isolation and evidence invariants

Each replay uses a newly-created disposable fixture root with five independent
subscopes: `m1-m2`, `m8`, `m6-m7`, `m3`, and `m4-m5`. `m1-m2` is frozen after M1
and retained unchanged until M2; no other stage may use or alter it. The other
subscopes are likewise neither adopted nor reused by another run. The fixture
manifest records every root and its regular-file inventory before the first
product invocation.

Each subscope also has its own newly-created private `HOME`, `XDG_CONFIG_HOME`,
`XDG_CACHE_HOME`, and `XDG_DATA_HOME` roots; no two subscopes share them. All
fixture and private roots must be inside the run's disposable parent and must
not point to a user home, an existing Kio scope, a shared cache, PersonaCorpus,
or a repository checkout.
The command environment removes inherited secret, fault-injection, and test-only
variables. `KIO_FIXED_NOW` is prohibited because it is ineffective in the public
release binary. Actual wall-clock UTC timestamps must instead be recorded.

All receipts are create-only. A run must select a fresh evidence root before
writing; an existing file, directory, run identifier, or stage identifier is a
collision and fails closed. Never truncate, append to, rename over, delete, or
modify prior evidence. Fixture/product writes are allowed only where the
applicable milestone contract requires them and only within the disposable
fixture; evidence writes are allowed only below the evidence root.

The evidence root has this fixed layout (one `run-id` per replay):

```text
phase4-manual-evidence/<run-id>/
  run.json
  binding/
    refs.json
    release-download.json
    release-verify/
      command.txt
      stdout.bin
      stdout.sha256
      stderr.bin
      stderr.sha256
      receipt.json
    archive.sha256
    sidecar.sha256
    binary.sha256
    sidecar.json
  fixture/
    manifest.before.json
    digest.before.sha256
    isolation.json
    m1-m2/                 # frozen after M1 through M2
    m8/
    m6-m7/
    m3/
    m4-m5/
  stages/
    M1/ | M8/ | M6/ | M7/ | M3/ | M4/ | M5/ | M2/
      stage.json
      command.txt
      stdout.bin
      stdout.sha256
      stderr.bin
      stderr.sha256
      fixture-manifest.before.json
      fixture-manifest.after.json
      observation-log-manifest.json
      digest.before.sha256
      digest.after.sha256
      result.json
      completion.json
  final.json
```

`run.json` is a create-only start record: it contains `schema_version`, `run_id`,
UTC start time, host platform, the complete fixed binding, disposable root paths,
and `status=running`. `stage.json` is also a create-only start record containing
the stage identifier, predecessor stage identifiers, intended mutation class, and
`status=running` before invocation. Neither start record is a pass or a terminal
result.

`result.json` is required for every started stage and contains the exact argv and
relevant environment (including all HOME/XDG values), cwd, start/end UTC
timestamps, exit code, stdout/stderr SHA-256, fixture-manifest digest before/after,
digest before/after, observed result, applicable stop rule, mandatory
`terminal_status`, and mandatory predicate/assertion results. Binary stdout/stderr
are retained even when empty; their SHA-256 is always recorded.

`completion.json` is the create-only terminal record for every started stage. It
must contain only terminal `status` (`passed`, `failed`, or `blocked`), all
assertion results, and SHA-256 values for every stage input, output, fixture
manifest, observation-log manifest, and `result.json`; it must also repeat the
`result.json` terminal status and reject disagreement. A stage is passed only if
its `completion.json` exists, has `status=passed`, and all required assertions are
true. A `running` record, missing completion, or unreadable completion is never a
pass.

`final.json` must list every applicable stage, its `completion.json` SHA-256, and
its terminal status. It may set `status=manual-accepted` only when every
applicable stage completion exists and is `passed`; otherwise it must not assert
manual acceptance.

The only status strings are:

| status | meaning |
| --- | --- |
| `running` | receipt creation began; no outcome yet |
| `passed` | this stage met its stated manual replay check only |
| `failed` | command or stated check failed; no later stage may run |
| `blocked` | a required external/native-host capability was unavailable or unverified; no later dependent stage may run |
| `not-run` | a stage was intentionally not started because an earlier stop rule fired |
| `manual-unverified` | run-level state when no complete applicable stage set has passed |
| `manual-accepted` | run-level state only after every applicable stage passed under this contract |

No other label (`complete`, `historical-pass`, `automated-verified`, `not planned`,
or an absent receipt) is equivalent to `passed` or `manual-accepted`.

## 3. Required order, dependencies, and stage predicates

The stage sequence is M1, M8, M6, M7, M3, M4, M5, then M2. Each stage starts
only after its required predecessor completion is `passed`, its after-digest is
recorded, and its observed mutation class matches this contract. The sole
exception is the M8 coverage-only blocker defined below: it records
`blocked`, keeps the run `manual-unverified`, and permits the explicitly
independent M6--M7, M3, M4--M5, and M2 sequence to continue. Any other
`failed` or `blocked` completion stops the run and marks remaining stages
`not-run`.

For every stage, record actual UTC wall timestamps, exact command/environment,
fixture and private-XDG manifests, input/output bytes and SHA-256 values, and all
predicate results. Protected manifests cover fixture working files, Kio store,
config, and registry. A product-specified append-only observation log is instead
listed in a separate log manifest with path, before/after digest, and contract
reason; any unlisted, unexplained, or missing-manifest change fails closed.

### M1: retention dry-run in frozen `m1-m2`

Within `m1-m2`, run `kio init --json`, install the exact config below, write and
manifest a first exact `document.md`, and run
`kio index --offline --approve --json`. Replace only `document.md` with a second
manifested byte sequence that has a different raw hash, then run the same index
command again so the old and current commits have different trees. Public
`--approve` is itself a writer even with `--offline`: it publishes the isolated
approval state, adds `adapter.policy.allow_network = true` to the scope config,
and refreshes the isolated `$XDG_CACHE_HOME/kio/aggregator.sqlite`. Treat `init`,
the first index, and the second index as three different closed mutation classes;
a directory-prefix allowance is forbidden. The first index permits only the
fixed `.kio` leaves `.lock`, `HEAD`, `approvals.jsonl`, `config.toml`,
`index/{chunks.jsonl,sqlite.db}`, `manifest.json`, `purge/epoch`,
`quarantine.jsonl`, `refs/heads/main`, `scope.json`, `tasks.jsonl`, and
`tool-lock.json`; hash-sharded descendants of
`objects/{chunks,commits,manifests,normalized,normalized_unit_objects,normalized_units,prepared,raw,toollocks,trees}`;
and exactly the observed private-XDG leaves
`xdg-cache/kio/aggregator.sqlite` and
`xdg-data/kio/{consents.jsonl,consents.lock,cost-ledger.sqlite,cost-ledger.sqlite.write-seq,logs/events.jsonl,logs/scrub.lock,scope-registry.sqlite}`.
The second index permits updates only to the applicable existing metadata,
index/ledger/purge/tool-lock/device-observability/registry/cache leaves in that
set and new hash-sharded CAS descendants caused by the changed document.
`approvals.jsonl`, `consents.jsonl`, and the already-approved config bytes must
have identical before/after digests on the second index. `consents.lock` must be
replaced only as that exact existing lock leaf; it may not be created, removed, or
retyped. Record its observed 97-byte before/after states and require mode `0644`,
`nlink = 1`, different before/after SHA-256 values, no terminal LF, and compact
canonical JSON with exactly `pid`, `token`, and `created_at`: the release sentinel
PID is `4294967295`, the token is 32 lowercase hexadecimal characters, and
`created_at` is a UTC second timestamp. No additional lock or temporary leaf may
remain.
Unlisted regular files, retained SQLite temporary siblings, and `-wal`, `-shm`,
or `-journal` sidecars fail closed. Record the complete observed path set and
change class for each invocation. Before/after manifests can reject a retained
unexpected file but cannot prove that a transient sibling was never created and
deleted within the invocation; state this observation limit rather than replacing
it with a prefix allowance. `--offline`, the empty secret environment, and
`network_allowed = false` remain mandatory. Do not restore the config between the
two indexes, because only the document may be changed in that interval.
Immediately after the second index, restore and re-manifest the exact config block
below, then make no further config or document change before either dry-run.
Record actual UTC times for both index invocations. No time or hour boundary is
required: all-zero
horizons make the retained-auto set empty; the older non-tip tree is therefore a
real candidate while tips remain excluded. Run the identical
`kio gc --dry-run --json` command twice.

The scope-local `.kio/config.toml` bytes are exactly the following block,
including its final LF:

```toml
[gc]
mode = "manual_only"

[gc.auto_retention]
keep_last_hours = 0
keep_hourly_days = 0
keep_daily_weeks = 0
keep_weekly_months = 0
```

The config bytes and both document versions are immutable fixture inputs after
the second index. Both dry-run invocations must exit 0. Each JSON object must
have exactly the top-level keys `status`, `as_of`, `scope_path`, `policy`,
`limits`, `stats`, `stability_check_stats`, `candidate_count`,
`candidate_tree_count`, `estimated_bytes`, `candidates`, `exclusions`,
`object_kinds_planned`, `truth_digest`, `stable_truth_digest`,
`baseline_receipts_digest`, and `plan_digest`. Require `status = "dry_run"`,
`candidate_count = 1`, `candidate_tree_count = 1`, and exactly one old candidate
with the fields `commit_hash`, `tree_hash`, `commit_type`, `created_at`, `policy`,
and `size_bytes` matching recorded fixture truth. Ref tips must be excluded and
`object_kinds_planned` must equal `["tree"]`. The invocation-specific `as_of`
records actual wall time and need not be byte-identical. All other semantic plan
fields must match between the invocations. The
`truth_digest`, `stable_truth_digest`, `baseline_receipts_digest`, and
`plan_digest` must each be well formed and stable across both invocations; they
are not required to equal one another. `stats` and `stability_check_stats` must
each have the exact counter shape and non-negative integer values, but they are
diagnostics and are not required to equal each other: the first pass performs
retention reachability while the independent stable-truth reread does not repeat
that traversal. Each diagnostics object must instead be stable across the two
identical dry-run invocations. Protected manifests must remain unchanged.
Preserve this fixture unchanged until M2.

### M8: independent `m8` inventory coverage boundary

Create a separate `m8` scope with the same public-only `init`, exact
`manual_only`/all-zero config, two different document versions, and two offline
index invocations plus the same post-second-index exact-config restoration used
by M1. Record its old commit/tree as a real retention-GC
candidate, but do not copy or reuse M1 evidence or state. First run the identical
preliminary `kio gc --dry-run --json` command twice. Apply the M1 retention-plan
rules: each diagnostics object has the exact non-negative integer counter shape,
the two diagnostics objects within one output need not equal one another, and
each object must be stable across the two invocations after removing `as_of`.
Both preliminary invocations are read-only. Then run
`kio gc --dry-run --prune-unreachable --json` twice and record both exact JSON
outputs; they must be byte-identical. Each pass must have exactly the top-level
keys `schema_version`, `operation`, `status`, `read_only`, `diagnostic_only`,
`mutation_authority`, `objects`, `summary`, `shallow_boundaries`, `limits`, and
`stats`, with sorted unique objects and the exact fixed summary shape:
`schema_version = 1`, `operation = "unreachable_object_inventory"`,
`status = "dry_run"`, `read_only = true`, `diagnostic_only = true`,
`mutation_authority = false`, and independent inventory/stability pass stats.
Unlike the preliminary retention planner diagnostics, the two unreachable-
inventory pass stats must be exactly equal: the implementation compares two
independent full scans, including objects, shallow boundaries, stats, and
observations, and rejects any mismatch. This equality is only a read-only
stability gate; it grants no mutation authority and does not weaken
`diagnostic_only = true` or `mutation_authority = false`.
The exact `summary` keys are `object_count`, `physical_bytes`,
`candidate_count`, `candidate_bytes`, `protected_count`, `protected_bytes`,
`inventory_only_count`, and `inventory_only_bytes`.
The complete fixture and private-HOME/XDG digest must be identical before,
between, and after the two invocations; no protected write is allowed.

The public CLI cannot construct a real unreachable candidate without internal CAS
tampering, which is forbidden. Therefore record the truthful
`summary.candidate_count = 0` and terminal
`status = "blocked"` with
`reason = "public_cli_unreachable_candidate_unconstructable"`. This is not an
M8 pass and prevents `manual-accepted`. It is a known coverage-only blocker,
not a safety mismatch, so it permits the independent M6-and-later sequence above.
The recorded old tree must instead appear as `classification = "protected"` and
`reason = "retention_gc_owned"`. Do not tamper with internal CAS state to change
this result or manufacture an orphan manifest, normalized unit, embedding, or
unpublished tool-lock.

### M6: batch verification in `m6-m7`

Create and manifest exact `evidence.md` bytes containing a single unambiguous
heading and the literal token `3600`, then use only public `kio init --json`,
`kio index --offline --approve --json`, and
`kio search "3600" --mode text --json`. Use only
`results[0].evidence_pointer` from that search response. Run both exact mode
pairs: `kio evidence verify <pointer> --json` with
`kio evidence verify --batch <pointers.jsonl> --json`, and
`kio evidence verify <pointer> --strict --json` with
`kio evidence verify --batch <pointers.jsonl> --strict --json`.

The text search appends the existing metrics/access JSONL records with their
existing append-only prefix proof, and creates or updates exactly the two scrub
lock leaves for those logs. Thus its permitted log/lock mutation set is
`$XDG_DATA_HOME/kio/logs/metrics.jsonl`, `$XDG_DATA_HOME/kio/logs/scrub.lock`,
`.kio/logs/access.jsonl`, and `.kio/logs/access.scrub.lock`. Independently of
pagination or the presence of `next_cursor`, search calls `cursor_signing_key()`.
In the fresh private XDG state, first use must create_new the separate fixed
`$XDG_DATA_HOME/kio/cursor-key` path. Record that it was absent before search and
is afterward a regular `nlink = 1`, mode `0600`, exactly-32-byte file with no LF,
plus its SHA-256 and filesystem metadata. Record the explicitly present nullable
`paging.next_cursor` value and its derived issued/not-issued state in the same
receipt without making key creation conditional on that value. Its digest need not be deterministic,
but no additional private file is permitted in this first-search mutation set.

For every subsequent M6 standalone, batch, and strict verification, and for M7
index, log, retarget, and re-verification, this cursor-key's bytes, SHA-256, and
metadata must remain unchanged. This first-use key is a public CLI contract, not a
product defect; it is distinct from the four documented log/lock leaves and is
not excused by a generic private-XDG digest allowance.

The batch is a regular, single-link (`nlink = 1`) JSONL file whose exact bytes are
the same alive pointer on two LF-terminated lines. Record those bytes, final LF,
byte count, and SHA-256. Every invocation must exit 0. Each batch response must
have exactly the seven top-level keys `schema`, `schema_version`, `input_sha256`,
`strict`, `results`, `summary`, and `verified_count`, with
`schema = "kio.evidence.batch-verify"`, `schema_version = 1`, the exact input
SHA-256 and strict flag, all six summary statuses present with every record
`alive`; the exact six `summary.status_counts` keys are `alive`, `tombstoned`,
`not_found`, `scope_unreachable`, `unverifiable`, and `registry_duplicate`, with
only `alive = 2`; and `verified_count = summary.total = 2`. Input order and duplicate
entries must be preserved. Within each same-strictness pair, every
`results[].result` must have byte parity with the standalone status object after
removing only its terminal LF.

### M7: exact retarget in `m6-m7`

Add `unrelated.md`, run `kio index --offline --approve --json`, and obtain
`kio log --json`; require
`commits[0].commit_hash` to match `^sha256:[0-9a-f]{64}$`. The exact
`unrelated.md` bytes are fixed in the fixture manifest. Preserve the
original `evidence.md` bytes and raw hash unchanged. Retarget the old pointer to
that exact returned commit twice using
`kio evidence retarget <pointer> --at <exact-commit-from-log> --json`. Each
retarget result must
have exactly the seven top-level keys `schema`, `schema_version`, `status`,
`target_commit`, `retargeted_from`, `new_pointer`, and `match_method`, with
`status = "retargeted"`, the exact target commit, and
`match_method = "heading_path_exact"`; both outputs must be byte-identical. The
issued pointer's strict verification must be alive and bound to that commit.
During each retarget, the full fixture and private-XDG digest remains unchanged.
Ref aliases (`ref`, `HEAD`, and `latest`) are forbidden. Any exercised changed
source, alternate-path, duplicate-heading, malformed-target, not-found,
ambiguous, or shallow negative control must fail with structured stderr and no
stdout; it cannot replace the happy path.

### M3: after-index sweep in `m3`

In a separate `m3` scope, run `kio init --json`, install the exact M1
`manual_only` config, and use two exact, manifested document versions plus two
`kio index --offline --approve --json` invocations to create a real non-tip
candidate exactly as in M1; no clock boundary is required. Replace the
scope-local config with these exact bytes and final LF, recording the bytes and
digests of both configs:

```toml
[gc]
mode = "after_index"
max_runtime_seconds = 60

[gc.auto_retention]
keep_last_hours = 0
keep_hourly_days = 0
keep_daily_weeks = 0
keep_weekly_months = 0
```

Modify the document again and run `kio index --offline --approve --json`. Require
exit 0, successful non-partial durable publication, and a nested GC result with
`status = "completed"`, never `reason = "no_candidates"` and never
`status = "deferred"`; its mode must be `after_index` and
`max_runtime_seconds = 60`. Canonical receipts must bind every swept
candidate/tree and exist before their trees are removed; each swept tree must be
absent and the active marker must be absent after completion.

### M4 and M5: one-shot auto/idle boundary in `m4-m5`

In a separate `m4-m5` scope, run `kio init --json`, install the exact M1
`manual_only` config, index an exact manifested old document with
`kio index --offline --approve --json`, change only that document to an exact
manifested current version, and run the same index command again so their trees
differ. Then install exactly these scope-local config bytes with a final LF, and
record the bytes and digests of both configs:

```toml
[gc]
mode = "on_idle"
max_runtime_seconds = 60
idle_threshold_seconds = 10

[gc.auto_retention]
keep_last_hours = 0
keep_hourly_days = 0
keep_daily_weeks = 0
keep_weekly_months = 0

[snapshot.auto]
enabled = true
interval_seconds = 3600
on_change_threshold = 1
```

Invoke the one-shot public `kio snapshot auto --json` once with no working-file
change. This is the required no-op/baseline negative control, not an M4 pass; it
must exit 0 with outer `status = "baseline_recorded"`, outer
`reason = "first_observation"`, `publication_status = "completed"`,
`snapshot_status = "noop"`, and nested `gc.status = "skipped"`; it must publish
`.kio/snapshot-auto.json` version 2 as JCS canonical JSON plus LF.
Then change one tracked file and invoke the same command again. M4 requires exit
0, outer `status = "not_idle"` **and** outer
`reason = "working_set_changed"`, `publication_status = "completed"`,
`snapshot_status = "completed"`, `eligibility_reason = "change_threshold"`, a
new commit/tree with expected stats, and an exact HEAD transition. Outer
`status = "completed"` is not required and does not replace those predicates.

For M5, make no further fixture or config change, wait at least 10 actual seconds,
and invoke the same one-shot command. Save the actual observation values and
require `idle_observed_seconds >= idle_threshold_seconds` with
`idle_threshold_seconds = 10`; equality is not required. Require exit 0, outer
`status = "completed"`, outer `reason = "idle_gc_completed"`, nested
`gc.status = "completed"`, nested `gc.mode = "on_idle"`, and nested
`gc.reason != "no_candidates"`. The canonical receipt must precede tree removal;
the old tree and active marker must be absent after completion. No scheduler may
be installed, enabled, registered, retained, modified, or uninstalled; cron,
launchd, systemd,
Windows Task Scheduler, login items, services, and persistent PATH/profile
changes are forbidden. Time injection is forbidden.

### M2: final on-demand sweep in frozen `m1-m2`

M2 is last and runs only against the frozen M1 fixture. Run `kio gc --yes --json`
and require exit 0 and `status = "completed"`. Its canonical receipt must have
exactly the four fields `commit_hash`, `tree_hash`, `gc_policy`, and
`shallowed_at`; its filename must be the candidate commit's 64 lowercase hex
payload, and its bytes must be canonical JSON with one terminal LF. All values
must bind to the recorded M1 candidate/tree and actual wall-clock receipt time;
the receipt must exist before removal. The candidate tree
must be absent afterward, all non-tree bytes must remain unchanged, and the
marker/internal cleanup state must be complete. M2 must never target user data, a
repository, an existing scope, an evidence root, or another fixture.

If exercised before the authorized `--yes` invocation, omitted `--yes`, the
incompatible `--dry-run --yes` form, a changed plan, or unsupported index
rotation must reject without fixture mutation. Bounded deferred/resume coverage
is optional. None of these negative controls can replace the completed M2
transition.

Negative controls may be recorded, but are non-acceptance evidence and cannot
supply a missing required predicate.

## 4. Fail-closed conditions

Stop the current run, save only the create-only failure/block receipt available at
that point, and mark all remaining stages `not-run` if any of these occurs:

- candidate, tag, release asset, sidecar, platform, archive, binary, byte count,
  or digest is not exactly bound as in section 1;
- a fixture, `HOME`, XDG root, evidence root, or command target resolves outside
  the recorded disposable parent, or a symlink/alias obscures that check;
- a stage's before/after digest cannot be produced, differs contrary to its
  mutation class, or reports an unaccounted file/object/state change;
- a command exits unexpectedly, emits a contract violation, uses a non-public
  binary, needs network/paid credentials, or requests a new privilege;
- any historical evidence is offered in place of a stage receipt, or an existing
  evidence file would need alteration;
- an operator observes user data, another repository's data, or an unrelated
  process/scheduler state in the intended target path.

Linux and Windows distribution replay require a verified native host and the
matching public archive before they can be run. macOS execution cannot stand in
for either. If either native-host precondition is unverified or unavailable,
record that platform as `blocked`; do not fabricate a pass, and do not downgrade
the macOS result into cross-platform acceptance.

## 5. Explicit non-authorizations

This contract authorizes only the disposable, distribution-bound manual replay
above. It does **not** authorize Kio internal Full or distribution Full, D1,
dogfood, paid API use, GPU allocation, online OCR, fixture discovery outside the
new disposable fixture, physical/bare prune, non-tree CAS reclamation, CoW GC,
M9, roadmap work, arbitrary source-product changes beyond the approved
defect-only minimum described below, or a policy change.

It also does not authorize changing a GitHub Release, tag, workflow, signature,
notarization, deployment, publication, push, PR, or workflow rerun/dispatch.
PersonaCorpus is at most a read-only dependency observation and its outputs,
leases, Git state, production tasks, and blockers cannot be changed or used as
Kio acceptance evidence. A product defect discovered by this replay is not
accepted evidence: stop the current run and preserve its receipt. The approved
minimal fix and regression test may then be implemented under its own validation
record, but the failed evidence root is never reused; acceptance requires a fresh,
fully bound replay from the first stage.

Historical Phase 3, prior RC, CI, source-tree, Full, PersonaCorpus, or earlier
manual evidence is context only. It never supplies a missing binding, fixture
manifest, before/after digest, command/exit record, binary digest, or stage pass
for this RC replay.
