//! W1-b: operation-specific startup dependencies and read-only budget degradation.
//! APP-01's durable free-execution state is deliberately outside these tests.
use crate::commands::{CommandRequest, IndexArgs, InitArgs, SearchMode};
use crate::context::test_support::context;
use crate::response::ResponseBody;
use crate::test_support::{
    PrivateTempDir, create_private_test_directory, create_private_test_file,
};
use kio_core::control::ControlHandle;
use kio_core::test_control::TestEnvGuard;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIBWTCB/6ADAgECAhR5P0J0YMFaZPlrOFyN8/jwpGzhqjAKBggqhkjOPQQDAjAh\nMR8wHQYDVQQDDBZyY2dlbiBzZWxmIHNpZ25lZCBjZXJ0MCAXDTc1MDEwMTAwMDAw\nMFoYDzQwOTYwMTAxMDAwMDAwWjAhMR8wHQYDVQQDDBZyY2dlbiBzZWxmIHNpZ25l\nZCBjZXJ0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEGhIwEPQnvWSlH+iUQ5Ui\nq0khmBj4hlxmW+mTL5xjjyxwwopOnkxLs2AofasS2lYPdILzMaIeRE78g2S7Hz1n\nX6MTMBEwDwYDVR0RBAgwBocEfwAAATAKBggqhkjOPQQDAgNJADBGAiEA0GBG4uEL\nSJJea18R17+yhRTyn3rsD6PzDhdg7F/v2sQCIQCccgcgjDh+ABNajeb1deKZoqbx\nnP4qBrGe09azOI4jbg==\n-----END CERTIFICATE-----\n";

// As in contextual_embedding_tests, no ambient credentials, selectors or XDG
// roots enter the child. Unlike that helper this does NOT initialize billing.
pub(super) fn isolated(test: &str) -> bool {
    const CHILD: &str = "KIO_OPERATION_PRECONDITIONS_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(test) {
        return true;
    }
    let private = PrivateTempDir::new();
    for name in ["home", "config", "data", "cache", "state", "tmp"] {
        create_private_test_directory(&private.path().join(name));
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child.env_clear();
    for name in ["PATH", "SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            child.env(name, value);
        }
    }
    let output = child
        .args(["--exact", test, "--nocapture"])
        .env(CHILD, test)
        .env("HOME", private.path().join("home"))
        .env("USERPROFILE", private.path().join("home"))
        .env("XDG_CONFIG_HOME", private.path().join("config"))
        .env("XDG_DATA_HOME", private.path().join("data"))
        .env("XDG_CACHE_HOME", private.path().join("cache"))
        .env("XDG_STATE_HOME", private.path().join("state"))
        .env("TMPDIR", private.path().join("tmp"))
        .env("TMP", private.path().join("tmp"))
        .env("TEMP", private.path().join("tmp"))
        .env("KIO_EVAL_DETERMINISTIC_EMBED", "scale-v3")
        .current_dir(private.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{test}:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

fn private_bytes(path: &Path, bytes: &[u8]) {
    create_private_test_file(path).write_all(bytes).unwrap();
}

fn replace_private_bytes(path: &Path, bytes: &[u8]) {
    let directory = kio_core::private_fs::verify_private_directory(path.parent().unwrap()).unwrap();
    directory
        .write_atomic(
            Path::new(path.file_name().unwrap()),
            bytes,
            kio_core::store_dir::Publication::Replace,
        )
        .unwrap();
}

fn index(online: bool) -> CommandRequest {
    CommandRequest::Index(IndexArgs {
        preview: false,
        yes: true,
        online,
        offline: !online,
        realtime: online,
        batch: false,
    })
}

fn text_search() -> CommandRequest {
    let mut args = super::search_request(Some(SearchMode::Text));
    args.query = Some("needle".into());
    CommandRequest::Search(args)
}

fn fixture() -> PrivateTempDir {
    let root = PrivateTempDir::new();
    private_bytes(
        &root.path().join("note.md"),
        b"# Local note\n\nneedle stays searchable\n",
    );
    crate::execute(
        &context(root.path()),
        CommandRequest::Init(InitArgs {
            path: None,
            independent: true,
            yes: true,
        }),
    )
    .unwrap();
    crate::execute(&context(root.path()), index(false)).unwrap();
    root
}

fn initialize_ledger() {
    drop(
        kio_pipeline::ledger::LedgerDb::initialize(crate::ledger_access::ledger_db_path(
            &ControlHandle::default(),
        ))
        .unwrap(),
    );
}

#[derive(Clone, Copy, Debug)]
enum Unused {
    Tools,
    RetiredAdapterConfig,
    Trust,
}

fn install_unused(dependency: Unused) -> (PathBuf, Vec<u8>) {
    let control = ControlHandle::default();
    let config = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()).join("kio");
    if !config.exists() {
        create_private_test_directory(&config);
    }
    let (path, bytes) = match dependency {
        Unused::Tools => (config.join("tools.toml"), b"[embedding\n".to_vec()),
        Unused::RetiredAdapterConfig => (
            config.join("config.toml"),
            b"[adapter.policy.offline_api]\nca_pem_path = '/unused/retired.pem'\n".to_vec(),
        ),
        Unused::Trust => (
            crate::trust::directory_path(&control),
            b"not a trust directory".to_vec(),
        ),
    };
    // Each fixture is installed separately: a tools failure must not mask a CA
    // or retired-config regression. The existing valid local baseline is real.
    private_bytes(&path, &bytes);
    (path, bytes)
}

fn local_result(root: &Path, request: CommandRequest) -> crate::response::CommandResponse {
    let interaction =
        std::sync::Arc::new(crate::test_support::DiagnosticInteraction::new(None, false));
    let ctx = crate::context::AppContext {
        working_directory: root.to_path_buf(),
        interaction: interaction.clone(),
    };
    let output =
        crate::execute(&ctx, request).expect("unused adapter dependency blocked local result");
    // Readiness may be structured in the response or a separate diagnostic;
    // the contract fixes the observation, not a new response field name.
    let disclosure = format!(
        "{} {}",
        serde_json::to_value(&output.body).unwrap(),
        String::from_utf8_lossy(&interaction.bytes())
    )
    .to_ascii_lowercase();
    assert!(
        disclosure.contains("adapter")
            && (disclosure.contains("reason") || disclosure.contains("kio-e-")),
        "local result lost separate adapter readiness reason: {disclosure}"
    );
    output
}

fn remove_fixture_file(path: &Path, expected: &[u8]) {
    assert_eq!(fs::read(path).unwrap(), expected);
    // Only our fresh synthetic file is retired; never remove user/cache state.
    fs::remove_file(path).unwrap();
}

#[test]
fn status_and_init_ignore_each_unused_adapter_dependency() {
    if !isolated(
        "tests::operation_preconditions_tests::status_and_init_ignore_each_unused_adapter_dependency",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    initialize_ledger();
    for dependency in [Unused::Tools, Unused::RetiredAdapterConfig, Unused::Trust] {
        let root = fixture();
        let (path, bytes) = install_unused(dependency);
        let output = local_result(root.path(), CommandRequest::Status);
        assert!(matches!(output.body, ResponseBody::Status(_)));
        let fresh = PrivateTempDir::new();
        let output = local_result(
            fresh.path(),
            CommandRequest::Init(InitArgs {
                path: None,
                independent: true,
                yes: true,
            }),
        );
        assert!(matches!(output.body, ResponseBody::Init(_)));
        assert!(fresh.path().join(".kio/scope.json").is_file());
        remove_fixture_file(&path, &bytes);
    }
}

#[test]
fn offline_index_and_text_search_ignore_each_unused_adapter_dependency() {
    if !isolated(
        "tests::operation_preconditions_tests::offline_index_and_text_search_ignore_each_unused_adapter_dependency",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    initialize_ledger();
    for dependency in [Unused::Tools, Unused::RetiredAdapterConfig, Unused::Trust] {
        let root = fixture();
        let (path, bytes) = install_unused(dependency);
        // A new input proves this run publishes content, rather than accepting a
        // cached success from the clean baseline.
        fs::write(root.path().join("second.md"), "needle newly indexed\n").unwrap();
        let output = local_result(root.path(), index(false));
        assert!(matches!(
            output.body,
            ResponseBody::Index(crate::response::index::IndexResponse::Published(_))
        ));
        let output = local_result(root.path(), text_search());
        let ResponseBody::Search(search) = output.body else {
            panic!("expected search");
        };
        assert!(
            !search.results.is_empty(),
            "local hits must survive {dependency:?}"
        );
        assert_eq!(
            serde_json::to_value(search).unwrap()["resolved_mode"],
            "text"
        );
        remove_fixture_file(&path, &bytes);
    }
}

fn tree(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    if path.exists() {
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(tree(&path));
            } else {
                files.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
    }
    files
}

fn billing_tree(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    tree(path)
        .into_iter()
        .filter(|(file, _)| {
            file.strip_prefix(path)
                .unwrap()
                .components()
                .any(|component| {
                    let name = component.as_os_str().to_string_lossy();
                    name.starts_with("cost-ledger") || name.starts_with("ledger.")
                })
        })
        .collect()
}

fn objects<'a>(value: &'a Value, rows: &mut Vec<&'a serde_json::Map<String, Value>>) {
    match value {
        Value::Object(row) => {
            rows.push(row);
            for value in row.values() {
                objects(value, rows);
            }
        }
        Value::Array(values) => {
            for value in values {
                objects(value, rows);
            }
        }
        _ => {}
    }
}

fn assert_unknown_budget(value: &Value, original_code: &str) {
    let mut rows = Vec::new();
    objects(value, &mut rows);
    let observations = rows
        .iter()
        .filter(|row| row.contains_key("budget_status"))
        .collect::<Vec<_>>();
    assert!(
        !observations.is_empty(),
        "missing budget observation: {value}"
    );
    for row in observations {
        assert_eq!(row["budget_status"], "unknown");
        let reason = row
            .get("budget_reason")
            .expect("unknown budget requires reason");
        assert!(
            reason.to_string().contains(original_code),
            "lost typed reason {original_code}: {reason}"
        );
    }
    for row in rows {
        if let Some(paused) = row.get("budget_paused") {
            assert!(
                paused.is_null(),
                "unknown is not budget_paused=false: {value}"
            );
        }
        for key in [
            "device_spent_usd",
            "folder_spent_usd",
            "device_remaining_usd",
            "folder_remaining_usd",
        ] {
            if let Some(amount) = row.get(key) {
                assert!(amount.is_null(), "unknown {key}: {amount}");
            }
        }
    }
}

#[test]
fn local_results_preserve_unknown_budget_reason_without_ledger_mutation() {
    if !isolated(
        "tests::operation_preconditions_tests::local_results_preserve_unknown_budget_reason_without_ledger_mutation",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    initialize_ledger();
    let root = fixture();
    for corrupt in [false, true] {
        for operation in ["status", "search", "index"] {
            // Baseline publication happens with a valid ledger. Only observation
            // uses a fresh unavailable store, avoiding APP-01 executor coupling.
            let device = PrivateTempDir::new();
            create_private_test_directory(&device.path().join("kio"));
            let _data = TestEnvGuard::set("XDG_DATA_HOME", device.path());
            let control = ControlHandle::default();
            let path = crate::ledger_access::ledger_db_path(&control);
            if corrupt {
                private_bytes(&path, b"not a SQLite ledger\n");
            }
            let original = match crate::ledger_access::open_search_budget_snapshot(&path, &control)
            {
                Err(error) => error.error_code().to_owned(),
                Ok(None) => "KIO-E-LEDGER-UNINITIALIZED-001".to_owned(),
                Ok(Some(_)) => panic!("fixture unexpectedly has a usable ledger"),
            };
            let before = billing_tree(device.path());
            let request = match operation {
                "status" => CommandRequest::Status,
                "search" => {
                    let mut args = super::search_request(Some(SearchMode::Text));
                    args.query = Some("needle".into());
                    args.scope = Some(root.path().to_path_buf());
                    CommandRequest::Search(args)
                }
                _ => index(false),
            };
            let output = crate::execute(&context(root.path()), request).unwrap_or_else(|e| {
                panic!("{operation}, corrupt={corrupt} lost local result: {e}")
            });
            match &output.body {
                ResponseBody::Status(_) if operation == "status" => {}
                ResponseBody::Search(search) if operation == "search" => {
                    assert!(!search.results.is_empty())
                }
                ResponseBody::Index(crate::response::index::IndexResponse::Published(_))
                    if operation == "index" => {}
                _ => panic!("wrong local response: {:?}", output.body),
            }
            assert_unknown_budget(&serde_json::to_value(output.body).unwrap(), &original);
            assert_eq!(
                billing_tree(device.path()),
                before,
                "{operation} rewrote billing artifacts"
            );
        }
    }
}

#[test]
fn selected_adapter_profile_and_credential_stay_strict() {
    if !isolated(
        "tests::operation_preconditions_tests::selected_adapter_profile_and_credential_stay_strict",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    initialize_ledger();
    let root = fixture();
    let _local = TestEnvGuard::remove("KIO_EVAL_DETERMINISTIC_EMBED");
    let _mock = TestEnvGuard::set("KIO_TEST_GEMINI_EMBED", "mock");
    let trace = root.path().join("query-send-trace");
    let _trace = TestEnvGuard::set("KIO_TEST_QUERY_EMBED_TRACE", &trace);
    let config = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()).join("kio");
    if !config.exists() {
        create_private_test_directory(&config);
    }
    let path = config.join("tools.toml");
    // Explicitly selected built-in online profile, invalid replacement target.
    private_bytes(
        &path,
        b"[embedding.gemini_embedding_2]\nurl = 'http://127.0.0.1:1'\n",
    );
    let error = crate::execute(&context(root.path()), index(true)).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-CONFIG-SCHEMA-001");
    assert!(!trace.exists());
    remove_fixture_file(&path, &fs::read(&path).unwrap());

    // Real credential consumer rechecks its retained declaration against the
    // source at use time. A hardlink is rejected on every supported platform.
    const CANARY: &str = "synthetic-never-send-key-r36";
    let bytes = format!("[embedding.gemini_embedding_2]\nauth = 'plain:{CANARY}'\n");
    private_bytes(&path, bytes.as_bytes());
    let settings =
        crate::settings::adapter_runtime_settings_from_user_config(&ControlHandle::default())
            .unwrap();
    let alias = config.join("credential-alias");
    fs::hard_link(&path, &alias).unwrap();
    let error = settings.resolve_role_api_key("embedding").unwrap_err();
    let text = format!("{error:?} {error}");
    assert!(text.contains(kio_adapter::tool_lock::TOOLS_PRIVATE_CREDENTIAL_ERROR));
    assert!(!text.contains(CANARY));
    assert!(!trace.exists());
    let error = crate::execute(
        &context(root.path()),
        CommandRequest::Adapter(crate::commands::AdapterArgs {
            command: Some(crate::commands::AdapterCommand::Approve(
                crate::commands::ApproveArgs {
                    target: crate::commands::AdapterTarget::Tool("gemini_embedding_2".into()),
                    preview: false,
                    yes: true,
                    resume: None,
                    send_secrets: false,
                },
            )),
        }),
    )
    .unwrap_err();
    // The bounded metadata reader rejects this hardlink before plaintext
    // classification. The retained credential consumer above still proves
    // the dedicated private-credential check at actual key use.
    assert_eq!(error.error_code(), "KIO-E-STORE-CORRUPT-001");
    assert!(!error.to_error_json().to_string().contains(CANARY));
    assert!(!trace.exists());

    // The HTTPS dependency consumer must still refuse a changed active CA.

    let ca = root.path().join("source.pem");
    private_bytes(&ca, PEM.as_bytes());
    let control = ControlHandle::default();
    let trust = crate::trust::directory_path(&control);
    let record = crate::trust::register(&trust, &ca, false, None, &control).unwrap();
    let mut record = serde_json::to_value(record).unwrap();
    let digest = format!("sha256:{}", "a".repeat(64));
    let snapshot = format!("ca-{}.pem", "a".repeat(64));
    record["digest"] = Value::String(digest);
    record["snapshot"] = Value::String(snapshot.clone());
    private_bytes(&trust.join(snapshot), PEM.as_bytes());
    fs::write(
        trust.join("active.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    let before = tree(&trust);
    let error = match crate::trust::acquire_runtime_trust(&trust, &control) {
        Err(error) => error,
        Ok(_) => panic!("changed CA must refuse selected HTTPS authority"),
    };
    assert_eq!(error.error_code(), "KIO-E-LOCAL-TRUST-CHANGED-001");
    assert_eq!(tree(&trust), before);
}

#[test]
fn unknown_budget_never_authorizes_paid_query_dispatch() {
    if !isolated(
        "tests::operation_preconditions_tests::unknown_budget_never_authorizes_paid_query_dispatch",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for unavailable in ["approval", "corrupt", "absent"] {
        let device = PrivateTempDir::new();
        let _data = TestEnvGuard::set("XDG_DATA_HOME", device.path());
        initialize_ledger();
        let root = fixture();
        let _local = TestEnvGuard::remove("KIO_EVAL_DETERMINISTIC_EMBED");
        let _mock = TestEnvGuard::set("KIO_TEST_GEMINI_EMBED", "mock");
        let trace = root.path().join("paid-query-send-trace");
        let _trace = TestEnvGuard::set("KIO_TEST_QUERY_EMBED_TRACE", &trace);
        let ctx = context(root.path());
        crate::execute(
            &ctx,
            CommandRequest::Adapter(crate::commands::AdapterArgs {
                command: Some(crate::commands::AdapterCommand::Approve(
                    crate::commands::ApproveArgs {
                        target: crate::commands::AdapterTarget::Tool("gemini_embedding_2".into()),
                        preview: false,
                        yes: true,
                        resume: None,
                        send_secrets: false,
                    },
                )),
            }),
        )
        .unwrap();
        crate::execute(&ctx, index(true)).unwrap();
        // Publish the exact paid vector space and prove the selected query can
        // dispatch before withdrawing one prerequisite. Deterministic baseline
        // vectors alone would fail compatibility before testing admission.
        let mut args = super::search_request(Some(SearchMode::Vector));
        args.query = Some(format!("paid query positive control {unavailable}"));
        args.online = true;
        crate::execute(&ctx, CommandRequest::Search(args)).unwrap();
        assert!(
            trace.is_file(),
            "fixture did not admit the selected mocked paid query"
        );
        fs::remove_file(&trace).unwrap();
        match unavailable {
            "approval" => {
                crate::execute(
                    &ctx,
                    CommandRequest::Adapter(crate::commands::AdapterArgs {
                        command: Some(crate::commands::AdapterCommand::Revoke(
                            crate::commands::RevokeArgs {
                                target: crate::commands::AdapterTarget::Tool(
                                    "gemini_embedding_2".into(),
                                ),
                            },
                        )),
                    }),
                )
                .unwrap();
            }
            "corrupt" => {
                let path = crate::ledger_access::ledger_db_path(&ControlHandle::default());
                fs::write(
                    path.with_extension("sqlite.checkpoint.json"),
                    b"broken checkpoint\n",
                )
                .unwrap();
            }
            _ => {
                // Retire only this isolated test's known synthetic billing
                // artifacts to model a never-initialized device. Product code
                // must leave that absence unchanged.
                for path in billing_tree(device.path()).into_keys() {
                    fs::remove_file(path).unwrap();
                }
                let path = crate::ledger_access::ledger_db_path(&ControlHandle::default());
                assert!(
                    matches!(
                        crate::ledger_access::open_search_budget_snapshot(
                            &path,
                            &ControlHandle::default()
                        ),
                        Ok(None)
                    ),
                    "missing-ledger fixture retained incomplete billing authority"
                );
            }
        }
        let before = billing_tree(device.path());
        let mut args = super::search_request(Some(SearchMode::Vector));
        args.query = Some(format!("paid query must not send {unavailable}"));
        args.online = true;
        match crate::execute(&ctx, CommandRequest::Search(args)) {
            Err(error) if unavailable == "absent" => {
                assert_eq!(
                    error.error_code(),
                    "KIO-E-SEARCH-VEC-UNAVAIL-001",
                    "{error}"
                );
                assert_eq!(
                    error.context()["fallback_reason"],
                    "query_embedding_unavailable"
                );
            }
            Err(error) if unavailable == "corrupt" => {
                assert!(error.error_code().starts_with("KIO-E-LEDGER-"), "{error}")
            }
            Err(error) => {
                assert_eq!(
                    error.error_code(),
                    "KIO-E-SEARCH-VEC-UNAUTHORIZED-001",
                    "{error}"
                );
                assert_eq!(
                    error.context()["fallback_reason"],
                    "embedding_not_authorized"
                );
            }
            Ok(response) => {
                let ResponseBody::Search(search) = response.body else {
                    panic!("expected search");
                };
                assert!(
                    search.error_code.is_some() || search.fallback_reason.is_some(),
                    "{unavailable}: no refusal reason"
                );
                assert_eq!(
                    serde_json::to_value(search).unwrap()["resolved_mode"],
                    "text"
                );
            }
        }
        assert!(!trace.exists(), "{unavailable} dispatched paid query");
        assert_eq!(
            billing_tree(device.path()),
            before,
            "refused query mutated billing artifacts"
        );
    }
}

#[test]
fn online_embedding_ignores_unused_markdown_and_trust() {
    if !isolated(
        "tests::operation_preconditions_tests::online_embedding_ignores_unused_markdown_and_trust",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    initialize_ledger();
    let root = fixture();
    let _local = TestEnvGuard::remove("KIO_EVAL_DETERMINISTIC_EMBED");
    let _mock = TestEnvGuard::set("KIO_TEST_GEMINI_EMBED", "mock");
    let trace = root.path().join("query-send-trace");
    let _trace = TestEnvGuard::set("KIO_TEST_QUERY_EMBED_TRACE", &trace);
    let config = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()).join("kio");
    if !config.exists() {
        create_private_test_directory(&config);
    }
    let tools = config.join("tools.toml");
    const SELECTED: &str =
        "[embedding.gemini_embedding_2]\nauth = 'plain:synthetic-selected-embedding-only'\n";
    private_bytes(&tools, SELECTED.as_bytes());
    let ctx = context(root.path());
    crate::execute(
        &ctx,
        CommandRequest::Adapter(crate::commands::AdapterArgs {
            command: Some(crate::commands::AdapterCommand::Approve(
                crate::commands::ApproveArgs {
                    target: crate::commands::AdapterTarget::Tool("gemini_embedding_2".into()),
                    preview: false,
                    yes: true,
                    resume: None,
                    send_secrets: false,
                },
            )),
        }),
    )
    .unwrap();
    crate::execute(&ctx, index(true)).unwrap();
    for unused in ["markdown", "trust", "markdown_transport"] {
        let guard = if unused == "markdown" {
            replace_private_bytes(
                &tools,
                format!(
                    "{SELECTED}[markdown.mistral_ocr_markdownize]\nurl = 'http://127.0.0.1:1'\n"
                )
                .as_bytes(),
            );
            None
        } else if unused == "trust" {
            replace_private_bytes(&tools, SELECTED.as_bytes());
            Some(install_unused(Unused::Trust))
        } else {
            replace_private_bytes(&tools, format!("{SELECTED}[markdown.paddleocr_vl_local]\nkind = 'offline_api'\nurl = 'https://127.0.0.1:8443'\n").as_bytes());
            Some(install_unused(Unused::RetiredAdapterConfig))
        };
        if unused == "markdown_transport" {
            // The selected Gemini grant must not validate an unrelated local
            // OCR transport merely because that role has valid metadata.
            crate::execute(
                &ctx,
                CommandRequest::Adapter(crate::commands::AdapterArgs {
                    command: Some(crate::commands::AdapterCommand::Approve(
                        crate::commands::ApproveArgs {
                            target: crate::commands::AdapterTarget::Tool(
                                "gemini_embedding_2".into(),
                            ),
                            preview: false,
                            yes: true,
                            resume: None,
                            send_secrets: false,
                        },
                    )),
                }),
            )
            .expect("unavailable unused Markdown transport blocked selected Gemini approval");
            assert!(
                !trace.exists(),
                "permission inspection invoked query transport"
            );
        }
        let mut args = super::search_request(Some(SearchMode::Vector));
        args.query = Some(format!("selected paid embedding ignores {unused}"));
        args.online = true;
        crate::execute(&ctx, CommandRequest::Search(args))
            .unwrap_or_else(|error| panic!("unused {unused} blocked selected embedding: {error}"));
        assert!(
            trace.is_file(),
            "selected embedding did not reach mocked query transport"
        );
        fs::remove_file(&trace).unwrap();
        if let Some((path, bytes)) = guard {
            remove_fixture_file(&path, &bytes);
        }
    }
}

#[test]
fn configured_https_ca_failure_does_not_block_no_work_index_or_text() {
    if !isolated(
        "tests::operation_preconditions_tests::configured_https_ca_failure_does_not_block_no_work_index_or_text",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    initialize_ledger();
    let root = fixture();
    let config = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()).join("kio");
    if !config.exists() {
        create_private_test_directory(&config);
    }
    private_bytes(
        &config.join("tools.toml"),
        b"[markdown.paddleocr_vl_local]\nkind = 'offline_api'\nurl = 'https://127.0.0.1:8443'\n",
    );
    let control = ControlHandle::default();
    let ca = root.path().join("source.pem");
    private_bytes(&ca, PEM.as_bytes());
    let trust = crate::trust::directory_path(&control);
    let record = crate::trust::register(&trust, &ca, false, None, &control).unwrap();
    let mut record = serde_json::to_value(record).unwrap();
    let snapshot = format!("ca-{}.pem", "a".repeat(64));
    record["digest"] = Value::String(format!("sha256:{}", "a".repeat(64)));
    record["snapshot"] = Value::String(snapshot.clone());
    private_bytes(&trust.join(snapshot), PEM.as_bytes());
    fs::write(
        trust.join("active.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    let before = tree(&trust);
    // Ordinary index mode, with no new content or OCR work, rather than the
    // explicit --offline bypass. Declaring a HTTPS role is not a selected send.
    let request = CommandRequest::Index(IndexArgs {
        preview: false,
        yes: true,
        online: false,
        offline: false,
        realtime: false,
        batch: false,
    });
    let output = local_result(root.path(), request);
    assert!(matches!(
        output.body,
        ResponseBody::Index(crate::response::index::IndexResponse::Published(_))
    ));
    let output = local_result(root.path(), text_search());
    let ResponseBody::Search(search) = output.body else {
        panic!("expected search");
    };
    assert!(!search.results.is_empty());
    assert_eq!(
        tree(&trust),
        before,
        "local no-work commands changed CA state"
    );
    // Explicit authorization consumes the selected HTTPS dependency strictly.
    let error = crate::execute(
        &context(root.path()),
        CommandRequest::Adapter(crate::commands::AdapterArgs {
            command: Some(crate::commands::AdapterCommand::Approve(
                crate::commands::ApproveArgs {
                    target: crate::commands::AdapterTarget::Tool("paddleocr_vl_local".into()),
                    preview: false,
                    yes: true,
                    resume: None,
                    send_secrets: false,
                },
            )),
        }),
    )
    .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-LOCAL-TRUST-CHANGED-001");
    assert_eq!(tree(&trust), before);
}

#[test]
fn local_commands_still_validate_consumed_budget_policy() {
    if !isolated(
        "tests::operation_preconditions_tests::local_commands_still_validate_consumed_budget_policy",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    initialize_ledger();
    let root = fixture();
    let config = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()).join("kio");
    if !config.exists() {
        create_private_test_directory(&config);
    }
    private_bytes(
        &config.join("config.toml"),
        b"[budget]\nmonthly_usd_cap = -1.0\n",
    );
    // These operations consume budget policy for their own decision path.
    for request in [CommandRequest::Status, index(false)] {
        let error = crate::execute(&context(root.path()), request).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-CONFIG-SCHEMA-001");
    }
    // 04 §5.4: an unavailable budget observation cannot suppress text results.
    // Paid admission still validates this policy before reserving or dispatching.
    let ledger_path = crate::ledger_access::ledger_db_path(&ControlHandle::default());
    let before = billing_tree(ledger_path.parent().unwrap());
    let response = crate::execute(&context(root.path()), text_search()).unwrap();
    let ResponseBody::Search(search) = response.body else {
        panic!("text search must retain local results");
    };
    assert!(!search.results.is_empty());
    assert_unknown_budget(
        &serde_json::to_value(search).unwrap(),
        "KIO-E-CONFIG-SCHEMA-001",
    );
    assert_eq!(billing_tree(ledger_path.parent().unwrap()), before);
}

#[test]
fn managed_trust_cache_holds_one_lease_until_invocation_drop() {
    if !isolated(
        "tests::operation_preconditions_tests::managed_trust_cache_holds_one_lease_until_invocation_drop",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let root = PrivateTempDir::new();
    let ca = root.path().join("source.pem");
    private_bytes(&ca, PEM.as_bytes());
    let control = ControlHandle::default();
    let trust = crate::trust::directory_path(&control);
    crate::trust::register(&trust, &ca, false, None, &control).unwrap();
    let provider = std::sync::Arc::new(crate::trust::RuntimeTrustProvider::new(control.clone()));
    let settings = kio_adapter::tool_lock::AdapterRuntimeSettings {
        managed_local_trust: Some(provider.clone()),
        ..kio_adapter::tool_lock::AdapterRuntimeSettings::with_control(control)
    };
    use kio_adapter::tool_lock::ManagedLocalTrustProvider;
    let binding = provider.binding().unwrap();
    assert!(
        !provider.acquired(),
        "metadata lookup must not acquire the CA lock"
    );
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(trust.join(".lock"))
        .unwrap();
    lock.try_lock()
        .expect("metadata lookup retained a filesystem lock");
    lock.unlock().unwrap();
    let path = settings.local_peer_ca_path().unwrap();
    assert!(path.is_file());
    assert_eq!(settings.local_peer_trust_binding().unwrap(), binding);
    assert!(!settings.local_peer_trust_digest().unwrap().is_empty());
    settings
        .local_endpoint("markdown", "https://127.0.0.1:8443")
        .unwrap();
    assert!(
        lock.try_lock().is_err(),
        "selected HTTPS must retain the CA lock"
    );
    drop(settings);
    assert!(
        lock.try_lock().is_err(),
        "another reference still owns this invocation's lease"
    );
    drop(provider);
    lock.try_lock()
        .expect("dropping the invocation did not release CA lock");
    lock.unlock().unwrap();
}

#[test]
fn managed_trust_change_between_identity_and_send_refuses_dispatch() {
    if !isolated(
        "tests::operation_preconditions_tests::managed_trust_change_between_identity_and_send_refuses_dispatch",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let root = PrivateTempDir::new();
    let ca = root.path().join("source.pem");
    private_bytes(&ca, PEM.as_bytes());
    let control = ControlHandle::default();
    let trust = crate::trust::directory_path(&control);
    crate::trust::register(&trust, &ca, false, None, &control).unwrap();
    let provider = crate::trust::RuntimeTrustProvider::new(control.clone());
    use kio_adapter::tool_lock::ManagedLocalTrustProvider;
    let first = provider.binding().unwrap();
    assert!(!provider.acquired());
    crate::trust::revoke(&trust, false, &control).unwrap();
    crate::trust::register(&trust, &ca, false, None, &control).unwrap();
    let before = tree(&trust);
    let error = provider.acquire().unwrap_err();
    assert_eq!(
        error.error_code(),
        "KIO-E-LOCAL-TRUST-CHANGED-001",
        "{first}: {error}"
    );
    assert!(!provider.acquired());
    assert_eq!(
        tree(&trust),
        before,
        "failed acquisition changed trust state"
    );
}

#[test]
fn snapshot_ignores_unconsumed_adapter_configuration_and_credentials() {
    if !isolated(
        "tests::operation_preconditions_tests::snapshot_ignores_unconsumed_adapter_configuration_and_credentials",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    initialize_ledger();
    let root = fixture();
    let _local = TestEnvGuard::remove("KIO_EVAL_DETERMINISTIC_EMBED");
    for dependency in [Unused::Tools, Unused::RetiredAdapterConfig, Unused::Trust] {
        let (path, bytes) = install_unused(dependency);
        fs::write(
            root.path().join("note.md"),
            format!("# Snapshot {dependency:?}\n\nlocal snapshot needle\n"),
        )
        .unwrap();
        let output = local_result(
            root.path(),
            CommandRequest::Snapshot(crate::commands::SnapshotArgs {
                action: crate::commands::SnapshotAction::Create(
                    crate::commands::SnapshotCreateArgs {
                        message: Some(format!("local snapshot {dependency:?}")),
                    },
                ),
            }),
        );
        let ResponseBody::Snapshot(crate::response::snapshot::SnapshotResponse::Create(snapshot)) =
            output.body
        else {
            panic!("expected manual snapshot");
        };
        assert!(
            snapshot.base.commit_hash.is_some(),
            "snapshot did not capture new local content"
        );
        let output = local_result(
            root.path(),
            CommandRequest::Snapshot(crate::commands::SnapshotArgs {
                action: crate::commands::SnapshotAction::Auto,
            }),
        );
        let ResponseBody::Snapshot(crate::response::snapshot::SnapshotResponse::Auto(auto)) =
            output.body
        else {
            panic!("expected scheduled snapshot response");
        };
        assert_eq!(auto.reason, "disabled");
        remove_fixture_file(&path, &bytes);
    }
    // This is a correctly private synthetic credential file with an unsafe
    // link count. Snapshot never consumes the credential or its pricing.
    let config = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()).join("kio");
    let tools = config.join("tools.toml");
    let credential =
        b"[embedding.gemini_embedding_2]\nauth = 'plain:synthetic-unused-snapshot-secret'\n";
    private_bytes(&tools, credential);
    let alias = config.join("snapshot-credential-alias");
    fs::hard_link(&tools, &alias).unwrap();
    fs::write(
        root.path().join("note.md"),
        "# Snapshot ignores an unused credential\n\nneedle\n",
    )
    .unwrap();
    let output = local_result(
        root.path(),
        CommandRequest::Snapshot(crate::commands::SnapshotArgs {
            action: crate::commands::SnapshotAction::Create(crate::commands::SnapshotCreateArgs {
                message: Some("snapshot without credential consumption".into()),
            }),
        }),
    );
    let ResponseBody::Snapshot(crate::response::snapshot::SnapshotResponse::Create(snapshot)) =
        output.body
    else {
        panic!("expected manual snapshot");
    };
    assert!(snapshot.base.commit_hash.is_some());
    assert_eq!(fs::read(&tools).unwrap(), credential);
    assert_eq!(fs::read(&alias).unwrap(), credential);
    // Snapshot does not consume device budget policy. Scope policy remains a
    // separate, mandatory content authority even for this local operation.
    let device_config = config.join("config.toml");
    let invalid_unused_budget = b"[budget]\nmonthly_usd_cap = -1.0\n";
    private_bytes(&device_config, invalid_unused_budget);
    fs::write(
        root.path().join("note.md"),
        "# Snapshot independent of device budget\n\nneedle\n",
    )
    .unwrap();
    let output = local_result(
        root.path(),
        CommandRequest::Snapshot(crate::commands::SnapshotArgs {
            action: crate::commands::SnapshotAction::Create(crate::commands::SnapshotCreateArgs {
                message: Some("unused device budget must not block content".into()),
            }),
        }),
    );
    let ResponseBody::Snapshot(crate::response::snapshot::SnapshotResponse::Create(snapshot)) =
        output.body
    else {
        panic!("expected manual snapshot");
    };
    assert!(snapshot.base.commit_hash.is_some());
    assert_eq!(fs::read(&device_config).unwrap(), invalid_unused_budget);
    fs::write(root.path().join(".kio/config.toml"), b"[scope\n").unwrap();
    let error = crate::execute(
        &context(root.path()),
        CommandRequest::Snapshot(crate::commands::SnapshotArgs {
            action: crate::commands::SnapshotAction::Create(crate::commands::SnapshotCreateArgs {
                message: None,
            }),
        }),
    )
    .unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-CONFIG-SCHEMA-001");
}

#[test]
fn configured_https_embedding_metadata_does_not_acquire_unused_ca() {
    if !isolated(
        "tests::operation_preconditions_tests::configured_https_embedding_metadata_does_not_acquire_unused_ca",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    initialize_ledger();
    let root = PrivateTempDir::new();
    crate::execute(
        &context(root.path()),
        CommandRequest::Init(InitArgs {
            path: None,
            independent: true,
            yes: true,
        }),
    )
    .unwrap();
    crate::execute(&context(root.path()), index(false)).unwrap();
    let _local = TestEnvGuard::remove("KIO_EVAL_DETERMINISTIC_EMBED");
    let config = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()).join("kio");
    if !config.exists() {
        create_private_test_directory(&config);
    }
    private_bytes(&config.join("tools.toml"), b"[embedding.qwen3_vl_embedding_local]\nkind = 'offline_api'\nurl = 'https://127.0.0.1:8443'\n");
    let control = ControlHandle::default();
    let ca = root.path().join("source.pem");
    private_bytes(&ca, PEM.as_bytes());
    let trust = crate::trust::directory_path(&control);
    let record = crate::trust::register(&trust, &ca, false, None, &control).unwrap();
    crate::execute(
        &context(root.path()),
        CommandRequest::Adapter(crate::commands::AdapterArgs {
            command: Some(crate::commands::AdapterCommand::Approve(
                crate::commands::ApproveArgs {
                    target: crate::commands::AdapterTarget::Tool("qwen3_vl_embedding_local".into()),
                    preview: false,
                    yes: true,
                    resume: None,
                    send_secrets: false,
                },
            )),
        }),
    )
    .unwrap();
    let snapshot = serde_json::to_value(record).unwrap()["snapshot"]
        .as_str()
        .unwrap()
        .to_owned();
    // Change only certificate bytes, leaving the approved lifecycle identity
    // untouched. The second bundle still parses, but has a different DER hash.
    let altered = PEM.replace("nP4qBrGe", "mP4qBrGe");
    assert_ne!(
        kio_adapter::local_peer::local_trust_digest_from_pem_bytes(PEM.as_bytes()).unwrap(),
        kio_adapter::local_peer::local_trust_digest_from_pem_bytes(altered.as_bytes()).unwrap()
    );
    fs::write(trust.join(snapshot), altered.as_bytes()).unwrap();
    let before = tree(&trust);
    let request = CommandRequest::Index(IndexArgs {
        preview: false,
        yes: true,
        online: false,
        offline: false,
        realtime: false,
        batch: false,
    });
    let output = local_result(root.path(), request);
    assert!(matches!(
        output.body,
        ResponseBody::Index(crate::response::index::IndexResponse::Published(_))
    ));
    assert_eq!(
        tree(&trust),
        before,
        "metadata-only indexing changed trust state"
    );
    // Once actual embedding input exists, the same selected destination must
    // refuse the changed CA before its HTTPS client can submit that input.
    fs::write(
        root.path().join("note.md"),
        "# Actual selected embedding\n\nneedle needs a vector\n",
    )
    .unwrap();
    let interaction =
        std::sync::Arc::new(crate::test_support::DiagnosticInteraction::new(None, false));
    let ctx = crate::context::AppContext {
        working_directory: root.path().to_path_buf(),
        interaction: interaction.clone(),
    };
    let request = CommandRequest::Index(IndexArgs {
        preview: false,
        yes: true,
        online: false,
        offline: false,
        realtime: false,
        batch: false,
    });
    let refusal = match crate::execute(&ctx, request) {
        Err(error) => format!("{error:?} {error}"),
        Ok(output) => format!(
            "{} {}",
            serde_json::to_value(output.body).unwrap(),
            String::from_utf8_lossy(&interaction.bytes())
        ),
    };
    assert!(
        refusal.contains("KIO-E-LOCAL-TRUST-CHANGED-001"),
        "selected HTTPS failure lost typed CA reason: {refusal}"
    );
    assert_eq!(
        tree(&trust),
        before,
        "refused selected work changed CA state"
    );
}

#[test]
fn default_local_index_keeps_baseline_and_native_work_with_unavailable_markdown() {
    if !isolated(
        "tests::operation_preconditions_tests::default_local_index_keeps_baseline_and_native_work_with_unavailable_markdown",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    initialize_ledger();
    let root = fixture();
    let config = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()).join("kio");
    if !config.exists() {
        create_private_test_directory(&config);
    }
    let tools = config.join("tools.toml");
    let bytes = b"[markdown.mistral_ocr_markdownize]\nurl = 'http://127.0.0.1:1'\n";
    private_bytes(&tools, bytes);
    fs::write(
        root.path().join("second.md"),
        "# Locally published\n\nneedle available while unused markdown config is broken\n",
    )
    .unwrap();
    let output = local_result(
        root.path(),
        CommandRequest::Index(IndexArgs {
            preview: false,
            yes: true,
            online: false,
            offline: false,
            realtime: false,
            batch: false,
        }),
    );
    assert!(matches!(
        output.body,
        ResponseBody::Index(crate::response::index::IndexResponse::Published(_))
    ));
    let output = crate::execute(&context(root.path()), text_search()).unwrap();
    assert!(
        serde_json::to_value(output.body)
            .unwrap()
            .to_string()
            .contains("second.md")
    );
    let tasks = kio_pipeline::task::TaskStore::new_with_control(
        root.path().join(".kio"),
        ControlHandle::default(),
    )
    .all()
    .unwrap();
    assert!(
        !tasks
            .iter()
            .any(|task| task.output_ref.starts_with("online:")),
        "unavailable role was replaced by a default remote destination"
    );
    assert_eq!(fs::read(&tools).unwrap(), bytes);
}

fn exercise_unconsumed_device_config(operation: &str) {
    initialize_ledger();
    for bytes in [
        b"[budget]\nmonthly_usd_cap = -1.0\n".as_slice(),
        b"[search]\ndefault_mode = 'invalid-unused-mode'\n".as_slice(),
    ] {
        let root = fixture();
        let original = fs::read(root.path().join("note.md")).unwrap();
        let ancestor = fs::read_to_string(root.path().join(".kio/HEAD"))
            .unwrap()
            .trim()
            .to_owned();
        fs::write(
            root.path().join("note.md"),
            "# Current content\n\nneedle current descendant\n",
        )
        .unwrap();
        crate::execute(&context(root.path()), index(false)).unwrap();
        let current = fs::read_to_string(root.path().join(".kio/HEAD"))
            .unwrap()
            .trim()
            .to_owned();
        assert_ne!(ancestor, current, "restore fixture needs a real ancestor");
        let config = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()).join("kio");
        if !config.exists() {
            create_private_test_directory(&config);
        }
        let path = config.join("config.toml");
        private_bytes(&path, bytes);
        let output = match operation {
            "init" => {
                let fresh = PrivateTempDir::new();
                let response = crate::execute(
                    &context(fresh.path()),
                    CommandRequest::Init(InitArgs {
                        path: None,
                        independent: true,
                        yes: true,
                    }),
                )
                .expect("unused device budget/search blocked scope initialization");
                assert!(fresh.path().join(".kio/scope.json").is_file());
                response
            }
            "export" => {
                let destination = PrivateTempDir::new();
                let response = crate::execute(
                    &context(root.path()),
                    CommandRequest::Export(crate::commands::ExportArgs {
                        source: ancestor.clone(),
                        to: destination.path().to_path_buf(),
                        force: false,
                        yes: false,
                    }),
                )
                .expect("unused device budget/search blocked historical export");
                assert_eq!(
                    fs::read(destination.path().join("note.md")).unwrap(),
                    original
                );
                response
            }
            _ => {
                let response = crate::execute(
                    &context(root.path()),
                    CommandRequest::Restore(crate::commands::RestoreArgs {
                        source: ancestor,
                        paths: vec![PathBuf::from("note.md")],
                        delete_missing: Vec::new(),
                        expected_head: Some(current),
                        preview: false,
                        message: Some("local restore without unrelated settings".into()),
                    }),
                )
                .expect("unused device budget/search blocked managed restore");
                assert_eq!(fs::read(root.path().join("note.md")).unwrap(), original);
                response
            }
        };
        assert!(matches!(
            (operation, output.body),
            ("init", ResponseBody::Init(_))
                | ("export", ResponseBody::Export(_))
                | ("restore", ResponseBody::Restore(_))
        ));
        remove_fixture_file(&path, bytes);
    }
}

#[test]
fn init_ignores_unconsumed_device_budget_and_search_settings() {
    if !isolated(
        "tests::operation_preconditions_tests::init_ignores_unconsumed_device_budget_and_search_settings",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    exercise_unconsumed_device_config("init");
}

#[test]
fn export_ignores_unconsumed_device_budget_and_search_settings() {
    if !isolated(
        "tests::operation_preconditions_tests::export_ignores_unconsumed_device_budget_and_search_settings",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    exercise_unconsumed_device_config("export");
}

#[test]
fn restore_ignores_unconsumed_device_budget_and_search_settings() {
    if !isolated(
        "tests::operation_preconditions_tests::restore_ignores_unconsumed_device_budget_and_search_settings",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    exercise_unconsumed_device_config("restore");
}

#[test]
fn consumed_search_and_index_device_policy_stays_mandatory() {
    if !isolated(
        "tests::operation_preconditions_tests::consumed_search_and_index_device_policy_stays_mandatory",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    initialize_ledger();
    let root = fixture();
    let config = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()).join("kio");
    if !config.exists() {
        create_private_test_directory(&config);
    }
    let path = config.join("config.toml");
    private_bytes(&path, b"[search]\ndefault_mode = 'invalid-consumed-mode'\n");
    let error = crate::execute(&context(root.path()), text_search()).unwrap_err();
    assert_eq!(error.error_code(), "KIO-E-CONFIG-SCHEMA-001");
    for invalid in [
        "[adapter.policy]\nmax_input_bytes = 0\n",
        "[adapter]\nlane = 'invalid-consumed-lane'\n",
        "[markdownize.incremental]\nthreshold = 2.0\n",
    ] {
        replace_private_bytes(&path, invalid.as_bytes());
        let error = crate::execute(&context(root.path()), index(false)).unwrap_err();
        assert_eq!(error.error_code(), "KIO-E-CONFIG-SCHEMA-001", "{invalid}");
    }
}

#[test]
fn invalid_consumed_budget_never_dispatches_authorized_paid_query() {
    if !isolated(
        "tests::operation_preconditions_tests::invalid_consumed_budget_never_dispatches_authorized_paid_query",
    ) {
        return;
    }
    let _lock = kio_core::test_control::test_env_lock()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let device = PrivateTempDir::new();
    let _data = TestEnvGuard::set("XDG_DATA_HOME", device.path());
    initialize_ledger();
    let root = fixture();
    let _local = TestEnvGuard::remove("KIO_EVAL_DETERMINISTIC_EMBED");
    let _mock = TestEnvGuard::set("KIO_TEST_GEMINI_EMBED", "mock");
    let trace = root.path().join("invalid-budget-paid-query-trace");
    let _trace = TestEnvGuard::set("KIO_TEST_QUERY_EMBED_TRACE", &trace);
    let ctx = context(root.path());
    crate::execute(
        &ctx,
        CommandRequest::Adapter(crate::commands::AdapterArgs {
            command: Some(crate::commands::AdapterCommand::Approve(
                crate::commands::ApproveArgs {
                    target: crate::commands::AdapterTarget::Tool("gemini_embedding_2".into()),
                    preview: false,
                    yes: true,
                    resume: None,
                    send_secrets: false,
                },
            )),
        }),
    )
    .unwrap();
    crate::execute(&ctx, index(true)).unwrap();
    let mut positive = super::search_request(Some(SearchMode::Vector));
    positive.query = Some("valid budget paid query positive control".into());
    positive.online = true;
    crate::execute(&ctx, CommandRequest::Search(positive)).unwrap();
    assert!(
        trace.is_file(),
        "fixture must admit selected paid query before corrupting cost policy"
    );
    fs::remove_file(&trace).unwrap();
    let config =
        PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap()).join("kio/config.toml");
    if !config.parent().unwrap().exists() {
        create_private_test_directory(config.parent().unwrap());
    }
    for (case, invalid) in [
        "[budget]\nmonthly_usd_capp = 1\n",
        "budget = 'bad'\n",
        "[budget]\nhard_stop = 'bad'\n",
        "[budget]\nmonthly_usd_cap = 'bad'\n",
        "[budget]\nwarn_at_percent = 'bad'\n",
        "[budget.per_adapter]\nembedding = 'bad'\n",
        "[budegt]\nmonthly_usd_cap = 1\n",
    ]
    .iter()
    .enumerate()
    {
        if config.exists() {
            replace_private_bytes(&config, invalid.as_bytes());
        } else {
            private_bytes(&config, invalid.as_bytes());
        }
        let before = billing_tree(device.path());
        let mut query = super::search_request(Some(SearchMode::Vector));
        query.query = Some(format!("invalid selected cost policy fresh query {case}"));
        query.online = true;
        let error = crate::execute(&ctx, CommandRequest::Search(query)).unwrap_err();
        assert_eq!(
            error.error_code(),
            "KIO-E-CONFIG-SCHEMA-001",
            "{invalid}: {error}"
        );
        assert!(
            !trace.exists(),
            "invalid selected cost policy dispatched: {invalid}"
        );
        assert_eq!(
            billing_tree(device.path()),
            before,
            "invalid cost policy mutated billing authority"
        );
    }
}
