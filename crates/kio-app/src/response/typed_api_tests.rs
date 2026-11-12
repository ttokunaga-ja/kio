use super::device::InitResponse;
use super::{CommandResponse, Completion, ResponseBody};
use crate::commands::CommandRequest;
use crate::context::AppContext;
use serde_json::json;
use std::path::PathBuf;

#[test]
fn c4_public_execute_returns_command_response() {
    let _execute: fn(&AppContext, CommandRequest) -> kio_core::Result<CommandResponse> =
        crate::execute;
}

#[test]
fn c4_public_response_keeps_completion_outside_payload_wire_shape() {
    for completion in [
        Completion::Complete,
        Completion::Partial,
        Completion::Permanent,
        Completion::AuthRequired,
        Completion::BudgetBlocked,
    ] {
        let response = CommandResponse {
            body: ResponseBody::Init(InitResponse {
                status: "initialized".into(),
                repaired: vec![],
                path: PathBuf::from("scope/日本語"),
                kio_path: PathBuf::from("scope/日本語/.kio"),
                independent: None,
            }),
            completion,
        };
        assert_eq!(response.completion, completion);
        let actual = serde_json::to_value(response.body).expect("test-only public body projection");
        let expected = json!({"status":"initialized","repaired":[],"path":PathBuf::from("scope/日本語"),
            "kio_path":PathBuf::from("scope/日本語/.kio")});
        assert_eq!(actual, expected);
        assert_eq!(
            serde_json::to_vec(&actual).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }
}
