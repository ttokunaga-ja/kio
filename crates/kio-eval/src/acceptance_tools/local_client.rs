//! Private, fixed-grammar client for the two-hop local GPU acceptance lane.
//!
//! The client never accepts a remote shell fragment.  It writes its transient
//! configuration, capability, observations, and diagnostics only under the
//! verified route root that owns the pinned SSH key.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use clap::{Args as ClapArgs, ValueEnum};
use kio_core::{
    private_fs::{read_private_file, verify_private_directory},
    store_dir::{Publication, StoreDirectory},
};
use kio_process::{BoundedProcessOptions, BoundedStdin, SupervisedChild, run_bounded_command};
use serde_json::Value;
use sha2::{Digest, Sha256};

const PUBLIC_MAX: usize = 64 * 1024;
const PRIVATE_MAX: u64 = 1024 * 1024;
const CAPABILITY_LEN: usize = 64;
const INIT_TIMEOUT: Duration = Duration::from_secs(450);
const IDENTITY_TIMEOUT: Duration = Duration::from_secs(90);
const PHASE_TIMEOUT: Duration = Duration::from_secs(450);
const EVAL_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, ClapArgs)]
pub struct RouteMaterializeArgs {
    /// New private directory; an existing path is never adopted or repaired.
    #[arg(long)]
    pub root: PathBuf,
}

/// Secrets are read only from this step's environment, never command arguments.
pub fn materialize_route(args: RouteMaterializeArgs) -> Result<(), String> {
    let key = std::env::var("KIO_LAB_KEY").map_err(|_| "dedicated route key is unavailable")?;
    let hosts = std::env::var("KIO_LAB_HOSTS").map_err(|_| "pinned route hosts are unavailable")?;
    write_route_material(&args.root, &key, &hosts)
}

fn write_route_material(root: &Path, key: &str, hosts: &str) -> Result<(), String> {
    for material in [key, hosts] {
        if material.trim().is_empty() || material.len() > PUBLIC_MAX || material.contains('\0') {
            return Err("route material is absent or outside its bound".into());
        }
    }
    let absolute = std::path::absolute(root).map_err(|_| "cannot resolve route root")?;
    let parent = absolute.parent().ok_or("route root needs a parent")?;
    let leaf = absolute.file_name().ok_or("route root needs a name")?;
    let parent = kio_core::private_fs::verify_private_creation_parent(parent)
        .map_err(|_| "route root parent is not controlled")?;
    let handle = parent
        .create_directory(Path::new(leaf))
        .map_err(|_| "route root must be a new private directory")?;
    let directory =
        StoreDirectory::from_retained(handle, absolute).map_err(|_| "cannot retain route root")?;
    for (name, bytes) in [("id", key.as_bytes()), ("known_hosts", hosts.as_bytes())] {
        directory
            .write_atomic(Path::new(name), bytes, Publication::CreateOnly)
            .map_err(|_| "cannot publish private route material")?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Mode {
    Prepare,
    Run,
}

/// Arguments retained from `actions-client.py`.  The route root is derived
/// from `--identity-file`; outputs must remain in that owner-private root.
#[derive(Debug, ClapArgs)]
pub struct Args {
    #[arg(value_enum)]
    pub mode: Mode,
    #[arg(long)]
    pub candidate: String,
    #[arg(long)]
    pub run_id: String,
    #[arg(long)]
    pub attempt: String,
    #[arg(long, default_value = "windows")]
    pub os: String,
    #[arg(long)]
    pub jump_host: String,
    #[arg(long, default_value = "kio-ci")]
    pub jump_user: String,
    #[arg(long)]
    pub known_hosts: PathBuf,
    #[arg(long)]
    pub identity_file: PathBuf,
    #[arg(long)]
    pub identity_out: PathBuf,
    #[arg(long)]
    pub capability_file: PathBuf,
    #[arg(long)]
    pub expected: Option<PathBuf>,
    #[arg(long)]
    pub eval_bin: Option<PathBuf>,
    #[arg(long)]
    pub candidate_bin: Option<PathBuf>,
    #[arg(long)]
    pub fixture: Option<PathBuf>,
    #[arg(long)]
    pub work_dir: Option<PathBuf>,
    #[arg(long)]
    pub receipt: Option<PathBuf>,
}

pub fn run(args: Args) -> Result<(), String> {
    let mut client = Client::new(&args)?;
    match args.mode {
        Mode::Prepare => {
            client.capability = Some(new_capability()?);
            // This create-only publication is deliberately before the first
            // network attempt: a lost init response still leaves the exact
            // client-generated capability available for authenticated finish.
            client.write_capability(&args.capability_file)?;
            let prepared = client.initialize().and_then(|()| {
                let identity_hash = client.write_identity(&args.identity_out)?;
                serde_jcs::to_string(&serde_json::json!({
                    "service_identity_sha256": identity_hash,
                }))
                .map_err(|error| format!("cannot encode prepare response: {error}"))
            });
            let output =
                finish_after_prepare_error(prepared, || client.invoke(Verb::Finish).map(|_| ()))?;
            println!("{output}");
        }
        Mode::Run => {
            client.load_capability(&args.capability_file)?;
            // The expected-case step may fail after a successful prepare.
            // Its missing artifact must not bypass the always-run cleanup.
            let inputs = finish_after_prepare_error(RunInputs::from_args(&args), || {
                client.invoke(Verb::Finish).map(|_| ())
            })?;
            client.run_phases(&args.identity_out, inputs)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum Verb {
    Init,
    Identity,
    StartOcr,
    StopOcr,
    StartEmbedding,
    StopEmbedding,
    Finish,
}

impl Verb {
    const fn name(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Identity => "identity",
            Self::StartOcr => "start-ocr",
            Self::StopOcr => "stop-ocr",
            Self::StartEmbedding => "start-embedding",
            Self::StopEmbedding => "stop-embedding",
            Self::Finish => "finish",
        }
    }

    const fn timeout(self) -> Duration {
        match self {
            Self::Identity => IDENTITY_TIMEOUT,
            Self::Init | Self::Finish => INIT_TIMEOUT,
            Self::StartOcr | Self::StopOcr | Self::StartEmbedding | Self::StopEmbedding => {
                PHASE_TIMEOUT
            }
        }
    }
}

struct Client {
    route: StoreDirectory,
    route_path: PathBuf,
    config_leaf: String,
    candidate: String,
    run_id: String,
    attempt: String,
    os: String,
    capability: Option<String>,
}

impl Client {
    fn new(args: &Args) -> Result<Self, String> {
        validate_args(args)?;
        let route_path = args
            .identity_file
            .parent()
            .ok_or_else(|| "identity file has no route-root parent".to_owned())?
            .to_path_buf();
        let route = verify_private_directory(&route_path)
            .map_err(|error| format!("route root is not owner-private: {error}"))?;
        let route_path = fs::canonicalize(route.path())
            .map_err(|error| format!("cannot canonicalize route root: {error}"))?;
        require_route_output(&route_path, &args.identity_out, "identity output")?;
        require_route_output(&route_path, &args.capability_file, "capability file")?;
        // Read and validate both trust inputs before SSH can consume their
        // paths.  The verified owner-private parent prevents another local
        // principal from substituting either pin later.
        read_private_file(&args.known_hosts, PRIVATE_MAX)
            .map_err(|error| format!("pinned known-hosts record is unsafe: {error}"))?;
        read_private_file(&args.identity_file, PRIVATE_MAX)
            .map_err(|error| format!("dedicated SSH key is unsafe: {error}"))?;

        let config_leaf = format!("kio-acceptance-ssh-{}.conf", random_suffix()?);
        let config = ssh_config(args)?;
        route
            .write_atomic(
                Path::new(&config_leaf),
                config.as_bytes(),
                Publication::CreateOnly,
            )
            .map_err(|error| format!("cannot create private SSH configuration: {error}"))?;
        route
            .ensure_owner_private(Path::new(&config_leaf))
            .map_err(|error| format!("private SSH configuration is unsafe: {error}"))?;

        Ok(Self {
            route,
            route_path,
            config_leaf,
            candidate: args.candidate.clone(),
            run_id: args.run_id.clone(),
            attempt: args.attempt.clone(),
            os: args.os.clone(),
            capability: None,
        })
    }

    fn config_path(&self) -> PathBuf {
        self.route_path.join(&self.config_leaf)
    }

    fn ssh_command(&self) -> Command {
        let mut command = Command::new("ssh");
        command.arg("-F").arg(self.config_path()).args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=20",
        ]);
        command
    }

    fn base_command(&self) -> Command {
        let mut command = self.ssh_command();
        command.arg("kio-lab-wsl");
        command
    }

    fn invoke(&mut self, verb: Verb) -> Result<Vec<u8>, String> {
        if self.capability.is_none() {
            return Err("lease capability is unavailable".into());
        }
        let command_text = format!(
            "v1 {} {} {} {} {}",
            verb.name(),
            self.candidate,
            self.run_id,
            self.attempt,
            self.os
        );
        let mut command = self.base_command();
        command.arg(command_text);
        let stdin = self
            .capability
            .as_ref()
            .map(|token| BoundedStdin::new(format!("{token}\n").into_bytes(), CAPABILITY_LEN + 1));
        let output = run_bounded_command(
            &mut command,
            BoundedProcessOptions {
                timeout: verb.timeout(),
                max_stdout_bytes: PUBLIC_MAX,
                max_stderr_bytes: PUBLIC_MAX,
            },
            stdin,
        )
        .map_err(|error| format!("dispatcher {} failed: {error}", verb.name()))?;
        if !output.status.success() {
            return Err(format!("dispatcher rejected {}", verb.name()));
        }
        Ok(output.stdout.into_bytes())
    }

    fn initialize(&mut self) -> Result<(), String> {
        let value = canonical_object(&self.invoke(Verb::Init)?, "dispatcher init")?;
        validate_init_response(&value)
    }

    fn write_capability(&self, output: &Path) -> Result<(), String> {
        let capability = self
            .capability
            .as_deref()
            .ok_or_else(|| "lease capability is unavailable".to_owned())?;
        self.write_create_only(
            output,
            format!("{capability}\n").as_bytes(),
            "capability file",
        )
    }

    fn load_capability(&mut self, input: &Path) -> Result<(), String> {
        let bytes = read_private_file(input, (CAPABILITY_LEN + 1) as u64).map_err(|error| {
            format!("lease capability is unavailable; no cleanup command was sent: {error}")
        })?;
        let token = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|value| value.strip_suffix('\n'))
            .filter(|value| {
                bytes.len() == CAPABILITY_LEN + 1 && is_lower_hex(value, CAPABILITY_LEN)
            })
            .ok_or_else(|| "capability file differs".to_owned())?;
        self.capability = Some(token.to_owned());
        Ok(())
    }

    fn write_identity(&mut self, output: &Path) -> Result<String, String> {
        let raw = self.invoke(Verb::Identity)?;
        let value = canonical_object(&raw, "dispatcher identity")?;
        let object = value
            .as_object()
            .ok_or_else(|| "dispatcher identity contract differs".to_owned())?;
        if object.len() != 3
            || !object.contains_key("identity")
            || !object.contains_key("observed")
            || !object.contains_key("public_ca_pem")
        {
            return Err("dispatcher identity contract differs".into());
        }
        self.write_create_only(output, &raw, "identity output")?;
        Ok(sha256(&compact_json(&value["identity"])?))
    }

    fn write_create_only(&self, output: &Path, bytes: &[u8], label: &str) -> Result<(), String> {
        let leaf = route_leaf(&self.route_path, output, label)?;
        self.route
            .write_atomic(Path::new(leaf), bytes, Publication::CreateOnly)
            .map_err(|error| format!("cannot create {label}: {error}"))?;
        self.route
            .ensure_owner_private(Path::new(leaf))
            .map_err(|error| format!("{label} is not owner-private: {error}"))
    }

    fn run_phases(&mut self, identity: &Path, inputs: RunInputs) -> Result<(), String> {
        let result = self.run_phases_inner(identity, &inputs);
        if result.is_err() {
            self.cleanup();
        }
        result
    }

    fn run_phases_inner(&mut self, identity: &Path, inputs: &RunInputs) -> Result<(), String> {
        let identity_value = read_identity(identity)?;
        self.invoke(Verb::StartOcr)?;
        self.observe(&identity_value, "ocr")?;
        let mut tunnel = self.start_tunnel()?;
        let result = (|| {
            self.health(&identity_value, &mut tunnel, "ocr")?;
            self.evaluate("ocr", inputs, &identity_value)?;
            self.invoke(Verb::StopOcr)?;
            self.invoke(Verb::StartEmbedding)?;
            self.observe(&identity_value, "embedding")?;
            self.health(&identity_value, &mut tunnel, "embedding")?;
            self.evaluate("embedding", inputs, &identity_value)?;
            self.invoke(Verb::StopEmbedding)?;
            self.invoke(Verb::Finish)?;
            Ok(())
        })();
        let _ = tunnel.terminate_and_wait();
        result
    }

    fn start_tunnel(&self) -> Result<SupervisedChild, String> {
        let mut command = self.ssh_command();
        command.args([
            "-N",
            "-o",
            "ExitOnForwardFailure=yes",
            "-L",
            "127.0.0.1:18443:127.0.0.1:18443",
            "-L",
            "127.0.0.1:18444:127.0.0.1:18444",
            "kio-lab-wsl",
        ]);
        SupervisedChild::spawn(&mut command, Duration::from_secs(1_300))
            .map_err(|error| format!("cannot start owned SSH tunnel: {error}"))
    }

    fn health(
        &self,
        identity: &Value,
        tunnel: &mut SupervisedChild,
        phase: &str,
    ) -> Result<(), String> {
        let pem = public_ca(identity)?;
        let certificate = ureq::tls::Certificate::from_pem(pem.as_bytes())
            .map_err(|error| format!("public CA PEM is invalid: {error}"))?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(2)))
            .timeout_global(Some(Duration::from_secs(4)))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::new_with_certs(&[certificate]))
                    .build(),
            )
            .build()
            .into();
        let port = match phase {
            "ocr" => 18_443,
            "embedding" => 18_444,
            _ => return Err("invalid local GPU phase".into()),
        };
        let endpoint = format!("https://127.0.0.1:{port}/health");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if tunnel
                .try_wait()
                .map_err(|error| format!("cannot observe owned SSH tunnel: {error}"))?
                .is_some()
            {
                return Err("owned SSH tunnel failed to start".into());
            }
            if agent.get(&endpoint).call().is_ok() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err("forwarded local GPU health check failed".into());
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    fn observe(&mut self, original: &Value, phase: &str) -> Result<(), String> {
        let live = canonical_object(&self.invoke(Verb::Identity)?, "dispatcher identity")?;
        validate_observation(original, &live, phase)?;
        let observed = format!("kio-v1-local-observed-{phase}.json");
        self.route
            .write_atomic(
                Path::new(&observed),
                canonical_json(&live)?.as_slice(),
                Publication::CreateOnly,
            )
            .map_err(|error| format!("cannot create observed phase record: {error}"))?;
        self.route
            .ensure_owner_private(Path::new(&observed))
            .map_err(|error| format!("observed phase record is unsafe: {error}"))
    }

    fn evaluate(&self, phase: &str, inputs: &RunInputs, identity: &Value) -> Result<(), String> {
        let ca_leaf = format!("kio-v1-local-{phase}-ca.pem");
        let identity_leaf = format!("kio-v1-local-{phase}-service-identity.json");
        let ca = public_ca(identity)?;
        let identity_bytes = compact_json(&identity["identity"])?;
        self.route
            .write_atomic(Path::new(&ca_leaf), ca.as_bytes(), Publication::CreateOnly)
            .map_err(|error| format!("cannot create public CA file: {error}"))?;
        self.route
            .write_atomic(
                Path::new(&identity_leaf),
                &identity_bytes,
                Publication::CreateOnly,
            )
            .map_err(|error| format!("cannot create service identity file: {error}"))?;
        let ca_path = self.route_path.join(&ca_leaf);
        let service_path = self.route_path.join(&identity_leaf);
        let result = (|| {
            let mut command = Command::new(&inputs.eval_bin);
            command.args([
                "acceptance",
                "authenticated-local",
                "--phase",
                phase,
                "--expected-case",
            ]);
            command
                .arg(&inputs.expected)
                .args(["--os", &self.os, "--bin"])
                .arg(&inputs.candidate_bin)
                .arg("--fixture")
                .arg(&inputs.fixture)
                .arg("--work-dir")
                .arg(&inputs.work_dir)
                .arg("--receipt")
                .arg(&inputs.receipt)
                .args(["--ocr-endpoint", "https://127.0.0.1:18443"])
                .args(["--embedding-endpoint", "https://127.0.0.1:18444"])
                .arg("--ca-pem")
                .arg(&ca_path)
                .arg("--service-identity")
                .arg(&service_path)
                .arg("--service-identity-sha256")
                .arg(sha256(&identity_bytes));
            let output = run_bounded_command(
                &mut command,
                BoundedProcessOptions {
                    timeout: EVAL_TIMEOUT,
                    max_stdout_bytes: PUBLIC_MAX,
                    max_stderr_bytes: PUBLIC_MAX,
                },
                None,
            )
            .map_err(|error| format!("authenticated-local {phase} failed: {error}"))?;
            if !output.status.success() {
                return Err(format!("authenticated-local {phase} failed"));
            }
            Ok(())
        })();
        let _ = self.route.remove_file(Path::new(&ca_leaf));
        let _ = self.route.remove_file(Path::new(&identity_leaf));
        result
    }

    fn cleanup(&mut self) {
        for verb in [Verb::StopOcr, Verb::StopEmbedding, Verb::Finish] {
            let _ = self.invoke(verb);
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.route.remove_file(Path::new(&self.config_leaf));
    }
}

struct RunInputs {
    expected: PathBuf,
    eval_bin: PathBuf,
    candidate_bin: PathBuf,
    fixture: PathBuf,
    work_dir: PathBuf,
    receipt: PathBuf,
}

impl RunInputs {
    fn from_args(args: &Args) -> Result<Self, String> {
        let inputs = Self {
            expected: required(&args.expected, "expected")?,
            eval_bin: required(&args.eval_bin, "eval-bin")?,
            candidate_bin: required(&args.candidate_bin, "candidate-bin")?,
            fixture: required(&args.fixture, "fixture")?,
            work_dir: required(&args.work_dir, "work-dir")?,
            receipt: required(&args.receipt, "receipt")?,
        };
        if !inputs.expected.is_file()
            || !inputs.eval_bin.is_file()
            || !inputs.candidate_bin.is_file()
            || !inputs.fixture.is_dir()
        {
            return Err("required candidate input is absent".into());
        }
        Ok(inputs)
    }
}

fn validate_init_response(value: &Value) -> Result<(), String> {
    if value.get("status") != Some(&Value::String("ok".into()))
        || value.as_object().map_or(0, |map| map.len()) != 1
    {
        return Err("dispatcher init capability contract differs".into());
    }
    Ok(())
}

/// Injectable prepare-order seam.  The capability publication is the sole
/// operation allowed before init; an init response loss must still attempt an
/// authenticated finish with that already-persisted capability.
#[cfg(test)]
fn prepare_sequence<T>(
    write_capability: impl FnOnce() -> Result<(), String>,
    initialize: impl FnOnce() -> Result<(), String>,
    after_initialize: impl FnOnce() -> Result<T, String>,
    finish: impl FnOnce() -> Result<(), String>,
) -> Result<T, String> {
    write_capability()?;
    let prepared = initialize().and_then(|()| after_initialize());
    finish_after_prepare_error(prepared, finish)
}

/// A lease exists before any local create-only publication.  If a local
/// publication fails, its original error remains authoritative while the
/// authenticated dispatcher finish is attempted exactly once.
fn finish_after_prepare_error<T>(
    operation: Result<T, String>,
    finish: impl FnOnce() -> Result<(), String>,
) -> Result<T, String> {
    match operation {
        Ok(value) => Ok(value),
        Err(error) => {
            let _ = finish();
            Err(error)
        }
    }
}

fn required(value: &Option<PathBuf>, name: &str) -> Result<PathBuf, String> {
    value
        .clone()
        .ok_or_else(|| format!("run inputs are required ({name})"))
}

fn validate_args(args: &Args) -> Result<(), String> {
    if !is_lower_hex(&args.candidate, 40) {
        return Err("invalid candidate".into());
    }
    if !positive_decimal(&args.run_id, 19) {
        return Err("invalid run id".into());
    }
    if !positive_decimal(&args.attempt, 10) {
        return Err("invalid attempt".into());
    }
    if !matches!(args.os.as_str(), "linux" | "macos" | "windows") {
        return Err("invalid OS".into());
    }
    if args.jump_user != "kio-ci" {
        return Err("invalid jump user".into());
    }
    if !valid_host(&args.jump_host) {
        return Err("invalid jump host".into());
    }
    Ok(())
}

fn positive_decimal(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.as_bytes()[0] != b'0'
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn valid_host(value: &str) -> bool {
    if value.is_empty() || value.len() > 253 || value.contains(['\n', '\r', '\0']) {
        return false;
    }
    if value.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    value.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

fn require_route_output(route: &Path, output: &Path, label: &str) -> Result<(), String> {
    route_leaf(route, output, label).map(|_| ())
}

fn route_leaf<'a>(route: &Path, output: &'a Path, label: &str) -> Result<&'a str, String> {
    let parent = output
        .parent()
        .ok_or_else(|| format!("{label} has no parent"))?;
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|error| format!("cannot resolve {label} parent: {error}"))?;
    if canonical_parent != route {
        return Err(format!("{label} must be inside the private route root"));
    }
    output
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| format!("{label} must be a direct UTF-8 route-root leaf"))
}

fn validate_observation(original: &Value, live: &Value, phase: &str) -> Result<(), String> {
    if live.get("identity") != original.get("identity") {
        return Err("service identity changed during phase".into());
    }
    let observations = live
        .get("observed")
        .and_then(Value::as_array)
        .ok_or_else(|| "service identity changed during phase".to_owned())?;
    let entry = observations
        .iter()
        .find(|entry| entry.get("phase") == Some(&Value::String(phase.to_owned())))
        .ok_or_else(|| "active service phase was not observed".to_owned())?;
    let images = entry
        .get("running_image_ids")
        .and_then(Value::as_array)
        .ok_or_else(|| "active service observation lacks measured image identities".to_owned())?;
    let expected_images = if phase == "ocr" { 2 } else { 1 };
    if entry.get("schema") != Some(&Value::String("kio.v1.local_gpu.phase_observed.v1".into()))
        || entry.get("identity_sha256")
            != Some(&Value::String(sha256(&compact_json(
                &original["identity"],
            )?)))
        || images.len() != expected_images
        || images.iter().any(|image| {
            image
                .as_str()
                .is_none_or(|image| !image.starts_with("sha256:") || !is_lower_hex(&image[7..], 64))
        })
    {
        return Err("active service observation lacks measured image identities".into());
    }
    Ok(())
}

fn ssh_config(args: &Args) -> Result<String, String> {
    let known = ssh_path(&args.known_hosts)?;
    let key = ssh_path(&args.identity_file)?;
    Ok(format!(
        "Host kio-lab-jump\n  HostName {}\n  User kio-ci\n  Port 22\n  IdentityFile \"{key}\"\n  IdentitiesOnly yes\n  StrictHostKeyChecking yes\n  UserKnownHostsFile \"{known}\"\n  GlobalKnownHostsFile none\n  HostKeyAlias kio-lab-jump\nHost kio-lab-wsl\n  HostName 127.0.0.1\n  User kio-test\n  Port 2222\n  ProxyJump kio-lab-jump\n  IdentityFile \"{key}\"\n  IdentitiesOnly yes\n  StrictHostKeyChecking yes\n  UserKnownHostsFile \"{known}\"\n  GlobalKnownHostsFile none\n  HostKeyAlias kio-lab-wsl\n",
        args.jump_host
    ))
}

fn ssh_path(path: &Path) -> Result<String, String> {
    let raw = fs::canonicalize(path)
        .map_err(|error| format!("cannot canonicalize SSH path: {error}"))?
        .to_string_lossy()
        .replace('\\', "/");
    if raw.contains(['\0', '\n', '\r', '"']) {
        return Err("unsafe SSH config path".into());
    }
    Ok(raw)
}

fn canonical_object(raw: &[u8], label: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_slice(raw).map_err(|_| format!("{label} is not JSON"))?;
    if !value.is_object() || canonical_json(&value)? != raw {
        return Err(format!("{label} contract differs"));
    }
    Ok(value)
}

fn compact_json(value: &Value) -> Result<Vec<u8>, String> {
    serde_jcs::to_vec(value).map_err(|error| format!("cannot canonicalize JSON: {error}"))
}

fn canonical_json(value: &Value) -> Result<Vec<u8>, String> {
    let mut bytes = compact_json(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn read_identity(path: &Path) -> Result<Value, String> {
    let bytes = read_private_file(path, PUBLIC_MAX as u64)
        .map_err(|error| format!("service identity is unsafe: {error}"))?;
    canonical_object(&bytes, "service identity")
}

fn public_ca(identity: &Value) -> Result<&str, String> {
    let pem = identity
        .get("public_ca_pem")
        .and_then(Value::as_str)
        .ok_or_else(|| "public CA PEM is absent".to_owned())?;
    if !pem.ends_with('\n') || pem.ends_with("\n\n") {
        return Err("public CA PEM must have exactly one final newline".into());
    }
    Ok(pem)
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn new_capability() -> Result<String, String> {
    let mut bytes = [0_u8; CAPABILITY_LEN / 2];
    getrandom::fill(&mut bytes)
        .map_err(|error| format!("cannot generate lease capability: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn random_suffix() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| format!("cannot create private name: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_material_is_private_at_birth_and_existing_paths_are_unchanged() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("route");
        write_route_material(&path, "private test key\n", "pinned test hosts\n").unwrap();
        verify_private_directory(&path).unwrap();
        assert_eq!(
            read_private_file(&path.join("id"), PUBLIC_MAX as u64).unwrap(),
            b"private test key\n"
        );
        assert!(write_route_material(&path, "replacement", "replacement").is_err());
        assert_eq!(
            read_private_file(&path.join("id"), PUBLIC_MAX as u64).unwrap(),
            b"private test key\n"
        );
    }

    #[test]
    fn invalid_route_material_cannot_create_a_directory() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("route");
        assert!(write_route_material(&path, "", "hosts").is_err());
        assert!(write_route_material(&path, "key", &"x".repeat(PUBLIC_MAX + 1)).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn dispatcher_arguments_and_host_grammar_are_strict() {
        assert!(is_lower_hex(&"a".repeat(40), 40));
        assert!(!is_lower_hex(&"A".repeat(40), 40));
        assert!(positive_decimal("42", 19));
        assert!(!positive_decimal("042", 19));
        for host in ["100.64.0.7", "gpu.example.test", "a-b.example"] {
            assert!(valid_host(host), "{host}");
        }
        for host in ["host;id", "host name", "-bad.example", "bad-.example"] {
            assert!(!valid_host(host), "{host}");
        }
    }

    #[test]
    fn client_generated_capability_is_exact_lower_hex_token() {
        let token = new_capability().unwrap();
        assert!(is_lower_hex(&token, CAPABILITY_LEN));
    }

    #[test]
    fn canonical_dispatch_responses_reject_noncanonical_or_wrong_shape() {
        let init = canonical_json(&serde_json::json!({"status":"ok"})).unwrap();
        let init_value = canonical_object(&init, "init").unwrap();
        assert!(validate_init_response(&init_value).is_ok());
        let old_server_token =
            canonical_json(&serde_json::json!({"capability":"b".repeat(64),"status":"ok"}))
                .unwrap();
        let old_server_token = canonical_object(&old_server_token, "init").unwrap();
        assert!(validate_init_response(&old_server_token).is_err());
        assert!(canonical_object(b"{\"status\":\"ok\", \"capability\":\"x\"}\n", "init").is_err());
        assert!(canonical_object(b"[]\n", "identity").is_err());
    }

    #[test]
    fn capability_create_only_collision_does_not_invoke_remote_init() {
        let launcher_calls = std::cell::RefCell::new(Vec::new());
        let error = prepare_sequence(
            || {
                launcher_calls.borrow_mut().push("capability");
                Err("cannot create capability file: already exists".into())
            },
            || {
                launcher_calls.borrow_mut().push("init");
                Ok(())
            },
            || Ok(()),
            || {
                launcher_calls.borrow_mut().push("finish");
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error, "cannot create capability file: already exists");
        assert_eq!(*launcher_calls.borrow(), ["capability"]);
    }

    #[test]
    fn lost_init_response_retains_capability_and_attempts_authenticated_finish() {
        let launcher_calls = std::cell::RefCell::new(Vec::new());
        let capability = "b".repeat(CAPABILITY_LEN);
        let persisted_capability = capability.clone();
        let error = prepare_sequence(
            || {
                launcher_calls.borrow_mut().push("capability");
                Ok(())
            },
            || {
                launcher_calls.borrow_mut().push("init");
                Err("dispatcher init failed: response lost".into())
            },
            || Ok(()),
            || {
                launcher_calls.borrow_mut().push("finish");
                assert_eq!(persisted_capability, capability);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error, "dispatcher init failed: response lost");
        assert_eq!(*launcher_calls.borrow(), ["capability", "init", "finish"]);
    }

    #[test]
    fn prepare_preserves_original_error_when_injected_finish_fails() {
        let error = prepare_sequence(
            || Ok(()),
            || Err("dispatcher unavailable".into()),
            || Ok(()),
            || Err("finish unavailable".into()),
        )
        .unwrap_err();
        assert_eq!(error, "dispatcher unavailable");
    }

    #[cfg(unix)]
    #[test]
    fn route_root_owns_the_pinned_two_hop_configuration() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let known_hosts = temp.path().join("known_hosts");
        let identity_file = temp.path().join("id");
        fs::write(&known_hosts, "jump ssh-ed25519 AAAA\n").unwrap();
        fs::write(&identity_file, "private\n").unwrap();
        fs::set_permissions(&known_hosts, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&identity_file, fs::Permissions::from_mode(0o600)).unwrap();
        let args = Args {
            mode: Mode::Prepare,
            candidate: "a".repeat(40),
            run_id: "42".into(),
            attempt: "1".into(),
            os: "windows".into(),
            jump_host: "100.64.0.7".into(),
            jump_user: "kio-ci".into(),
            known_hosts,
            identity_file,
            identity_out: temp.path().join("identity.json"),
            capability_file: temp.path().join("capability"),
            expected: None,
            eval_bin: None,
            candidate_bin: None,
            fixture: None,
            work_dir: None,
            receipt: None,
        };
        let client = Client::new(&args).unwrap();
        let config = read_private_file(&client.config_path(), PRIVATE_MAX).unwrap();
        let config = String::from_utf8(config).unwrap();
        assert!(config.contains("Host kio-lab-jump"));
        assert!(config.contains("Host kio-lab-wsl"));
        assert_eq!(config.matches("GlobalKnownHostsFile none").count(), 2);
        assert!(!config.contains('\\'));
    }

    #[test]
    fn observation_requires_exact_identity_phase_and_image_ids() {
        let identity =
            serde_json::json!({"schema":"kio.local-gpu.identity/v1","run_nonce":"a".repeat(64)});
        let original =
            serde_json::json!({"identity":identity,"observed":[],"public_ca_pem":"CA\n"});
        let digest = sha256(&compact_json(&original["identity"]).unwrap());
        let live = serde_json::json!({
            "identity": original["identity"],
            "observed": [{
                "schema":"kio.v1.local_gpu.phase_observed.v1",
                "phase":"ocr",
                "identity_sha256":digest,
                "running_image_ids":[format!("sha256:{}", "a".repeat(64)), format!("sha256:{}", "b".repeat(64))]
            }],
            "public_ca_pem":"CA\n"
        });
        assert!(validate_observation(&original, &live, "ocr").is_ok());
        assert!(validate_observation(&original, &live, "embedding").is_err());
    }
}
