use super::completion_exit_code;
use kio_app::response::Completion;

#[test]
fn c4_completion_maps_to_existing_numeric_process_exits() {
    for (completion, expected) in [
        (Completion::Complete, 0),
        (Completion::Partial, 3),
        (Completion::Permanent, 4),
        (Completion::AuthRequired, 5),
        (Completion::BudgetBlocked, 6),
    ] {
        assert_eq!(completion_exit_code(completion).code(), expected);
    }
}
