# v1 implementation contracts

This document fixes implementation choices from the approved
[v1 plan](../tasks/v1-implementation-plan.md). It is a contract, not a statement
that every implementation or native acceptance test is complete. Actual evidence
is tracked in [the implementation record](../tasks/v1-implementation-progress.md).

## Authority and stored information

| Location | Role | Recovery rule |
|---|---|---|
| Each scope's `.kio/objects`, `HEAD`, immutable tags | Knowledge content and linear history | Recover from the scope's backup; do not infer a replacement HEAD from loose objects |
| `.kio/scope.json`, `management.json`, scope settings and consent | Local scope identity, membership, policy and explicit permission | Preserve current authority during content restore; copied knowledge does not establish a new grant |
| `.kio/index/sqlite.db`, central search replica | Derived search state | Rebuild from current scope truth and policy |
| Central registry and watcher queue | Discovery and scheduling state | Revalidate scope truth; startup reconciliation recovers lost hints |
| Central billing intents, reservations and results | Non-rebuildable operational truth | Authority-bound, create-only backup and explicit reconciliation; absence must not authorize a fresh paid submission |
| Device configuration and local Adapter trust | Device-specific execution authority | Explicit device enrollment; never import trust from a scope |

### Device billing lifecycle

Billing is operational information, not knowledge reconstructed from a folder.
`ledger init` explicitly creates the central device ledger. Scope initialization,
offline indexing and text search do not initialize it. Before initialization,
budget observations are unknown; absence is never evidence of zero spending or
permission to submit paid work. `ledger status` reads a private snapshot without
opening the source SQLite database for writing.

An initialized ledger consists of `cost-ledger.sqlite`, its normal SQLite WAL
coordination files when present, `cost-ledger.sqlite.authority.json`,
`cost-ledger.sqlite.checkpoint.json`, its private backup proof, and the stable
`ledger.lifecycle.lock`.
Authority binds a ledger ID and era to the database metadata. The checkpoint
binds that identity to a strictly increasing signed-64-bit sequence. Neither
ordinary open nor provider reconciliation may invent a missing checkpoint,
repair a schema, lower a sequence, or adopt another era.

Each domain write holds the device filesystem lock, validates the actual SQLite
connection it uses, performs its SQL transaction, and durably publishes the next
checkpoint before COMMIT. Failure between checkpoint publication and COMMIT
leaves a mismatch that blocks subsequent paid work. A stale backup therefore
cannot silently become a new accounting baseline. Retained directory/file
handles, owner-private storage and no-follow checks reject unsafe substitutions;
this is not protection from an adversary already controlling the same OS account.

Initialization publishes an explicit pending record before the database. An
interrupted initialization can only be resumed by `ledger init --resume`, which
must match that exact recorded identity and still-empty billing history. It is
not a reset command. Complete or partial existing artifacts are never replaced
by `ledger init`.

`ledger backup` captures a WAL-safe SQLite image while holding the lifecycle
lock, binds it to the authority and checkpoint, records a private exact-proof
for that generation, and publishes the requested owner-private backup directory
create-only. `ledger restore` loads that artifact but can recreate only a
missing database. Before it publishes either a restore journal or a database,
the surviving private authority, checkpoint, and backup proof must all bind the
same exact image. An existing database, a stale generation, a changed backup,
or missing proof is a refusal, not a recovery shortcut. See the ledger command
contract in [the CLI specification](06-cli-spec.md).

Recovery from lost billing history is separate from querying current provider
jobs. A successful provider inventory alone cannot establish the cost of absent
or unqueryable requests. Such uncertainty must remain visible and prevent new
paid admissions until explicit evidence establishes the accounting state.
Ledger backup and restore do not recreate scope content, permissions, device
grants, or Adapter trust.

A local management grant covers future nonignored descendants, including empty
directories. It does not grant external transmission. Every managed child has a
direct parent and reciprocal enrollment token recorded in both scopes. Validation
walks these records to an explicit root. Scope IDs, the original canonical path,
retained filesystem identities and enrollment tokens must agree. A registry row
cannot substitute for a missing or inconsistent parent.

In the current unreleased management and internal recovery formats, opaque OS
identity components are JSON strings: `device`, `inode`, and `file_index` use
exactly 16 lowercase hexadecimal digits; `volume_serial_number` uses exactly 8.
This preserves every bit through JCS canonicalization, including values above
2^53. Rust comparisons continue to use the original integer components. Numeric,
uppercase, signed, or non-fixed-width encodings are rejected; earlier numeric
records are not migrated or accepted as authority. The current unreleased persona
materialization record and filesystem attestation also encode `filesystem_device`
with the same 16-digit string format, preserving their numeric runtime checks.

An independently initialized nested root is not silently adopted. A moved,
copied, revoked or inconsistent child is ineligible until an explicit management
operation establishes a valid binding. A partial parent/child enrollment is
ineligible; publication of two records is not claimed to be a filesystem-wide
atomic transaction. Cross-volume traversal requires an independent explicit root.

Case sensitivity is established when the management grant is created and bound
to that filesystem identity. Search and preview do not create probe files.

### Explicit root registration

`kio root register [path] --preview` reads the selected managed root and its
whole enrolled subtree without creating a registry or any other state. It shows
the planned detachment, scope count, path/identity rebind, and grant revocation.
`--yes` is the only mutation form. `--resume OPERATION_ID` additionally requires
`--yes` and can continue only the exact pending journal for that root.

A moved root rebinds every enrolled descendant as one journaled operation. If
the selected root was a Child, it becomes an independent Root; descendants keep
their direct parent relations and enrollment tokens while their root authority is
updated. Cross-volume movement is permitted only through this explicit operation.
Device-cached old paths and registry rows on the current OS are checked natively
for a live scope with the same scope ID. A foreign-OS path is parsed only as a
lexical historical value and is never opened or used to establish absence.
Actual cross-OS transfer remains unvalidated. Normal child discovery never
starts this migration.

Registration preserves knowledge objects, HEAD, and scope approval records. It
does not preserve device execution authority: it revokes the selected subtree's
device-global central grants before publishing the new records. Each v2 management
record advances `registration_generation` exactly once, then the registry
atomically replaces the prevalidated old rows with unindexed new rows.

Before publication, each node receives a matching pending marker. The root
journal records the exact before/after records, identities, former paths, and
phase. Resume accepts only mutually matching journal, marker, record, retained
identity, and old-path observations; any mismatch remains fail-closed. There is
no compatibility reader: every pre-v2 management record, missing mandatory
generation, or unknown field is rejected.

This contract covers one authority subtree only. v1 multi-authority transfer,
merge, or inheritance is outside its scope.

### Child initialization and explicit cancellation

This is an implementation contract for the current revision; complete candidate
validation remains pending. A parent keeps at most one private initialization
operation for each child basename, under `child-initializations/<ULID>.json`.
The bounded set has at most 1024 journals, 8 MiB in aggregate, and 64 KiB per
journal. A pending child denied by current Ignore stays inert and does not block
unrelated children. It resumes only after policy admission for the exact planned
child identity and enrollment plan returns. A missing or changed pinned stage is
fail-closed and is never silently replaced.

Targeted retirement reads the parent index once and acts per child. Explicit
`repair cancel-child-initialization` may cancel only the exact unpublished,
parent-private stage selected by its operation ID; it never deletes a child root
or a user file. Before stage retirement, it cleans only that stage's verified
exact case probe. Its durable marker advances through `Prepared`, `Quarantined`,
and `Removed`; automatic replay resumes only that prior explicit cancellation.
A published pending `.kio` remains gated and resumes normally after Ignore is
removed. Root registration refuses a root with any pending initialization or
cancellation marker. Read-only planning creates neither a lock nor an archive.

### Atomic crash recovery and explicit-root bootstrap

This current-revision contract also remains under validation. An owner-private
`.kio-atomic` workspace with a permanent gate belongs only to `.kio` or a pinned
private stage owner, never to a user folder. It bounds one-file publication or
removal. An unpublished write may only be discarded; hardlinks are refused and
publication uses no-replace rename. Before quarantine, removal persists an exact
intent that binds typed physical owner, target, parent-chain, and source-file
identities using the lossless hexadecimal string encoding above.
Recovery acts only when those identities and exact bytes still match; otherwise
it fails closed. On Windows, foreign-owner working-file removal is refused before
any ACL or name mutation; a current-owner private DACL is validated through its
retained handle. A working-tree restore names the `.kio` owner permitted to
recover its residue. Read-only inspection never creates, locks, or cleans this
workspace.

An explicit CLI root bootstrap is serialized by `.root-init-journal.json` and a
permanent `.root-init-gate`. Normal management remains blocked until the exact
record is verified. Only an empty pre-marker new root may retry. Admission
authenticates only the immediate parent and holds that parent's lease; it refuses
to recreate an independent root for the same child when a lost `.kio` remains
parent-enrolled, while a valid existing Child is idempotent. Existing unknown,
old-format, or unmanaged non-empty stores are refused unchanged. The retained
scope root may contain only the fixed exact transient `.kio-case-probe`; it may
be resumed, while differing probe residue fails closed. The core unmanaged
`Repository::init` / `create_bound` APIs lack this enclosing bootstrap journal,
but have no shipped CLI, app, or eval production caller and the core crate is
not published; this is a low-level storage/testing concern, not an unimplemented
CLI route.

### Controlled managed roots

This current-revision contract remains under validation. Binding, capture, and
recheck require a managed root and its ancestors to be controlled by the current
user or a trusted OS administrator under the creation-parent policy. Every
command, including read-only status, search, and preview, refuses an unsafe
existing managed root with `KIO-E-MANAGEMENT-ROOT-UNSAFE-001` / exit 4 before
mutation. Read sharing such as Unix `0755` is permitted; write, delete, or ACL
mutation by another OS principal is not. Kio never automatically chmods,
re-owners, or repairs an existing root. `.kio` remains exact current-owner
private, which is stricter than this shared-read root boundary.

On Windows, `NtCreateFile` creates a new private object with the current owner
and a protected DACL; existing objects are never adopted. A working-file removal
keeps its verified handle through move and DACL handling, while read sharing
blocks a new writer or deleter. Unix does not claim inode-CAS protection against
a hostile same-account filesystem writer. v1 multi-user ACL mode is outside this
contract; it does not promise a future local-OS-folder model.

## Current policy

The current policy evaluator reads each ancestor's local configuration and
`.kioignore` through retained handles. Within one scope, its rule order applies.
A denial by any ancestor cannot be lifted by a child's negation. A denied
directory makes its descendants ineligible, including rows and tasks created
before that denial.

`index` (including preview) and both `reindex` modes reject a denied scope
before writer recovery or index mutation. Rejection leaves the scope's existing
HEAD, manifest and stored history intact; scope denial is not an instruction to
publish an empty tree. Ordinary file-level Ignore still filters eligible files.
Index and regenerating reindex revalidate the captured policy immediately before
publishing the journal/HEAD/manifest, including the no-op success path. A policy
change observed there aborts publication; immutable objects prepared earlier may
remain unreachable. This does not claim an atomic lock against an external
editor changing ancestor policy after that validation point.

The policy digest binds membership, local policy bytes, the fixed secret/control
path rules and filesystem case behavior. Search filters before limiting
candidates. Cursor replay rejects changed policy. OCR, document/query embedding
and reranking revalidate applicable authority immediately before admission to a
send. A watcher event is not needed for that check.

Revocation orders subsequent send admissions. Bytes already admitted to a provider
cannot be recalled. An external editor does not participate in Kio's store lock;
the live validation point, not the eventual watcher event, defines what the
operation observed.

## History and restore

### Source images in normalized knowledge

A standalone image remains an immutable image CAS object after OCR. For every
new image unit, the normalization boundary verifies the already-read raw bytes,
their hash and declared media type, then retains a canonical image reference in
the Markdown alongside the extracted text. The reference uses the current
scope identity and verified content hash; Adapter response metadata cannot
choose its authority. Image embedding and rebuilt search replicas derive their
image references from these persisted units, so text-only OCR output does not
remove the source image from search.

An image link is not ownership authority. Each immutable normalized unit pins
the required `owned_image_hashes` set, populated from verified source pixels or
images actually returned and persisted by its Adapter. A Markdown URI or a
free-form metadata field cannot add an owner. Local CAS access requires the URI
to name the actual scope and a retained authenticated owner allowed by current
policy. Any retained secret owner keeps the separate secret-send requirement;
a public link cannot remove it. Foreign-scope links retain their text and resolve
only through their declared scope's authority.

Mistral synchronous and batch results use the same bounded OCR parser and
image conversion. Batch collection keeps a successful provider job and its
reservation when local result storage fails; collecting that known result
again does not submit another paid job. Provider response rejection and local
storage failure are separate outcomes. The latter emits
`KIO-E-BATCH-RESULT-STORE-001` for an image CAS failure, so operators do not
mistake local storage repair for a need to retry the provider request.

A synchronous OCR request has no result-query API. A process restart with an
in-flight synchronous row, or any post-send transport, response-contract, or
local-publication failure, therefore settles the reservation conservatively
and records terminal `result_unknown`; normal retry and index/resume never
send it again. A typed provider auth, rate-limit, or quota rejection is a
known zero-cost rejection and may follow its normal fresh-reservation recovery.
`fallback_to_full` received after a paid incremental request is likewise not
archivable output and becomes `result_unknown`: Kio does not issue an implicit
second Full request. Only a pre-send incremental inapplicability, including a
zero-change no-HTTP path, may safely choose Full. An operator who deliberately
accepts the possible earlier charge uses `kio batch retry --resend-unknown
<selector> --yes`; that ledger-only command authorizes one exact current task
or embedding content group, then a later explicit online pass performs all
current grant, policy, secret, and budget checks under a new reservation.
For OCR that explicit recovery selects a fresh Full request rather than
repeating the prior incremental control response.

Image payloads are limited to 16 MiB and decoded under an 8,192-pixel limit per
dimension, 16 million pixels and a 64 MiB allocation budget. GIF source images
retained after OCR additionally require at most 32 frames and at most 16 million
pixels / 64 MiB of RGBA output across the animation. This does not add native
standalone GIF ingestion. Image embedding validates each CAS payload and admits
one image per HTTP call; it does not collect the corpus's image bytes in memory.
Every call revalidates current policy, its exact Adapter network grant, and any
required separate secret-send grant. These rules apply to authenticated local
HTTP as well as external provider admission; a zero price does not grant
permission to transmit content.

Ownership sets contain at most 256 canonical hashes per unit. Missing fields,
duplicates, malformed hashes and oversized sets are schema errors. Rebuild,
history verification and orphan collection consume the same typed ownership;
they cannot turn a foreign or user-written Markdown link into a local CAS edge.

An incremental unit marked unchanged retains its previous Markdown verbatim.
This rule does not rewrite pinned normalized instances or historical commits.
Pre-release diagnostic stores created before this contract require a fresh
store or an explicit reindex generation; ordinary reads and rebuilds never
silently migrate their content. Content restore preserves the current policy
and device grants, including for image queries and image-object access.

### Linear content history

Format `1.0.0` commits require `parent`, containing `null` for genesis or exactly
one commit hash. HEAD is the only mutable history reference. Its wire value is
`unborn\n` before the first snapshot, or a commit hash followed by a newline.
Missing, empty and malformed HEAD are errors. Legacy formats and `parents`
arrays are rejected before mutation; no fallback branch reference is consulted.

Publication writes immutable objects, a durable journal, conditional HEAD,
the mutable manifest, then removes the journal. Recovery checks the expected
parent and stored commit before replay. Read-only operations do not repair a
pending publication. SQLite and replica updates are separately recoverable.

`export` writes historical content into a separate destination. `restore` adopts
selected historical user raw paths as a new child of the observed current HEAD.
The historical source is provenance, never a second parent. Full-scope restore
does not touch unmanaged files; selected restore preserves unselected paths.
Deletion of a path absent from the source must be explicitly selected. Dirty
targets, missing/purged source objects and HEAD conflicts stop before application.

Neither operation may treat `.kio`, `.kioignore`, consent, configuration, journals
or billing state as historical user content to reinstall. Restoring knowledge
must not revive a revoked permission.

### Purge attribution and completeness

Purge fixes its deletion and preservation authority in a version-2 closure before
removing SQLite rows. Resume uses that recorded closure, without reclassifying
objects from a partially deleted index. Version-1 closures are rejected.

Chunk ownership comes from authenticated ledger rows, retained immutable history
and matching canonical chunk objects. SQLite metadata must agree with the ledger;
it cannot create ownership or preservation authority. Every chunk-target embedding
must have authenticated canonical chunk attribution, including embeddings whose
text initially appears unrelated to the requested raw objects. If ledger, index
and chunk objects are missing, a remaining vector's text hash cannot establish its
raw owner. Purge then stops before creating its closure with
`KIO-E-PURGE-AUTHORITY-INCOMPLETE-001`. It neither deletes uncertain objects nor
reports complete erasure. `repair rebuild-db` can reconstruct SQLite from intact
records; it cannot recreate missing ownership evidence.

The canonical chunk completeness traversal uses retained directory handles and
streams one object body at a time. It admits at most 100,000 directory entries
(including fanout directories), 128 MiB per chunk body, and 512 MiB of successfully
read bodies in total. A concurrent growth check may read one extra byte before
refusal. Unknown entries, malformed fanout and unattributed quarantine entries
stop preparation. These are traversal limits, not a bound on the entire purge
operation: the existing chunk-ledger parser still materializes its JSONL input.

## Watching defaults

| Setting | v1 initial value |
|---|---:|
| Event debounce | 250 ms |
| Maximum wait under repeated events | 2 s |
| Full reconciliation interval | 300 s |
| Pending path hints per root before full-root fallback | 4,096 |

The foreground CLI, its service lifecycle, and their exact argument contract are
defined in [the CLI specification](06-cli-spec.md). `watch status` is read-only;
`watch stop` requests a graceful stop after the active reconciliation. Instances
are serialized by an OS file lock, and stop requests name the current instance,
not a PID. A saved status includes its observation time and is not liveness proof.
Each pass reloads current device settings and validates the retained registered
root before invoking the same local index operation used manually. No network or
secret grants are created. The watch path is offline: it performs no provider-job
polling, download, or cleanup, and it does not use an HTTP Adapter. User-service
registration and lifecycle behavior are implemented as specified there; native
scheduler lifecycle acceptance remains a separate gate on each OS.

All generated `.kio` paths, including HEAD, manifest, lock and task files, are
suppressed as self-events. Management, policy, settings and consent paths still
trigger full reconciliation. Runtime queue/state files live outside the watched
root in owner-private device storage.

Native macOS, Linux and Windows events produce hints for the same reconciliation
operation used by manual indexing. Overflow, backend errors and queue limits
schedule a complete root reconciliation. Startup always reconciles. A successful
claim cannot discard a newer event received while that claim was running.
Incomplete work remains visible and queued. Wall-clock changes must not silently
suppress periodic recovery.

Generated object/index/log churn is excluded from ordinary content hints;
policy and consent control-file changes are retained. Native event delivery alone
does not establish content identity: reconciliation hashes the relevant bytes.

## Local Adapter and renderer boundaries

Adapter approval is paired authority: `adapter approve`, `revoke`, and `status`
coordinate the current scope's approval references with device-local grant
records. A send requires both active records and an exact current binding; a
scope row or device record alone cannot authorize it. Bindings cover the managed
scope/membership, execution identity and profile, destination, credential
binding, trust binding, and operation. Secret transmission, when selected, has
its own paired grant. Approval publishes resumable pending records across the
two stores; revocation removes the scope-side active or pending reference and
revokes matching device grants, so a later approval must establish fresh paired
authority. `status` reports the records and their effective current match. See
[the CLI specification](06-cli-spec.md) for command syntax and confirmation
rules.

Local Adapter endpoints require HTTPS loopback and an explicit port. Their CA
is a device-local managed snapshot registered through the Adapter trust lifecycle;
the active record carries a generation, digest, and snapshot identity, never a
user-configured source path or certificate bytes. The retired
`adapter.policy.offline_api.ca_pem_path` setting is rejected. Trust registration,
rotation, revocation, and status are device-local operations that do not read a
scope. Registration/resume creates a managed snapshot, rotation advances the
generation, and revocation leaves a durable tombstone; an active CA cannot be
silently replaced by registration. Runtime admission acquires the active managed
CA and retains the trust-store lock through Adapter dispatch, so rotation or
revocation cannot race an already admitted send. Only the managed active root is
trusted; proxy and redirect use are disabled. Certificate validity and name
verification finish before a body is sent. Authentication errors are permanent;
connection refusal remains a transport error.

`--offline` blocks every HTTP dispatch for that invocation, including an
authenticated local Adapter. It does not turn a local endpoint into a
non-network execution route. Offline behavior and the command-specific flag
forms are defined in [the Adapter specification](07-adapter-spec.md) and [the
CLI specification](06-cli-spec.md).

Renderer execution uses an explicit environment, private scratch space,
deadline and process-tree cleanup. OS confinement denies unnecessary network
access and unrelated private files. Missing confinement is a refusal, not an
unconfined fallback. The initial limits are 300 seconds wall/CPU time, 2 GiB
address/job memory where supported, 100 MiB Office input and 250 MiB PDF output.
Monitoring of scratch growth is distinguished from a kernel-enforced disk quota.
On Windows, the primary process exiting or closing its output pipes is not enough
to complete a conversion. The supervisor keeps observing the private Job until
its active process count reaches zero, then performs a final scratch scan before
accepting output. All observation, pipe draining and final scanning use the
original command deadline. Failures request termination of the entire Job and
observe cleanup only within the remaining time; timeout does not promise that
process termination has already completed. Closing the Job retains the
kill-on-close fallback. Native Windows tests and real Office conversion are
required to establish the behavior on the supported platform.
On macOS, the renderer may not fork. Its physical footprint is observed every
50 ms against a 2 GiB threshold; threshold or observation failure goes through
the same termination and reaping path as a timeout. This sampling is distinct
from an instantaneous kernel memory limit.

Office preparation has an identity separate from its resulting bytes. Each
prepared unit and its normalized manifest/object persist a required
`preparation_profile_hash`. That identity binds the base prepare profile and,
for Office, the selected renderer fingerprint, platform, PDF normalization rules
and any private UNO catalog/filter. A change invalidates incremental reuse even
when the extracted text is unchanged. `prepared_hash` remains the hash of the
prepared content; the display version remains provenance. Old normalized records
without the required preparation identity are rejected rather than assigned an
invented current profile.

The macOS Office invocation creates its own bounded service catalog without the
native MacSpellChecker component. That component initializes AppKit during
headless Writer startup and caused a confined conversion to abort. The filter
must match exactly one expected standalone component and preserve the remaining
catalog bytes. It neither edits the installed application nor loads the user's
LibreOffice profile. Its source identity is rechecked before each invocation.

The native Windows/Linux/macOS lanes must execute the real renderer and local
TLS tests. Missing prerequisites or skipped tests are not acceptance evidence.
Paid provider receipts and native service lifecycle receipts remain separate
required gates. PersonaScope/personaCorpus performance evaluation is optional for
v1 release readiness.
