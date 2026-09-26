# Provider acceptance Git ledger

The protected `codex/provider-acceptance-ledger` branch is the cumulative
budget authority for the provider campaign. The current implementation lives
in `crates/kio-eval/src/acceptance_tools/provider_ledger.rs` and is invoked as
`kio-acceptance-tools provider-ledger`. Its contract tests are part of the Rust
workspace; the superseded Python implementation has been removed.

Initialize exactly one immutable campaign before any provider call. Its root
commit contains only the campaign bootstrap and binds the hash of
`.github/workflows/v1-provider-acceptance.yml`. Each later reservation commit
atomically reserves all six provider/OS lanes: Mistral and Gemini on Linux,
macOS, and Windows. Reservations are `reserved_unknown`, so prior failures and
unknown outcomes remain charged; they are not retried for free.

The cumulative ceiling is USD 10 for each provider, including all earlier
failures. Each six-lane reservation consumes USD 0.30 per provider at the
current USD 0.10 lane allocation. A new campaign or any larger spend requires
fresh explicit budget authority and must never reset or replace the existing
branch.

The workflow has a narrow authority boundary: its protected control-plane job
is the sole `contents: write` holder. Paid jobs receive a specific reservation
commit, then verify that commit's bounded complete ancestry, immutable campaign
workflow hash, candidate, and lane allocation before a credential is exposed.
Artifacts are diagnostic receipts only; they never authorize spend. Candidate
jobs also require the exact current `main` SHA and a successful candidate-bound
native package workflow before they use a provider credential.

This design depends on branch protection that prevents deletion and force-push,
and on only the protected control-plane workflow being able to update the
ledger. A repository administrator able to bypass those protections can replace
the history; the ledger cannot defend against that authority. Branch protection
must therefore be installed and verified before this workflow is used.

All GitHub REST reads are bounded (at most 34 linear commits, seven tree
entries per commit, and 16 KiB JSON blobs). The helper neither downloads nor
extracts an Actions ZIP while validating the authority chain.

No provider campaign bootstrap, reservation, paid call, or acceptance receipt
currently exists. A workflow file or local helper build is not provider
acceptance.
