use std::sync::{Arc, Mutex};

use kio_core::{Result, test_control::TestEnvGuard};
use serde_json::{Value, json};

use super::{AppContext, Interaction, test_support};
use crate::{Command, commands::PointerArgs};

struct NestedInput {
    inner: AppContext,
    pointer: Vec<u8>,
    observed: Mutex<Option<Value>>,
}

impl Interaction for NestedInput {
    fn confirm(&self, _: &str) -> Result<bool> {
        Ok(false)
    }

    fn read_input(&self, max_bytes: usize) -> Result<Vec<u8>> {
        assert!(self.pointer.len() <= max_bytes);
        let result = crate::execute(&self.inner, Command::Status)?;
        let mut observed = self.observed.lock().unwrap();
        assert!(observed.is_none(), "outer input is consumed exactly once");
        *observed = Some(result);
        Ok(self.pointer.clone())
    }

    fn is_interactive(&self) -> bool {
        false
    }

    fn diagnostic(&self, _: &str) {}
    fn write_diagnostic(&self, _: &str) -> kio_core::Result<()> {
        Ok(())
    }
    fn flush_diagnostics(&self) -> kio_core::Result<()> {
        Ok(())
    }
}

fn outcome(result: Result<Value>) -> Value {
    match result {
        Ok(value) => json!({"value": value}),
        Err(error) => json!({
            "error_code": error.error_code(),
            "message": error.message(),
            "context": error.context(),
            "exit_code": error.exit_code() as u8,
        }),
    }
}

#[test]
fn nested_execute_keeps_outer_repository_and_interaction() {
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let _environment = [
        TestEnvGuard::set("HOME", &base),
        TestEnvGuard::set("XDG_CONFIG_HOME", base.join("config")),
        TestEnvGuard::set("XDG_DATA_HOME", base.join("data")),
        TestEnvGuard::set("XDG_CACHE_HOME", base.join("cache")),
    ];
    let outer_root = base.join("outer");
    let inner_root = base.join("inner");
    std::fs::create_dir(&outer_root).unwrap();
    std::fs::create_dir(&inner_root).unwrap();
    let crate::management::ExplicitRoot::Created(outer_repo) =
        crate::management::initialize_explicit_root(
            &outer_root,
            &kio_core::control::ControlHandle::default(),
        )
        .unwrap()
    else {
        panic!("fresh outer scope already exists");
    };
    let crate::management::ExplicitRoot::Created(inner_repo) =
        crate::management::initialize_explicit_root(
            &inner_root,
            &kio_core::control::ControlHandle::default(),
        )
        .unwrap()
    else {
        panic!("fresh inner scope already exists");
    };
    let hash = outer_repo.object_store().write_raw(b"outer-only").unwrap();
    let scope_id = outer_repo.scope_identity().unwrap().scope_id;
    let pointer = format!(
        "kio://{scope_id}/object/raw/{}",
        hash.strip_prefix("sha256:").unwrap()
    );
    let control = test_support::context(&outer_root);
    let settings = Arc::new(kio_adapter::tool_lock::AdapterRuntimeSettings::default());
    let invocation_control =
        kio_core::control::ControlHandle::new(kio_core::test_control::DebugTestControl::from_env());
    let invocation = crate::Invocation {
        context: &control,
        settings: &settings,
        control: &invocation_control,
    };
    let expected = outcome(crate::run_view(
        &invocation,
        PointerArgs {
            pointer: Some(pointer.clone()),
        },
    ));
    assert_ne!(
        expected["error_code"],
        "KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001"
    );
    let interaction = Arc::new(NestedInput {
        inner: test_support::context(&inner_root),
        pointer: pointer.into_bytes(),
        observed: Mutex::new(None),
    });
    let outer = AppContext {
        working_directory: outer_root,
        interaction: interaction.clone(),
    };
    let actual = outcome(crate::execute(
        &outer,
        Command::View(PointerArgs {
            pointer: Some("-".to_owned()),
        }),
    ));
    let observed = interaction.observed.lock().unwrap();
    assert_eq!(
        observed.as_ref().unwrap()["scope_path"],
        json!(inner_repo.kio_dir())
    );
    assert_eq!(actual, expected);
}
