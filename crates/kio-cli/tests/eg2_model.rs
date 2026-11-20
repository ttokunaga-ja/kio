#![cfg(debug_assertions)]

mod support;

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const BYTES: &[u8] = b"test model";
const HASH: &str = "e6a8036e9cb9f8c9f52ffd985c4d6f0498d94e6919f4c67b6423da0176e549fa";
const REVISION: &str = "e301f74d5551b0c2641bd5cb4652a76239d5c5f8";

struct Fixture {
    temp: tempfile::TempDir,
    data: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = support::canonical_tempdir();
        let data = support::create_private_child_dir(temp.path(), "data");
        Self { temp, data }
    }

    fn command(&self, args: &[&str]) -> Command {
        let binary = env!("CARGO_BIN_EXE_kio");
        // Give the Linux sandbox a retained device root even when the
        // sandbox maps ancestors of the fixture to a different UID.
        #[cfg(target_os = "linux")]
        let (mut command, data) = (
            {
                let mut command = Command::new("bash");
                command.args(["-c", r#"exec 9<"$1"; shift; exec "$@""#, "kio-test"]);
                command.arg(&self.data).arg(binary);
                command
            },
            Path::new("/dev/fd/9"),
        );
        #[cfg(not(target_os = "linux"))]
        let (mut command, data) = (Command::new(binary), self.data.as_path());
        command
            .envs(support::home_env(self.temp.path()))
            .env("XDG_DATA_HOME", data)
            .env("KIO_TEST_MODEL_SOURCE", "import")
            .current_dir(self.temp.path())
            .stdin(Stdio::null())
            .args(args);
        command
    }

    fn artifact(&self) -> PathBuf {
        self.data
            .join("kio/models/embeddinggemma-2")
            .join(REVISION)
            .join(HASH)
            .join("embeddinggemma-2-text-vision-440m.litertlm")
    }

    fn source(&self, bytes: &[u8]) -> PathBuf {
        let path = self.temp.path().join("source.model");
        fs::write(&path, bytes).unwrap();
        path
    }

    fn import(&self, source: &Path) -> Command {
        let mut command = self.command(&[
            "--json",
            "model",
            "install",
            "embeddinggemma-2",
            "--yes",
            "--offline",
            "--from",
        ]);
        command.arg(source);
        command
    }

    fn status(&self) -> Value {
        json_success(
            self.command(&["model", "status", "--json"])
                .output()
                .unwrap(),
        )
    }
}

fn json_success(output: Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

fn failed(output: Output, diagnostic: &str) {
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(diagnostic),
        "{output:?}"
    );
}

#[test]
fn model_cli_recognizes_the_documented_commands() {
    let output = Command::new(env!("CARGO_BIN_EXE_kio"))
        .args(["model", "install", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("--from"));
}

#[test]
fn offline_import_and_read_only_status_are_device_local() {
    let fixture = Fixture::new();
    let status = fixture.status();
    assert_eq!(status["readiness"], "missing");
    assert!(status["reason"].as_str().unwrap().contains("install"));
    assert_eq!(status["model_revision"], REVISION);
    assert_eq!(status["model_hash"], HASH);
    assert!(!fixture.data.join("kio").exists());

    let source = fixture.source(BYTES);
    let output = fixture.import(&source).output().unwrap();
    let confirmation = String::from_utf8_lossy(&output.stderr);
    for disclosure in [
        "huggingface.co/litert-community",
        REVISION,
        HASH,
        "10 bytes",
        "models",
        "Apache-2.0",
        "deployments must adhere to the Gemma Prohibited Use Policy",
        "https://ai.google.dev/gemma/prohibited_use_policy",
    ] {
        assert!(confirmation.contains(disclosure), "{confirmation}");
    }
    assert_eq!(json_success(output)["readiness"], "ready");
    assert_eq!(fs::read(&source).unwrap(), BYTES);
    let artifact = fixture.artifact();
    assert_eq!(fs::read(&artifact).unwrap(), BYTES);
    assert_eq!(fixture.status()["readiness"], "ready");

    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&artifact).unwrap().ino()
    };
    assert_eq!(
        json_success(fixture.import(&artifact).output().unwrap())["readiness"],
        "ready"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(&artifact).unwrap().ino(), inode);
    }

    let production = json_success(
        fixture
            .command(&["model", "status", "--json"])
            .env_remove("KIO_TEST_MODEL_SOURCE")
            .output()
            .unwrap(),
    );
    assert_eq!(production["readiness"], "missing");
    assert_eq!(
        production["model_hash"],
        "92dcbea108899e5d6e30d919b0744f90d9967e80c67a4ab5503ac16d54f62eb0"
    );
    assert!(!fixture.temp.path().join(".kio").exists());
    assert!(!fixture.data.join("kio/cost-ledger.sqlite").exists());

    fs::write(fixture.artifact(), b"bad  model").unwrap();
    assert_eq!(fixture.status()["readiness"], "hash-mismatch");
    assert_eq!(fs::read(fixture.artifact()).unwrap(), b"bad  model");
    json_success(fixture.import(&source).output().unwrap());
    assert_eq!(fixture.status()["readiness"], "ready");
}

#[test]
fn rejected_imports_preserve_existing_model() {
    let fixture = Fixture::new();
    let source = fixture.source(BYTES);
    failed(
        fixture
            .command(&["model", "install", "embeddinggemma-2", "--from"])
            .arg(&source)
            .output()
            .unwrap(),
        "--yes",
    );
    assert_eq!(fixture.status()["readiness"], "missing");
    json_success(fixture.import(&source).output().unwrap());

    for (bytes, error) in [
        (b"short".as_slice(), "size"),
        (b"test model extra".as_slice(), "size"),
        (b"bad  model".as_slice(), "SHA-256"),
    ] {
        fixture.source(bytes);
        failed(fixture.import(&source).output().unwrap(), error);
        assert_eq!(fs::read(&source).unwrap(), bytes);
        assert_eq!(fs::read(fixture.artifact()).unwrap(), BYTES);
        assert_eq!(fixture.status()["readiness"], "ready");
    }
}

#[test]
fn disk_failure_before_publication_preserves_the_valid_model() {
    let fixture = Fixture::new();
    let source = fixture.source(BYTES);
    json_success(fixture.import(&source).output().unwrap());

    let failure = fixture
        .import(&source)
        .env("KIO_TEST_DURABILITY_POINT", "atomic_write_staged")
        .env("KIO_TEST_DURABILITY_STORAGE_FAULT", "1")
        .output()
        .unwrap();
    failed(failure, "storage failure");
    assert_eq!(fs::read(&source).unwrap(), BYTES);
    assert_eq!(fs::read(fixture.artifact()).unwrap(), BYTES);
    assert_eq!(fixture.status()["readiness"], "ready");
    json_success(fixture.import(&source).output().unwrap());
}

#[test]
fn offline_download_is_rejected_before_accessing_the_source() {
    let fixture = Fixture::new();
    failed(
        fixture
            .command(&["model", "install", "embeddinggemma-2", "--offline", "--yes"])
            .output()
            .unwrap(),
        "offline",
    );
    assert_eq!(fixture.status()["readiness"], "missing");
    assert!(!fixture.artifact().exists());
}

#[test]
fn download_is_explicit_and_rejects_truncated_http() {
    let fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for bytes in [BYTES, b"short"] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8192);
            }
            assert!(request.starts_with(b"GET / "));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n")
                .unwrap();
            stream.write_all(bytes).unwrap();
        }
    });
    let download = || {
        fixture
            .command(&["--json", "model", "install", "embeddinggemma-2", "--yes"])
            .env("KIO_TEST_MODEL_SOURCE", &url)
            .output()
            .unwrap()
    };
    assert_eq!(json_success(download())["readiness"], "ready");
    let truncated = download();
    assert!(!truncated.status.success(), "{truncated:?}");
    assert!(truncated.stdout.is_empty(), "{truncated:?}");
    server.join().unwrap();
    assert_eq!(fs::read(fixture.artifact()).unwrap(), BYTES);
}

#[test]
fn interrupted_install_does_not_publish_and_can_retry() {
    let fixture = Fixture::new();
    let source = fixture.source(BYTES);
    let ready = fixture.temp.path().join("staged");
    for readiness in ["missing", "ready"] {
        let mut child = fixture
            .import(&source)
            .env("KIO_TEST_DURABILITY_POINT", "atomic_write_staged")
            .env("KIO_TEST_DURABILITY_READY", &ready)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "installer exited before staging: {:?}",
                child.wait_with_output().unwrap()
            );
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("installer did not stage in time");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(fixture.status()["readiness"], readiness);
        let model_before_kill = fs::read(fixture.artifact()).ok();
        assert_eq!(
            model_before_kill.as_deref(),
            (readiness == "ready").then_some(BYTES)
        );

        child.kill().unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(fs::read(fixture.artifact()).ok(), model_before_kill);
        fs::remove_file(&ready).unwrap();
        assert_eq!(
            json_success(fixture.import(&source).output().unwrap())["readiness"],
            "ready"
        );
    }
}
