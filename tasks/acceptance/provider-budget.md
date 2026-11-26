# Provider acceptance Git ledger

The protected `codex/provider-acceptance-ledger` branch is the cumulative
budget authority for the provider campaign. The current implementation lives
in `crates/kio-eval/src/acceptance_tools/provider_ledger.rs` and is invoked as
`kio-acceptance-tools provider-ledger`. Its contract tests are part of the Rust
workspace; the superseded Python implementation has been removed.

Initialize the first immutable campaign before any provider call. Its root
commit contains only the campaign bootstrap and binds the hash of
`.github/workflows/v1-provider-acceptance.yml`. Each later reservation commit
atomically reserves all six provider/OS lanes: Mistral and Gemini on Linux,
macOS, and Windows. Reservations are `reserved_unknown`, so prior failures and
unknown outcomes remain charged; they are not retried for free.

One successor may be appended through the same protected authority. Its commit
contains only the successor campaign record, with a new ID and the SHA-256 of
the exact reviewed workflow bytes on current `main`. The record binds the
predecessor campaign ID and its final tip, which must be the immediate Git
parent. The earlier bootstrap and reservations remain in Git history. New
reservations must name the active campaign. Historical reservations still
verify at their original commit using the campaign and workflow hash in force
there.

The cumulative ceiling is USD 10 for each provider across both campaigns,
including all earlier reservations. Each six-lane reservation consumes USD 0.30 per provider at the
current USD 0.10 lane allocation. A new campaign or any larger spend requires
fresh explicit budget authority and must never reset or replace the existing
branch.

The workflow has a narrow authority boundary. The control-plane job uses the
separate protected `v1-provider-ledger-authority` environment. Its built-in
`GITHUB_TOKEN` has only `actions: read` and `contents: read`. After verifying
the separately built helper's inventory and digest, it mints a short-lived
installation token for a dedicated private GitHub App, restricted to
`ttokunaga-ja/kio` and `contents: write` (plus implicit metadata read). Only
this job receives the App private key. The pinned token action retains its
default post-job revocation; the token is never exported as a job output.
The ledger helper receives the App token as `GITHUB_TOKEN`; its immediately
preceding main-SHA check uses the read-only built-in token as `GH_TOKEN`.

Paid jobs retain the separate `v1-provider-acceptance` environment and read-only
repository credentials. They receive a specific reservation commit, then verify
that commit's bounded complete ancestry, immutable campaign workflow hash,
candidate, and lane allocation before a provider credential is exposed.
Artifacts are diagnostic receipts only; they never authorize spend. Candidate
jobs also require the exact current `main` SHA and a successful candidate-bound
native package workflow before they use a provider credential.

## Setup and approval order

Implementation, workflow execution, and paid tests within the USD 10 cumulative
cap for each provider are already authorized. Confirm the concrete new platform
access grants at action time: App creation/installation and permissions,
environment credential access, and the App's ruleset bypass. Once those grants
are confirmed and the gates below pass, execute initialization and subsequent
dispatches under the existing authorization without requesting it again.
Do not start a paid run before campaign setup and protection verification are
complete.

1. Create a dedicated **private** GitHub App under `ttokunaga-ja`. Grant only
   repository **Contents: read and write** and **Metadata: read**; grant no
   organization, enterprise, account, Actions, administration, or workflow-write
   permissions. Disable webhooks if unused. Install it on **Only select
   repositories: kio**, repository ID `1220844216`, not all repositories.
   Record and read back its numeric App ID, client ID, slug, installation ID,
   installation owner, repository selection, and approved permissions. The
   numeric App ID is the ruleset actor ID; it is neither the client ID nor the
   installation ID. Do not substitute a presumed GitHub Actions App ID such
   as `15368`: the platform picker must offer the actual installed dedicated App.
2. Create protected environment `v1-provider-ledger-authority` with deployment
   branches/tags set to **Selected branches and tags**, allowing only the
   **branch** `main` and no tags. Apply the approved reviewer/approval policy.
   Store the generated App private key only as environment secret
   `KIO_LEDGER_APP_PRIVATE_KEY`. Store environment variables
   `KIO_LEDGER_APP_CLIENT_ID`, `KIO_LEDGER_APP_ID`, `KIO_LEDGER_APP_SLUG`, and
   `KIO_LEDGER_INSTALLATION_ID` using the read-back identities. Do not place the
   private key at repository/organization scope or in `v1-provider-acceptance`.
   Read back environment scope, protection, and variable values; never print,
   persist in evidence, or expose the private key or installation token.
3. Install **two separate active branch rulesets**, each targeting exactly
   `refs/heads/codex/provider-acceptance-ledger`, with no exclusions:

   | Ruleset | Rules | Bypass actors |
   | --- | --- | --- |
   | Immutable ledger history | `deletion`, `non_fast_forward`, `required_linear_history` | Empty, including no administrator or App bypass |
   | Ledger writer | `creation`, `update` | Exactly one `Integration` actor: the dedicated numeric App ID, `bypass_mode: always` |

   Read back both complete ruleset bodies and effective applicable rules,
   including enforcement, exact ref conditions, rule types and bypass actors.
   Check for inherited or other overlapping rulesets and legacy branch
   protection. A writer bypass must never bypass immutable history rules.
   Stop if the platform cannot express or enforce this configuration; do not
   broaden a bypass, exempt administrators, or weaken history protection.
4. After the approved workflow is on current `main`, inspect its exact bytes/hash
   and configuration, then execute an `operation: initialize` dispatch with the
   one approved campaign ID and empty candidate/package inputs. No paid jobs
   execute for this operation. Prove the App can create the root through this
   authority job under the installed protections; read back the root, bootstrap,
   workflow hash, run result, and protections again. Reconcile any partial or
   failed attempt before retrying; never delete or replace an existing ledger.
5. For an approved successor, follow the transition procedure below. Only
   after the applicable read-back passes, execute `operation: run` for the exact current
   candidate SHA, successful native package run ID, and same campaign. Preserve
   the USD 10 per-provider cumulative caps and `reserved_unknown` charges.
   A workflow hash change requires an approved transition before further
   reservations.

## F4 successor

D-20261126-01 approves one six-lane rerun under `v1-f4-2026-11b`.
Run 45788229324 already reserved USD 0.30 per provider under
`v1-f4-2026-11` and remains `reserved_unknown`. Its reservation commit was
`96f41efa1548f61c3be3b3f7a8fb1c00034ad298`. Check the live ref to establish
the actual final predecessor tip and total prior reservations.

After merging, review the workflow bytes on current `main`, read back the
environment and rulesets as in setup, and dispatch the provider workflow on
`main` with `operation=transition`, `campaign_id=v1-f4-2026-11b`,
`predecessor_campaign_id=v1-f4-2026-11`, and
`predecessor_tip=<verified current ledger tip>`. Leave `candidate_sha` and
`package_run_id` empty. No paid jobs execute. Require a successful authority
job, then read back the protected tip, its parent, the successor record and
workflow hash, the retained reservation, and the protections again. Reconcile
any partial or unknown outcome by reading back the ref before any retry.

After the successful exact-candidate native package run and successor
read-back, dispatch once on `main` with `operation=run`,
`campaign_id=v1-f4-2026-11b`, the exact `candidate_sha`, and the successful
`package_run_id`. Leave both predecessor inputs empty. This reserves another
USD 0.30 per provider. Preserve all charges and reconcile the result before
considering any further paid action.

The workflow validates the configured nonsecret identity formats before minting,
compares the pinned action's App slug and installation ID outputs to the approved
values, and checks one bounded `GET /installation/repositories?per_page=2`
response for `total_count == 1` and the exact repository ID and full name.
That endpoint proves the token's repository scope, not its issuer, complete App
installation selection, enterprise access, or complete permission set. The
pinned action's mint operation plus approved setup/readback is the identity
trust boundary; the action outputs are not an independent issuer attestation.
The configured numeric App ID is a setup/ruleset identity, not a runtime App ID
returned by this action. Any saved setup or principal evidence is nonsecret
diagnostic provenance and does not authorize ledger writes or paid calls.

GitHub App restrictions do not identify an individual workflow or job. Keeping
this control-plane job the only writer also depends on trusted changes to main,
the private key's environment isolation, approval policy, and repository/App
configuration. A repository or App administrator can change those protections
or credentials; the ledger cannot defend against that authority. The immutable
workflow hash and platform-enforced exact ruleset actor jointly constrain the
writer under that configuration. Do not describe this as workflow-level identity
enforcement or cryptographic principal attestation by the ledger format.

All ledger-helper GitHub REST reads are bounded (at most 35 linear commits, seven tree
entries per commit, and 16 KiB JSON blobs). The helper neither downloads nor
extracts an Actions ZIP while validating the authority chain.
