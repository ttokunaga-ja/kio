use super::completion_exit_code;
use kio_app::response::Completion;

#[test]
fn c4_completion_maps_to_existing_numeric_process_exits() {
    for (completion, expected_exit, expected_error) in [
        (Completion::Complete, 0, None),
        (Completion::Partial, 3, Some("KIO-E-SEARCH-PARTIAL-001")),
        (
            Completion::Permanent,
            4,
            Some("KIO-E-SEARCH-SCOPE-ALL-FAILED-001"),
        ),
        (Completion::AuthRequired, 5, Some("KIO-E-ADAPTER-AUTH-001")),
        (
            Completion::BudgetBlocked,
            6,
            Some("KIO-E-BUDGET-EXCEEDED-001"),
        ),
    ] {
        let exit_code = completion_exit_code(completion);
        assert_eq!(exit_code.code(), expected_exit);
        if let Some(error_code) = expected_error {
            assert_eq!(kio_app::non_success_error_code(exit_code), error_code);
        }
    }
}

#[test]
fn resource_limit_cli_contract_keeps_exit_three_and_exact_context() {
    let error = kio_core::error::ResourceLimit {
        resource: kio_core::error::Resource::Storage,
        stage: "raw_publish".to_owned(),
        limit: None,
        observed: Some(42),
        resource_tier: None,
    }
    .into_error("storage exhausted");
    assert_eq!(error.exit_code().code(), 3);
    assert_eq!(
        error.to_error_json(),
        serde_json::json!({
            "error_code":"KIO-E-RESOURCE-LIMIT-001", "message":"storage exhausted",
            "context":{"resource":"storage","stage":"raw_publish","limit":null,"observed":42,"resource_tier":null,"retry_disposition":"explicit_after_resource_change"}
        })
    );
}
