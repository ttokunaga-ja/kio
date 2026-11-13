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

#[test]
fn st04_cli_archive_budgets_are_optional_positive_u64() {
    use clap::Parser;
    for command in [vec!["kio", "index"], vec!["kio", "snapshot", "create"]] {
        assert!(crate::args::Cli::try_parse_from(&command).is_ok());
        for value in ["1", "536870913", "4294967297", "18446744073709551615"] {
            let mut args = command.clone();
            args.extend(["--max-file-bytes", value, "--max-total-bytes", value]);
            assert!(crate::args::Cli::try_parse_from(args).is_ok());
        }
        for value in ["0", "-1", "18446744073709551616"] {
            let mut args = command.clone();
            args.extend(["--max-file-bytes", value]);
            assert!(crate::args::Cli::try_parse_from(args).is_err());
        }
    }
}
