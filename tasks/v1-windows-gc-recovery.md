# Windows GC completion contract

Status: implementation is entering integration validation. Windows now enters
the implemented protocol so native tests can exercise it; native acceptance
and final security review remain release gates. This closes an M1 gap
found during final v1 acceptance preparation, not an optional post-v1 feature.

## Required boundaries

- SQLite verifies the actual main-file HANDLE through the public
  `SQLITE_FCNTL_WIN32_GET_HANDLE` operation before pager access. A locator derived
  from a retained handle is only a lookup hint. Existing sidecars, mismatching
  identities, reparse points, hardlinks and unexpected opens fail closed.
- Retain scope, `.kio`, both file parents and operation-private directories.
  All mutations use these capabilities, single direct leaves, same-volume
  checks, and exact file handles; diagnostic paths never grant authority.
- The ordinary store writer barrier serializes cooperating processes. File
  handles used for mutation also deny competing write/delete opens. Recovery
  rechecks identities and bytes; persisted journal fields alone do not authorize
  shallow receipts or tree deletion.
- No new Windows syscall is described as an atomic two-name exchange. The
  implementation uses journaled no-replace moves with explicit intermediate
  states. Unix atomic exchange remains unchanged.

## Namespace exchange

Use a GC-specific bounded helper under `kio-core::gc`, shared with `kio-index`.
Do not extend the general `.kio-atomic` on-disk schema. Separate reserved
directories `gc/internal/marker-exchange` and `gc/internal/index-exchange` keep
the journal and backup outside existing marker/index leaf inventories.
The scheduler checkpoint uses the same state machine with its own
`gc/internal/snapshot-state-exchange` owner and strict checkpoint leaf grammar.

A canonical, versioned intent records operation kind, both retained parent
identities, direct source/target names, both single-link file identities, lengths
and content hashes. Publish the complete intent before moving either file.
Reuse the existing atomic publication mechanism for this small record and
retain its staged-publication recovery rules. Unexpected artifacts or identity
substitutions stop recovery without deleting them.

| State | Public leaf | Prepared leaf | Reserved backup |
| --- | --- | --- | --- |
| Initial | source | target | absent |
| Source captured | absent | target | source |
| Target published | target | absent | source |
| Exchange complete | target | source | absent |
| Source retired | target | absent | absent |

Only these placements are admissible. Every present file must match its
recorded identity and content; a third file, duplicate name, changed parent or
hardlink fails closed. Move the retained source to backup, retained target to
public, then retained backup to the prepared name. Resume by finishing the
remaining steps. Keep the intent while retiring the old source through its exact
handle. Verify zero links, truncate and flush, close the deleting owner handle,
then verify the prepared name is absent before removing the intent. A surviving
name after disposition is not completion. The journal retains the original
identity throughout this sequence; a canonical-looking stale marker name alone
does not authorize cleanup.

At the terminal source-retired state, recovery validates the exact current
target, all retained parent identities and caller-supplied operation binding.
The caller revalidates current marker truth or SQLite rotation authority before
the helper clears the intent. There is no missing old file to reopen, fabricate,
move or delete. This state only permits completion of journal cleanup.

## Marker and index integration

- A pending marker exchange, including unpublished journal residue, is an
  active/corrupt GC state even if `gc/in_progress` is absent. All ordinary
  read/write/search/scheduler gates inspect this state. Read-only commands never
  recover it. `gc --yes` detects it and enters recovery under the writer lock
  before reading the ordinary marker.
- Validate old/new marker canonical schemas and the same allowed marker
  transition used for uninterrupted execution before recovery moves files.
  Preserve the semantic retention-policy revalidation before irreversible work.
  If the source is already retired, validate the actual target and current
  policy/receipt/phase invariants before clearing the journal; do not infer an
  old marker body from its digest.
- The index rotation already has durable source/target authority in the GC
  marker. Bind the exchange journal to that exact rotation and its private
  parent. Recovery may see an absent public index only while that matching
  journal accounts for the exact source and target. Do not open a missing index
  with CREATE or synthesize a replacement.
- The generic helper's completion is a namespace result, not SQLite or GC
  attestation. Continue to validate generation, rotation role, plan digest and
  source/target identity before authorizing any tree deletion.

## Exact-handle retirement

Windows tree retirement reuses the deterministic quarantine and frozen marker
authority. Open the validated tree with read/write/delete rights and read-only
sharing; move that same handle to the reserved quarantine with no replacement.
After the final marker/index/receipt checks, delete by handle, require the same
file identity and zero links, then truncate and flush through that handle.
An interruption leaves either the verified quarantine or no linked victim.
Do not translate the Unix sentinel-exchange implementation into pathname unlink.
Use the same exact-handle retirement for old markers, prepared indexes and
snapshot-auto temporary records where appropriate.

## Writer coordination and scheduled checkpoints

Storage format `2.0.0` requires an immutable empty `.kio/.store-gate`, created
on every OS by the planned initialization path. This is independent of the
product's v1.0 release goal. Older formats are rejected before gate inspection
or mutation; there is no migration, compatibility reader or lazy gate repair.
Windows inventory readers hold a shared nonblocking byte-range lock using a
read-only handle. Both ordinary and retained store writers acquire the same
gate exclusively before touching `.lock` and keep it until the owned lock
handle closes. Nested guards share the owner until the final guard drops,
regardless of drop order. Unix retains its existing directory flock protocol.
Publication-private and unrelated explicit-path locks use distinct leases.

Scheduled snapshot staging verifies Windows handle identities, input hashes,
the direct entry set, current ignore policy and purge boundaries before CAS
publication. Checkpoint replacement uses the five-state protocol above; a
pending journal never allows an absent public checkpoint to mean a first run.
Ordinary writers and GC reject a pending checkpoint exchange. The enabled
`snapshot auto` entrypoint alone acquires its recovery lock before index/GC
preflight or eligibility observation. A disabled configuration with pending
recovery returns an authority error without modifying the store.

The persistent checkpoint binding includes retained scope/store identities and
the exact HEAD, tool-lock, configuration and ignore content digests plus GC
policy. Configuration inode identity is not persisted as an irreversible
recovery condition: restoring the exact prior contents can restore the binding.
Within an invocation, exact file observations are checked before and after
recovery. Completing the old checkpoint does not authorize a new snapshot;
eligibility and current policy are evaluated again. Old/new canonical checkpoint
transitions preserve monotonic attempt and idle timestamps, and terminal
recovery validates the actual target before clearing intent.

## Validation and limits

Require native Windows tests for every move/intent/cleanup interruption,
positive nonempty CLI GC, automatic GC, repeated resume, source/target/parent
replacement, hardlink/reparse/collision rejection, ordinary-operation exclusion
during a missing public name, and retained-reader byte reclamation. Extend the
3 OS acceptance evidence so a purge-only A09 pass cannot stand in for GC.
A09 also exercises immutable read-only inventory and real scheduled snapshot
first-run no-op, changed-file linear publication and unchanged skipped repeat.
Separate native tests exercise all six checkpoint-exchange interruption seams.

The 2026-09-27 NTFS probe established retained no-replace rename, restrictive
sharing, exact-handle POSIX disposition, zero links and subsequent truncate/
flush. It did not establish power-loss ordering. Keep process-interruption
recovery evidence separate from filesystem/volume durability claims; do not
claim Unix-equivalent directory fsync based only on a successful file flush.
