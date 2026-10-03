//! Native watcher to application/CLI integration, with synthetic documents and
//! isolated device state. No provider credentials or user configuration inherit.
mod support;

use support::canonical_tempdir;

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = canonical_tempdir();
        let root = temp.path().join("root");
        let home = temp.path().join("home");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(home.join("tmp")).unwrap();
        let fixture = Self {
            _temp: temp,
            root,
            home,
        };
        fixture.ok(&["init"]);
        fixture
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("kio"));
        command.env_clear();
        if let Some(value) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", value);
        }
        command
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_DATA_HOME", self.home.join("data"))
            .env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("XDG_CACHE_HOME", self.home.join("cache"))
            .env("TMPDIR", self.home.join("tmp"))
            .env("TEMP", self.home.join("tmp"))
            .env("TMP", self.home.join("tmp"))
            .env("KIO_TEST_MARKDOWNIZE_ADAPTER", "deterministic")
            .arg("--json")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
    fn ok(&self, args: &[&str]) -> Value {
        let output = self.command(args).output().unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn status(&self) -> Value {
        self.ok(&["watch", "status"])
    }
    fn start(&self, interval: &str) -> Guard {
        let mut guard = Guard::new(
            self.command(&["watch", "run", "--reconcile-interval-seconds", interval])
                .spawn()
                .unwrap(),
        );
        std::thread::sleep(Duration::from_millis(500));
        if guard.child.as_mut().unwrap().try_wait().unwrap().is_some() {
            let output = guard.output();
            panic!(
                "watch exited on startup: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        guard
    }
    fn wait(&self, condition: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            let status = self.status();
            if condition(&status) {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "watch condition timed out: {status}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    /// Require published idle observations, rather than silence while a long
    /// reconciliation leaves the previous idle snapshot visible to readers.
    /// None permits finite enrollment catch-up; Some pins the final assertion.
    fn observed_idle(&self, stable_success: Option<u64>) -> Value {
        let started = Instant::now();
        let deadline = started + Duration::from_secs(25);
        let mut candidate: Option<(u64, u64, u64)> = None;
        let mut recent = VecDeque::new();
        loop {
            let status = self.status();
            let observation = &status["last_observation"];
            let success = observation["last_success_ms"].as_u64();
            let observed = observation["observed_at_ms"].as_u64();
            let idle = status["status"] == "running"
                && observation["backlog"] == 0
                && observation["degraded"] == false;
            recent.push_back(serde_json::json!({
                "elapsed_ms": started.elapsed().as_millis(),
                "status": status["status"],
                "success": success,
                "observed": observed,
                "backlog": observation["backlog"],
                "degraded": observation["degraded"],
                "failure": observation["last_failure"],
            }));
            if recent.len() > 40 {
                recent.pop_front();
            }
            assert!(
                Instant::now() < deadline,
                "watch never published stable idle observations; recent samples: {recent:?}"
            );
            if let Some(expected) = stable_success {
                assert!(
                    idle && success == Some(expected),
                    "watch resumed work after observed idle; recent samples: {recent:?}"
                );
            }
            match (idle, success, observed) {
                (true, Some(success), Some(observed)) => {
                    let (start, previous) = match candidate {
                        Some((previous_success, start, previous))
                            if previous_success == success && observed >= previous =>
                        {
                            (start, previous)
                        }
                        _ => (observed, observed),
                    };
                    candidate = Some((success, start, observed));
                    if observed > previous && observed.saturating_sub(start) >= 3_000 {
                        return status;
                    }
                }
                _ => candidate = None,
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}
struct Guard {
    child: Option<Child>,
    stdout: Option<JoinHandle<io::Result<Vec<u8>>>>,
    stderr: Option<JoinHandle<io::Result<Vec<u8>>>>,
}
impl Guard {
    fn new(mut child: Child) -> Self {
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let mut guard = Self {
            child: Some(child),
            stdout: None,
            stderr: None,
        };
        // Drain both pipes while the watcher runs, so a full pipe cannot block
        // the watcher before it exits.
        guard.stdout = Some(std::thread::spawn(move || read_output(stdout)));
        guard.stderr = Some(std::thread::spawn(move || read_output(stderr)));
        guard
    }
    fn output(&mut self) -> Output {
        let status = self.child.as_mut().unwrap().wait().unwrap();
        self.child = None;
        Output {
            status,
            stdout: self.stdout.take().unwrap().join().unwrap().unwrap(),
            stderr: self.stderr.take().unwrap().join().unwrap().unwrap(),
        }
    }
    fn stopped(mut self) {
        let deadline = Instant::now() + Duration::from_secs(25);
        let child = self.child.as_mut().unwrap();
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "watch stop timed out");
            std::thread::sleep(Duration::from_millis(100));
        }
        let output = self.output();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout)
            .expect("watch stop must return complete JSON output");
        assert_eq!(result["status"], "stopped");
    }
    fn crash(mut self) {
        self.child.as_mut().unwrap().kill().unwrap();
        let _ = self.output();
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(stdout) = self.stdout.take() {
            let _ = stdout.join();
        }
        if let Some(stderr) = self.stderr.take() {
            let _ = stderr.join();
        }
    }
}

fn read_output(mut pipe: impl Read) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    pipe.read_to_end(&mut output)?;
    Ok(output)
}

#[test]
fn malformed_adapter_settings_cannot_prevent_watcher_status_or_stop() {
    let fixture = Fixture::new();
    let process = fixture.start("300");
    fixture.wait(|s| s["last_observation"]["last_success_ms"].is_number());
    let config = fixture.home.join("config/kio");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("tools.toml"), "[invalid adapter configuration").unwrap();

    assert!(
        !fixture
            .command(&["index", "--offline"])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(fixture.status()["status"], "running");
    assert_eq!(fixture.ok(&["watch", "stop"])["status"], "stop_requested");
    process.stopped();
    assert_eq!(fixture.status()["status"], "stopped");
}

#[test]
fn native_watch_enrolls_empty_descendants_and_stops_without_self_event_loop() {
    let fixture = Fixture::new();
    assert_eq!(fixture.status()["status"], "stopped");
    assert!(
        !fixture.home.join("data/kio/watch").exists(),
        "status must not create runtime state"
    );
    fs::write(fixture.root.join(".kioignore"), "ignored/\n").unwrap();
    let process = fixture.start("300");
    let startup = fixture.wait(|s| s["last_observation"]["last_success_ms"].is_number());
    let startup_success = startup["last_observation"]["last_success_ms"].as_u64();
    let startup_observed = startup["last_observation"]["observed_at_ms"]
        .as_u64()
        .unwrap();
    let duplicate = fixture.command(&["watch", "run"]).output().unwrap();
    assert!(
        !duplicate.status.success(),
        "a root may have only one watcher"
    );
    fs::create_dir_all(fixture.root.join("empty/deep")).unwrap();
    fs::create_dir_all(fixture.root.join("ignored/deep")).unwrap();
    fixture.wait(|s| {
        fixture
            .root
            .join("empty/deep/.kio/management.json")
            .exists()
            && s["last_observation"]["last_success_ms"].as_u64().is_some()
            && s["last_observation"]["last_success_ms"].as_u64() != startup_success
            && s["last_observation"]["observed_at_ms"]
                .as_u64()
                .is_some_and(|observed| observed > startup_observed)
            && s["last_observation"]["backlog"] == 0
            && s["last_observation"]["degraded"] == false
    });
    assert!(!fixture.root.join("ignored/.kio").exists());
    // Enrollment controls may need a finite catch-up pass. Require fresh idle
    // publications before asserting that generated writes stay quiescent.
    let settled = fixture.observed_idle(None);
    let settled_success = settled["last_observation"]["last_success_ms"]
        .as_u64()
        .unwrap();
    let stable = fixture.observed_idle(Some(settled_success));
    assert_eq!(
        settled["last_observation"]["last_success_ms"],
        stable["last_observation"]["last_success_ms"]
    );
    assert_eq!(fixture.ok(&["watch", "stop"])["status"], "stop_requested");
    process.stopped();
    assert_eq!(fixture.status()["status"], "stopped");
    let scope: Value =
        serde_json::from_slice(&fs::read(fixture.root.join("empty/.kio/scope.json")).unwrap())
            .unwrap();
    assert!(
        scope["approvals"]
            .as_array()
            .is_none_or(|rows| rows.is_empty())
    );
}

#[test]
fn restart_reconciles_missed_changes_and_ignores_old_stop_request() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("notes.md"), "first\n").unwrap();
    let first = fixture.start("300");
    let state = fixture.wait(|s| s["last_observation"]["last_success_ms"].is_number());
    let old_instance = state["last_observation"]["instance"].clone();
    fixture.ok(&["watch", "stop"]);
    first.stopped();
    fs::write(fixture.root.join("notes.md"), "second\n").unwrap();
    fs::create_dir_all(fixture.root.join("while-stopped/empty")).unwrap();
    let second = fixture.start("300");
    fixture.wait(|s| {
        s["status"] == "running"
            && s["last_observation"]["instance"] != old_instance
            && s["last_observation"]["last_success_ms"].is_number()
            && fixture
                .root
                .join("while-stopped/empty/.kio/management.json")
                .exists()
    });
    second.crash();
    assert_eq!(
        fixture.status()["status"],
        "stopped",
        "persisted running state must not survive an OS lock owner exit"
    );
    let manual = fixture.ok(&["index", "--offline"]);
    assert_eq!(
        manual["status"], "noop",
        "manual index must converge with the watcher"
    );
    fs::create_dir(fixture.root.join("after-crash")).unwrap();
    let third = fixture.start("300");
    fixture.wait(|s| {
        s["status"] == "running"
            && fixture
                .root
                .join("after-crash/.kio/management.json")
                .exists()
    });
    fixture.ok(&["watch", "stop"]);
    third.stopped();
}

#[test]
fn watcher_rejects_child_authority_and_unregistered_root() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.root.join("child")).unwrap();
    fixture.ok(&["index", "--offline"]);
    for root in [fixture.root.join("child"), fixture.home.clone()] {
        let output = fixture
            .command(&["watch", "--root", root.to_str().unwrap(), "run"])
            .output()
            .unwrap();
        assert!(!output.status.success(), "invalid root must be rejected");
    }
}
