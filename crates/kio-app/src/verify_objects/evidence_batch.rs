use crate::context::EvidenceRegistry;
use std::collections::BTreeMap;
use std::path::Path;

use kio_core::cas::{hash_bytes, read_bounded_regular_file};
use kio_core::{ExitCode, Result};
use serde_json::Value;

use crate::response::evidence::{
    EvidenceBatchResponse, EvidenceBatchRow, EvidenceBatchSummary, EvidenceResponse,
    EvidenceStatusCounts, batch_completion,
};
use crate::response::{CommandResponse, ResponseBody, project_legacy};

use super::{
    EvidenceVerification, MAX_BATCH_INPUT_BYTES, MAX_BATCH_VERIFIED_BYTES, batch_limit_error,
    bind_batch_scopes, parse_batch_pointers, verify_batch_row,
};

pub(super) fn run_evidence_batch(
    ctx: &crate::invocation::Invocation<'_>,
    registry_input: &EvidenceRegistry,
    path: &Path,
    strict: bool,
) -> Result<Value> {
    let bytes = read_bounded_regular_file(path, MAX_BATCH_INPUT_BYTES).map_err(|error| {
        if matches!(
            error.error_code(),
            "KIO-E-STORE-OBJECT-TOO-LARGE-001" | "KIO-E-STORE-OBJECT-OVERSIZED-001"
        ) {
            batch_limit_error(0, "file_bytes", MAX_BATCH_INPUT_BYTES as usize)
        } else {
            error
        }
    })?;
    let pointers = parse_batch_pointers(&bytes)?;
    let input_sha256 = hash_bytes(&bytes);

    let (verifications, _) =
        kio_core::cas::VerifiedCasReadBudget::new().run(MAX_BATCH_VERIFIED_BYTES, |reads| {
            let bindings = bind_batch_scopes(ctx, registry_input, &pointers, strict, reads)?;
            let mut cache = BTreeMap::<Vec<u8>, EvidenceVerification>::new();
            let verifications = pointers
                .iter()
                .map(|pointer| {
                    verify_batch_row(ctx, registry_input, pointer, strict, &bindings, &mut cache)
                })
                .collect::<Result<Vec<_>>>()?;
            for binding in &bindings {
                binding.recheck(ctx, registry_input)?;
            }
            Ok(verifications)
        })?;
    let mut counts = EvidenceStatusCounts::default();
    let mut permanent = false;
    let mut retryable = false;
    let results = verifications
        .iter()
        .enumerate()
        .map(|(index, verification)| {
            counts.record(verification.result.status);
            if strict {
                match verification.exit {
                    ExitCode::PermanentFailure => permanent = true,
                    ExitCode::PartialFailure => retryable = true,
                    _ => {}
                }
            } else if verification.exit == ExitCode::PartialFailure {
                retryable = true;
            }
            EvidenceBatchRow {
                line: index + 1,
                result: verification.result.clone(),
            }
        })
        .collect::<Vec<_>>();
    project_legacy(CommandResponse {
        body: ResponseBody::Evidence(EvidenceResponse::Batch(EvidenceBatchResponse {
            schema: "kio.evidence.batch-verify".to_owned(),
            schema_version: 1,
            input_sha256,
            strict,
            results,
            summary: EvidenceBatchSummary {
                total: pointers.len(),
                status_counts: counts,
            },
            verified_count: pointers.len(),
        })),
        completion: batch_completion(strict, permanent, retryable),
    })
}
