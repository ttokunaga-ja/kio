use crate::context::EvidenceRegistry;
use std::collections::BTreeMap;
use std::path::Path;

use kio_core::cas::{hash_bytes, read_bounded_regular_file};
use kio_core::{ExitCode, Result};
use serde_json::{Value, json};

use crate::set_exit_override;

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
    let mut counts = BTreeMap::<&str, usize>::new();
    for key in [
        "alive",
        "tombstoned",
        "not_found",
        "scope_unreachable",
        "unverifiable",
        "registry_duplicate",
    ] {
        counts.insert(key, 0);
    }
    let mut permanent = false;
    let mut retryable = false;
    let results = verifications
        .iter()
        .enumerate()
        .map(|(index, verification)| {
            let status = verification.result.status.as_str();
            *counts.get_mut(status).expect("closed status set") += 1;
            if strict {
                match verification.exit {
                    ExitCode::PermanentFailure => permanent = true,
                    ExitCode::PartialFailure => retryable = true,
                    _ => {}
                }
            } else if verification.exit == ExitCode::PartialFailure {
                retryable = true;
            }
            json!({"line": index + 1, "result": verification.result})
        })
        .collect::<Vec<_>>();
    let mut output = json!({
        "schema": "kio.evidence.batch-verify",
        "schema_version": 1,
        "input_sha256": input_sha256,
        "strict": strict,
        "results": results,
        "summary": {"total": pointers.len(), "status_counts": counts},
        "verified_count": pointers.len(),
    });
    let exit = if strict && permanent {
        ExitCode::PermanentFailure
    } else if retryable {
        ExitCode::PartialFailure
    } else {
        ExitCode::Success
    };
    if exit != ExitCode::Success {
        set_exit_override(&mut output, exit);
    }
    Ok(output)
}
