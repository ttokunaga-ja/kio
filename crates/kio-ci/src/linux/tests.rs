use super::*;
pub(super) const UNIT: &str = "kio-confinement-preflight-0123456789abcdef0123456789abcdef.scope";
#[derive(Default)]
struct Fake {
    calls: Vec<Request>,
    environment: String,
    fail: &'static str,
    polls: usize,
    verified: bool,
}
impl Fake {
    fn cgroup() -> String {
        format!("/user.slice/user-1001.slice/user@1001.service/app.slice/{UNIT}")
    }
}
impl Host for Fake {
    fn uid(&self) -> u32 {
        1001
    }
    fn canonical(&mut self, path: &Path) -> Result<PathBuf> {
        Ok(path.into())
    }
    fn metadata(&mut self, path: &Path) -> Result<Metadata> {
        Ok(Metadata {
            uid: 1001,
            mode: if self.fail == "runtime" { 0o755 } else { 0o700 },
            directory: path.file_name().unwrap() != "bus",
            socket: path.file_name().unwrap() == "bus",
        })
    }
    fn read(&mut self, path: &Path) -> Result<String> {
        Ok(match path.file_name().unwrap().to_str().unwrap() {
            "cgroup" => {
                if self.fail == "membership" {
                    "0::/wrong/other.scope\n".into()
                } else {
                    format!("0::{}\n", Self::cgroup())
                }
            }
            "cgroup.controllers" | "cgroup.subtree_control" => "cpu memory pids".into(),
            "memory.max" => if self.fail == "resource" {
                "0"
            } else {
                "67108864"
            }
            .into(),
            "memory.swap.max" => "0".into(),
            "pids.max" => "32".into(),
            "memory.oom.group" => "1".into(),
            "cpu.max" => "100000 100000".into(),
            "cgroup.events" => if self.fail == "populated" {
                "populated 1\n"
            } else {
                "populated 0\n"
            }
            .into(),
            _ => return Err("unexpected read".into()),
        })
    }
    fn run(&mut self, request: Request) -> Result<Output> {
        self.calls.push(request.clone());
        let mut output = Output {
            status: Status::Success,
            stdout: String::new(),
            stderr: String::new(),
        };
        match request.program {
            "stat" => output.stdout = "cgroup2fs\n".into(),
            "busctl" if self.fail == "ping" => output.status = Status::Exit(1),
            "busctl" => assert_eq!(request.args.last().unwrap(), "Ping"),
            "systemd-run" => {
                if self.fail == "timeout" {
                    output.status = Status::Timeout;
                } else {
                    match probe(self, 1001, UNIT) {
                        Ok(value) => {
                            self.verified = true;
                            output.stdout = value;
                        }
                        Err(error) => {
                            output.status = Status::Exit(1);
                            output.stderr = error;
                        }
                    }
                }
            }
            "systemctl" if request.args[1] == "stop" => {
                if ["cleanup", "collected", "contradictory_missing"].contains(&self.fail) {
                    output.status = Status::Exit(1);
                }
                if self.fail == "cleanup_timeout" {
                    output.status = Status::Timeout;
                }
                if self.fail == "stop_transport" {
                    return Err("stop transport failed".into());
                }
            }
            "systemctl" if request.args[1] == "show" => {
                if request.args.last().unwrap().contains("LoadState") {
                    if self.fail == "state_status" {
                        output.status = Status::Exit(1);
                    }
                    if self.fail == "state_transport" {
                        return Err("state transport failed".into());
                    }
                    output.stdout = format!(
                        "{}ActiveState=inactive\nControlGroup={}\n",
                        if self.fail == "state_missing" {
                            ""
                        } else {
                            if ["collected", "contradictory_missing"].contains(&self.fail) {
                                "LoadState=not-found\n"
                            } else {
                                "LoadState=loaded\n"
                            }
                        },
                        if self.fail == "collected" {
                            String::new()
                        } else {
                            Self::cgroup()
                        }
                    );
                    if ["missing_active", "missing_failed"].contains(&self.fail) {
                        output.stdout = format!(
                            "LoadState=not-found\nActiveState={}\nControlGroup=\n",
                            if self.fail == "missing_active" {
                                "active"
                            } else {
                                "failed"
                            }
                        );
                    }
                } else {
                    output.stdout = format!(
                        "TimeoutStopUSec=5s\nKillSignal={}\nKillMode=control-group\nRuntimeMaxUSec=15s\n",
                        if self.fail == "termination" {
                            "15"
                        } else {
                            "9"
                        }
                    );
                }
            }
            "systemctl" => assert_eq!(request.args[1], "show-environment"),
            _ => panic!("unexpected command"),
        }
        Ok(output)
    }
    fn env_read(&mut self) -> Result<String> {
        Ok(self.environment.clone())
    }
    fn env_append(&mut self, before: &str, suffix: &str) -> Result<()> {
        assert!(self.verified);
        assert_eq!(before, self.environment);
        assert!(
            self.calls
                .last()
                .unwrap()
                .args
                .last()
                .unwrap()
                .contains("LoadState")
        );
        self.environment.push_str(suffix);
        Ok(())
    }
    fn pause(&mut self) {
        self.polls += 1;
    }
}
fn run(fake: &mut Fake) -> Result<()> {
    preflight(fake, UNIT, "/fixed/helper")
}
fn cleaned(fake: &Fake) {
    assert_eq!(
        fake.calls
            .iter()
            .filter(|c| c.program == "systemd-run")
            .count(),
        1
    );
    assert_eq!(
        fake.calls
            .iter()
            .filter(|c| c.args.get(1).is_some_and(|a| a == "stop"))
            .count(),
        1
    );
    assert!(
        fake.calls
            .last()
            .unwrap()
            .args
            .last()
            .unwrap()
            .contains("LoadState")
    );
}
#[test]
fn success_uses_peer_ping_and_exact_resource_scope_before_export() {
    let mut fake = Fake::default();
    run(&mut fake).unwrap();
    cleaned(&fake);
    let scope = fake
        .calls
        .iter()
        .find(|r| r.program == "systemd-run")
        .unwrap();
    assert_eq!(
        scope.args[..5],
        [
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            &format!("--unit={UNIT}")
        ]
    );
    assert_eq!(
        scope.args[5..14],
        PROPERTIES.map(|p| format!("--property={p}"))
    );
    assert_eq!(scope.timeout, Duration::from_secs(20));
    assert_eq!(
        fake.environment,
        "XDG_RUNTIME_DIR=/run/user/1001\nDBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1001/bus\n"
    );
}
#[test]
fn ping_failure_refuses_scope_and_export() {
    let mut fake = Fake {
        fail: "ping",
        ..Default::default()
    };
    assert!(run(&mut fake).is_err());
    assert!(!fake.calls.iter().any(|r| r.program == "systemd-run"));
    assert!(fake.environment.is_empty());
}
#[test]
fn child_limit_failure_cleans_and_refuses_export() {
    let mut fake = Fake {
        fail: "resource",
        ..Default::default()
    };
    assert!(run(&mut fake).unwrap_err().contains("memory.max"));
    cleaned(&fake);
    assert!(fake.environment.is_empty());
}
#[test]
fn timeout_without_child_output_still_checks_actual_cgroup() {
    let mut fake = Fake {
        fail: "timeout",
        ..Default::default()
    };
    assert!(run(&mut fake).unwrap_err().contains("Timeout"));
    cleaned(&fake);
    assert!(fake.environment.is_empty());
}
#[test]
fn cleanup_failure_refuses_export() {
    let mut fake = Fake {
        fail: "cleanup",
        ..Default::default()
    };
    assert!(run(&mut fake).unwrap_err().contains("clean up"));
    cleaned(&fake);
    assert!(fake.environment.is_empty());
}
#[test]
fn runtime_permissions_refuse_before_commands() {
    let mut fake = Fake {
        fail: "runtime",
        ..Default::default()
    };
    assert!(run(&mut fake).unwrap_err().contains("0700"));
    assert!(fake.calls.is_empty());
    assert!(fake.environment.is_empty());
}
#[test]
fn conflicting_environment_preserved() {
    let mut fake = Fake {
        environment: "XDG_RUNTIME_DIR=/wrong\n".into(),
        ..Default::default()
    };
    assert!(run(&mut fake).unwrap_err().contains("conflicting"));
    cleaned(&fake);
    assert_eq!(fake.environment, "XDG_RUNTIME_DIR=/wrong\n");
}
#[test]
fn export_idempotent() {
    let mut fake = Fake::default();
    run(&mut fake).unwrap();
    let before = fake.environment.clone();
    run(&mut fake).unwrap();
    assert_eq!(fake.environment, before);
}
#[test]
fn malformed_membership_and_termination_refuse_and_cleanup() {
    for fail in ["membership", "termination"] {
        let mut fake = Fake {
            fail,
            ..Default::default()
        };
        assert!(run(&mut fake).is_err());
        cleaned(&fake);
        assert!(fake.environment.is_empty());
    }
}
#[test]
fn residual_processes_refuse_export_after_bounded_polling() {
    let mut fake = Fake {
        fail: "populated",
        ..Default::default()
    };
    assert!(run(&mut fake).unwrap_err().contains("still has processes"));
    assert_eq!(fake.polls, 50);
    assert!(fake.environment.is_empty());
}
#[test]
fn closed_unit_uid_and_membership_grammar() {
    for unit in ["foreign.scope", "kio-confinement-preflight-../x.scope"] {
        assert!(probe(&mut Fake::default(), 1001, unit).is_err());
    }
    assert!(probe(&mut Fake::default(), 0, UNIT).is_err());
    assert!(probe(&mut Fake::default(), 1002, UNIT).is_err());
    for relative in [
        format!("/user.slice/user-1002.slice/user@1002.service/app.slice/{UNIT}"),
        format!("/user.slice/user-1001.slice/user@1001.service/../{UNIT}"),
        format!("/user.slice/user-1001.slice/user@1001.service/./{UNIT}"),
    ] {
        assert!(membership_path(&relative, 1001, UNIT).is_err());
    }
}

#[test]
fn malformed_cleanup_loadstate_refuses_export() {
    let mut fake = Fake {
        fail: "state_missing",
        ..Default::default()
    };
    assert!(run(&mut fake).is_err());
    assert!(fake.environment.is_empty());
}

#[test]
fn cleanup_timeout_transport_and_status_failures_refuse_export() {
    for fail in [
        "cleanup_timeout",
        "stop_transport",
        "state_status",
        "state_transport",
    ] {
        let mut fake = Fake {
            fail,
            ..Default::default()
        };
        assert!(run(&mut fake).is_err());
        cleaned(&fake);
        assert!(fake.environment.is_empty());
    }
}
#[test]
fn already_collected_scope_is_valid_cleanup() {
    let mut fake = Fake {
        fail: "collected",
        ..Default::default()
    };
    run(&mut fake).unwrap();
    cleaned(&fake);
    assert!(!fake.environment.is_empty());
}

#[test]
fn contradictory_not_found_scope_refuses_export() {
    let mut fake = Fake {
        fail: "contradictory_missing",
        ..Default::default()
    };
    assert!(run(&mut fake).is_err());
    assert!(fake.environment.is_empty());
}
#[test]
fn other_multiline_payload_does_not_impersonate_runtime_export() {
    let mut fake = Fake {
        environment: "OTHER<<EOF\nXDG_RUNTIME_DIR=/run/user/1001\nEOF\n".into(),
        ..Default::default()
    };
    run(&mut fake).unwrap();
    assert!(fake.environment.ends_with(
        "XDG_RUNTIME_DIR=/run/user/1001\nDBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1001/bus\n"
    ));
}

#[test]
fn missing_scope_active_or_failed_is_contradictory() {
    for fail in ["missing_active", "missing_failed"] {
        let mut fake = Fake {
            fail,
            ..Default::default()
        };
        assert!(run(&mut fake).unwrap_err().contains("contradictory"));
        assert!(fake.environment.is_empty());
    }
}
#[test]
fn multiline_target_duplicate_and_unterminated_preserve_environment() {
    for initial in [
        "XDG_RUNTIME_DIR<<EOF\n/run/user/1001\nEOF\n",
        "XDG_RUNTIME_DIR=/run/user/1001\nXDG_RUNTIME_DIR=/run/user/1001\n",
        "OTHER<<EOF\nXDG_RUNTIME_DIR=/run/user/1001\n",
    ] {
        let mut fake = Fake {
            environment: initial.into(),
            ..Default::default()
        };
        assert!(run(&mut fake).is_err());
        cleaned(&fake);
        assert_eq!(fake.environment, initial);
    }
}
