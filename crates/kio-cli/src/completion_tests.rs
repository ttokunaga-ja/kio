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
