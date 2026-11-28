# Provider acceptance Git ledger

The protected `codex/provider-acceptance-ledger` branch is the cumulative
budget authority for the provider campaign. The current implementation lives
in `crates/kio-eval/src/acceptance_tools/provider_ledger.rs` and is invoked as
`kio-acceptance-tools provider-ledger`. Its contract tests are part of the Rust
workspace; the superseded Python implementation has been removed.

Initialize the first immutable campaign before any provider call. Its root
commit contains only the campaign bootstrap. Each later reservation commit
atomically reserves all six provider/OS lanes: Mistral and Gemini on Linux,
macOS, and Windows. Reservations are `reserved_unknown`, so prior failures and
unknown outcomes remain charged.

An approved successor may follow any active campaign. Its campaign record
uses a globally unique ID and identifies the predecessor campaign and final
tip, which must be the immediate Git parent. New reservations name the
active campaign and record the reserving workflow's SHA-256 as provenance.
Campaign hashes are historical metadata. Workflow edits do not require a
transition. Earlier reservations remain verifiable at their original commits,
including records written before per-reservation hashes were recorded.

The cumulative ceiling is USD 10 for each provider across all campaigns,
including all earlier reservations. Each six-lane reservation consumes USD 0.30 per provider at the
current USD 0.10 lane allocation. A new campaign or any larger spend requires
fresh explicit budget authority and must never reset or replace the existing
branch.

The ledger writer and paid jobs share the `v1-provider-acceptance`
Environment, restricted to the `main` branch. The ledger writer alone requests
`contents: write` on its built-in `GITHUB_TOKEN`. It verifies the separately
built helper's inventory and digest before running it. Provider keys are
referenced only by the paid execution steps. Repository write access outside
this workflow is governed by GitHub repository permissions, and the ledger's
immutable-history ruleset protects the branch from deletion and history rewrites.

Paid jobs use read-only repository credentials and receive a specific
reservation commit. They verify its bounded ancestry, campaign, candidate, and
lane allocation before a provider credential is exposed. Receipt artifacts are
for diagnostics. Candidate jobs also require the exact current `main` SHA and verify the native package for
their OS before using a provider credential. Before reservation, the common
guard checks the completed source run and successful publication of all three
primary packages. Native acceptance and A12 reproduction are checked at final
aggregation.

## Dispatch order

Dispatch on current `main`. For a new ledger, use `operation: initialize`
with the approved campaign ID and empty candidate/package inputs. For an
existing ledger, continue the active campaign or follow the approved transition
procedure below. Reconcile uncertain outcomes against the existing ref before
retrying.

Use `operation: run` with the current candidate SHA, the completed native run
ID with all three packages, and the active campaign. The USD 10 cumulative cap
per provider includes all `reserved_unknown` charges.

## F4 successor

D-20261126-01 approved the transition from `v1-f4-2026-11` to
`v1-f4-2026-11b`, recorded at `10797ac9`. The initial reservation
(`96f41efa1548f61c3be3b3f7a8fb1c00034ad298`, run 45788229324) and the successor
reservation (`8bf3eb05`, run 45859763559) each charge USD 0.30 per provider.
D-20261127-01 permits using the active campaign after workflow edits. Check
the protected ref for the current tip and cumulative reservations.

For a future approved transition, dispatch `operation=transition` on `main`
with the new `campaign_id` and current `predecessor_campaign_id`. The helper verifies
and records the current ledger tip as the parent. Leave the candidate and package
inputs empty. After a
successful authority job, read back the tip, parent, campaign record and
retained reservations. Reconcile any unknown outcome against the ref before
retrying.

All ledger-helper GitHub REST reads are bounded (at most 256 linear commits, seven tree
entries per commit, and 16 KiB JSON blobs). The helper neither downloads nor
extracts an Actions ZIP while validating the authority chain.
