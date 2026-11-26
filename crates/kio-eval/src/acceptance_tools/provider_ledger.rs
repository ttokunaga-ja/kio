//! Fail-closed Git authority for bounded provider-acceptance reservations.
//!
//! This module deliberately knows no provider credential and performs only
//! GitHub Git-data API requests. An uncertain ref update is never treated as
//! unspent authority.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    path::{Path, PathBuf},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use clap::{Args as ClapArgs, Subcommand};
use kio_core::store_dir::{Publication, StoreDirectory, restrict_new_private_directory};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

const SCHEMA: &str = "kio.provider-acceptance-ledger/v1";
const BRANCH: &str = "codex/provider-acceptance-ledger";
const CAP: u64 = 10_000_000;
const ALLOCATION: u64 = 100_000;
const PROVIDERS: [&str; 2] = ["mistral", "gemini"];
const OSES: [&str; 3] = ["linux", "macos", "windows"];
const MAX_BLOB: usize = 16_384;
// One bootstrap, one approved transition, and at most 33 full reservations.
const MAX_HISTORY: usize = 256;
const GITHUB_API: &str = "https://api.github.com";

pub type Result<T> = std::result::Result<T, String>;

/// Arguments wired by the dedicated `kio-acceptance-tools provider-ledger`
/// binary. The GitHub token is intentionally accepted only from `GITHUB_TOKEN`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Initialize {
        #[command(flatten)]
        common: CommonArgs,
    },
    Transition {
        #[command(flatten)]
        common: CommonArgs,
        #[arg(long)]
        predecessor_campaign_id: String,
        #[arg(long)]
        predecessor_tip: String,
    },
    #[command(name = "reserve-all")]
    ReserveAll {
        #[command(flatten)]
        common: CommonArgs,
        #[arg(long)]
        candidate_sha: String,
        #[arg(long)]
        workflow_sha256: String,
    },
    Verify {
        #[command(flatten)]
        common: CommonArgs,
        #[arg(long)]
        candidate_sha: String,
        #[arg(long)]
        reservation_commit: String,
        #[arg(long)]
        provider: String,
        #[arg(long = "os")]
        os_name: String,
    },
    #[command(name = "export-state")]
    ExportState {
        #[command(flatten)]
        common: CommonArgs,
        #[arg(long)]
        candidate_sha: String,
        #[arg(long)]
        reservation_commit: String,
        #[arg(long)]
        provider: String,
        #[arg(long = "os")]
        os_name: String,
        #[arg(long)]
        state_dir: PathBuf,
    },
}

#[derive(Debug, ClapArgs)]
struct CommonArgs {
    #[arg(long)]
    repository: String,
    #[arg(long)]
    campaign_id: String,
    #[arg(long)]
    run_id: u64,
    #[arg(long)]
    attempt: u64,
}

impl CommonArgs {
    fn campaign_request(self) -> Request {
        Request {
            campaign_id: self.campaign_id,
            candidate_sha: None,
            workflow_sha256: None,
            run_id: self.run_id,
            attempt: self.attempt,
            reservation_commit: None,
            provider: None,
            os_name: None,
            state_dir: None,
        }
    }
}

/// Execute one bounded authority action and print its canonical JSON result.
pub fn run(args: Args) -> Result<()> {
    let token = env::var("GITHUB_TOKEN").map_err(|_| "GITHUB_TOKEN is required".to_owned())?;
    if token.is_empty() {
        return Err("GITHUB_TOKEN is required".to_owned());
    }
    let repository = match &args.command {
        Command::Initialize { common }
        | Command::Transition { common, .. }
        | Command::ReserveAll { common, .. }
        | Command::Verify { common, .. }
        | Command::ExportState { common, .. } => &common.repository,
    };
    let mut api = GitHubApi::new(repository, token)?;
    let result = match args.command {
        Command::Initialize { common } => initialize(&mut api, &common.campaign_request())?,
        Command::Transition {
            common,
            predecessor_campaign_id,
            predecessor_tip,
        } => transition(
            &mut api,
            &common.campaign_request(),
            &predecessor_campaign_id,
            &predecessor_tip,
        )?,
        Command::ReserveAll {
            common,
            candidate_sha,
            workflow_sha256,
        } => reserve_all(
            &mut api,
            &Request {
                campaign_id: common.campaign_id,
                candidate_sha: Some(candidate_sha),
                workflow_sha256: Some(workflow_sha256),
                run_id: common.run_id,
                attempt: common.attempt,
                reservation_commit: None,
                provider: None,
                os_name: None,
                state_dir: None,
            },
        )?,
        Command::Verify {
            common,
            candidate_sha,
            reservation_commit,
            provider,
            os_name,
        } => {
            verify(
                &mut api,
                &Request {
                    campaign_id: common.campaign_id,
                    candidate_sha: Some(candidate_sha),
                    workflow_sha256: None,
                    run_id: common.run_id,
                    attempt: common.attempt,
                    reservation_commit: Some(reservation_commit),
                    provider: Some(provider),
                    os_name: Some(os_name),
                    state_dir: None,
                },
            )?
            .0
        }
        Command::ExportState {
            common,
            candidate_sha,
            reservation_commit,
            provider,
            os_name,
            state_dir,
        } => export_state(
            &mut api,
            &Request {
                campaign_id: common.campaign_id,
                candidate_sha: Some(candidate_sha),
                workflow_sha256: None,
                run_id: common.run_id,
                attempt: common.attempt,
                reservation_commit: Some(reservation_commit),
                provider: Some(provider),
                os_name: Some(os_name),
                state_dir: Some(state_dir),
            },
        )?,
    };
    print!(
        "{}",
        String::from_utf8(canonical(&result)?)
            .map_err(|_| "canonical JSON is not UTF-8".to_owned())?
    );
    Ok(())
}

#[derive(Debug)]
struct Request {
    campaign_id: String,
    candidate_sha: Option<String>,
    workflow_sha256: Option<String>,
    run_id: u64,
    attempt: u64,
    reservation_commit: Option<String>,
    provider: Option<String>,
    os_name: Option<String>,
    state_dir: Option<PathBuf>,
}

trait Api {
    fn reference(&mut self) -> Result<Option<Value>>;
    fn commit(&mut self, oid: &str) -> Result<Value>;
    fn tree(&mut self, oid: &str) -> Result<Vec<Value>>;
    fn blob(&mut self, oid: &str) -> Result<Value>;
    fn create_commit(
        &mut self,
        files: &BTreeMap<String, Value>,
        parent: Option<&str>,
    ) -> Result<String>;
    fn cas_ref(&mut self, old: Option<&str>, new: &str, create: bool) -> Result<()>;
}

struct GitHubApi {
    base: String,
    token: String,
    agent: ureq::Agent,
}

impl GitHubApi {
    fn new(repository: &str, token: String) -> Result<Self> {
        validate_repository(repository)?;
        let agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .proxy(None)
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_global(Some(Duration::from_secs(20)))
            .build()
            .into();
        Ok(Self {
            base: format!("{GITHUB_API}/repos/{repository}"),
            token,
            agent,
        })
    }

    fn request(&mut self, method: &str, path: &str, data: Option<&Value>) -> Result<Option<Value>> {
        if !path.starts_with('/') || path.contains("//") || !self.base.starts_with(GITHUB_API) {
            return Err("invalid fixed GitHub API path".to_owned());
        }
        let url = format!("{}{path}", self.base);
        let authorization = format!("Bearer {}", self.token);
        let mut response = match (method, data) {
            ("GET", None) => self
                .agent
                .get(&url)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .header("Authorization", &authorization)
                .call(),
            ("POST", Some(data)) => self
                .agent
                .post(&url)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .header("Authorization", &authorization)
                .header("Content-Type", "application/json")
                .send(canonical(data)?),
            ("PATCH", Some(data)) => self
                .agent
                .patch(&url)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .header("Authorization", &authorization)
                .header("Content-Type", "application/json")
                .send(canonical(data)?),
            _ => return Err("invalid fixed GitHub API method".to_owned()),
        }
        .map_err(|error| format!("GitHub API request failed: {error}"))?;
        let status = response.status().as_u16();
        if status == 404 {
            return Ok(None);
        }
        if status == 422 {
            return Err("GitHub rejected ledger update".to_owned());
        }
        if status != 200 && status != 201 {
            return Err(format!("GitHub API request failed: {status}"));
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit((MAX_BLOB * 8 + 1) as u64)
            .read_to_vec()
            .map_err(|error| format!("GitHub response exceeds bound: {error}"))?;
        if bytes.len() > MAX_BLOB * 8 {
            return Err("GitHub response exceeds bound".to_owned());
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| "GitHub response is not JSON".to_owned())
    }
}

impl Api for GitHubApi {
    fn reference(&mut self) -> Result<Option<Value>> {
        self.request("GET", &format!("/git/ref/heads/{BRANCH}"), None)
    }

    fn commit(&mut self, oid: &str) -> Result<Value> {
        require_sha(oid, "commit")?;
        self.request("GET", &format!("/git/commits/{oid}"), None)?
            .ok_or_else(|| "ledger commit is missing".to_owned())
    }

    fn tree(&mut self, oid: &str) -> Result<Vec<Value>> {
        require_sha(oid, "tree")?;
        let result = self
            .request("GET", &format!("/git/trees/{oid}"), None)?
            .ok_or_else(|| "ledger tree is missing or exceeds bound".to_owned())?;
        if get_bool(&result, "truncated")?.unwrap_or(false) {
            return Err("ledger tree is missing or exceeds bound".to_owned());
        }
        let entries = get_array(&result, "tree")?.to_vec();
        if entries.len() > 8 {
            return Err("ledger tree has too many entries".to_owned());
        }
        Ok(entries)
    }

    fn blob(&mut self, oid: &str) -> Result<Value> {
        require_sha(oid, "blob")?;
        self.request("GET", &format!("/git/blobs/{oid}"), None)?
            .ok_or_else(|| "ledger blob is missing".to_owned())
    }

    fn create_commit(
        &mut self,
        files: &BTreeMap<String, Value>,
        parent: Option<&str>,
    ) -> Result<String> {
        let mut blobs = Vec::with_capacity(files.len());
        for (path, value) in files {
            let content = String::from_utf8(canonical(value)?)
                .map_err(|_| "canonical JSON is not UTF-8".to_owned())?;
            let blob = self
                .request(
                    "POST",
                    "/git/blobs",
                    Some(&json!({"content": content, "encoding": "utf-8"})),
                )?
                .ok_or_else(|| "GitHub rejected ledger update".to_owned())?;
            let sha = get_string(&blob, "sha")?;
            require_sha(sha, "new blob")?;
            blobs.push(json!({"path": path, "mode": "100644", "type": "blob", "sha": sha}));
        }
        let tree = self
            .request("POST", "/git/trees", Some(&json!({"tree": blobs})))?
            .ok_or_else(|| "GitHub rejected ledger update".to_owned())?;
        let tree_sha = get_string(&tree, "sha")?;
        require_sha(tree_sha, "new tree")?;
        let mut payload = Map::from_iter([
            (
                "message".to_owned(),
                Value::String("provider acceptance ledger".to_owned()),
            ),
            ("tree".to_owned(), Value::String(tree_sha.to_owned())),
        ]);
        if let Some(parent) = parent {
            require_sha(parent, "parent commit")?;
            payload.insert("parents".to_owned(), json!([parent]));
        }
        let commit = self
            .request("POST", "/git/commits", Some(&Value::Object(payload)))?
            .ok_or_else(|| "GitHub rejected ledger update".to_owned())?;
        let sha = get_string(&commit, "sha")?;
        require_sha(sha, "new commit")?;
        Ok(sha.to_owned())
    }

    fn cas_ref(&mut self, old: Option<&str>, new: &str, create: bool) -> Result<()> {
        require_sha(new, "new commit")?;
        let result = if create {
            self.request(
                "POST",
                "/git/refs",
                Some(&json!({"ref": format!("refs/heads/{BRANCH}"), "sha": new})),
            )?
        } else {
            if old.is_none() {
                return Err("ledger ref update lacks an expected parent".to_owned());
            }
            self.request(
                "PATCH",
                &format!("/git/refs/heads/{BRANCH}"),
                Some(&json!({"sha": new, "force": false})),
            )?
        };
        let Some(result) = result else {
            return Err("ledger ref update was rejected".to_owned());
        };
        // The successful update response is authoritative. A read-back alone
        // can lag the write on GitHub, so it only settles other responses.
        if ref_sha(&result) == Some(new) {
            return Ok(());
        }
        for delay in [0, 1, 2, 4, 8] {
            std::thread::sleep(Duration::from_secs(delay));
            if self.reference()?.as_ref().and_then(ref_sha) == Some(new) {
                return Ok(());
            }
        }
        Err("ledger ref update has unknown outcome".to_owned())
    }
}

fn ref_sha(reference: &Value) -> Option<&str> {
    reference.pointer("/object/sha").and_then(Value::as_str)
}

fn canonical(value: &Value) -> Result<Vec<u8>> {
    let mut bytes =
        serde_jcs::to_vec(value).map_err(|error| format!("cannot canonicalize JSON: {error}"))?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn digest(value: &Value) -> Result<String> {
    let digest = Sha256::digest(canonical(value)?);
    Ok(hex(digest.as_ref()))
}

fn require_id(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 96
        || !value.bytes().enumerate().all(|(index, byte)| {
            (byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
                && (index != 0 || byte.is_ascii_alphanumeric())
        })
    {
        return Err(format!("invalid {label}"));
    }
    Ok(())
}

fn require_sha(value: &str, label: &str) -> Result<()> {
    if value.len() != 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!("invalid {label}"));
    }
    Ok(())
}

fn require_digest(value: &str, label: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!("invalid {label}"));
    }
    Ok(())
}

fn validate_repository(repository: &str) -> Result<()> {
    let mut components = repository.split('/');
    let (Some(owner), Some(name), None) = (components.next(), components.next(), components.next())
    else {
        return Err("invalid GitHub repository".to_owned());
    };
    require_id(owner, "GitHub owner")?;
    require_id(name, "GitHub repository")
}

fn get_object(value: &Value) -> Result<&Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| "GitHub response has invalid object".to_owned())
}

fn get_string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    get_object(value)?
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("GitHub response has invalid {key}"))
}

fn get_array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value]> {
    get_object(value)?
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| format!("GitHub response has invalid {key}"))
}

fn get_bool(value: &Value, key: &str) -> Result<Option<bool>> {
    match get_object(value)?.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("GitHub response has invalid {key}")),
    }
}

fn tree_files<A: Api>(api: &mut A, commit: &Value) -> Result<BTreeMap<String, Value>> {
    let tree = get_object(commit)?
        .get("tree")
        .ok_or_else(|| "ledger commit has no tree".to_owned())?;
    let tree_sha = get_string(tree, "sha")?;
    require_sha(tree_sha, "tree")?;
    let entries = api.tree(tree_sha)?;
    let mut files = BTreeMap::new();
    for entry in entries {
        let path = get_string(&entry, "path")?;
        let valid_reservation = path
            .strip_prefix("reservation-")
            .and_then(|value| value.strip_suffix(".json"))
            .is_some_and(|allocation| require_id(allocation, "reservation path").is_ok());
        if get_string(&entry, "type")? != "blob"
            || get_string(&entry, "mode")? != "100644"
            || (path != "campaign.json" && !valid_reservation)
            || files.contains_key(path)
        {
            return Err("ledger tree has invalid entry".to_owned());
        }
        let blob_sha = get_string(&entry, "sha")?;
        require_sha(blob_sha, "blob")?;
        let blob = api.blob(blob_sha)?;
        files.insert(path.to_owned(), decode_blob(&blob)?);
    }
    Ok(files)
}

fn decode_blob(blob: &Value) -> Result<Value> {
    if get_string(blob, "encoding")? != "base64" {
        return Err("ledger blob is missing or encoded unexpectedly".to_owned());
    }
    let encoded = get_string(blob, "content")?;
    if encoded.bytes().any(|byte| {
        !byte.is_ascii()
            || (byte.is_ascii_whitespace() && !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    }) {
        return Err("ledger blob base64 contains invalid whitespace".to_owned());
    }
    let compact: String = encoded
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect();
    let raw = BASE64
        .decode(compact)
        .map_err(|_| "ledger blob is not valid base64".to_owned())?;
    if raw.len() > MAX_BLOB {
        return Err("ledger blob exceeds bound".to_owned());
    }
    serde_json::from_slice(&raw).map_err(|_| "ledger blob is not JSON".to_owned())
}

fn validate_campaign(campaign: &Value) -> Result<()> {
    let object = get_object(campaign)?;
    if get_string(campaign, "schema")? != SCHEMA || get_string(campaign, "kind")? != "campaign" {
        return Err("invalid campaign record".to_owned());
    }
    require_id(get_string(campaign, "campaign_id")?, "campaign id")?;
    require_digest(get_string(campaign, "epoch")?, "campaign epoch")?;
    let caps = object
        .get("caps_microusd")
        .and_then(Value::as_object)
        .ok_or_else(|| "campaign caps are not the approved bounds".to_owned())?;
    if caps.len() != PROVIDERS.len()
        || PROVIDERS
            .iter()
            .any(|provider| caps.get(*provider).and_then(Value::as_u64) != Some(CAP))
    {
        return Err("campaign caps are not the approved bounds".to_owned());
    }
    if get_string(campaign, "workflow_path")? != ".github/workflows/v1-provider-acceptance.yml" {
        return Err("campaign workflow path differs".to_owned());
    }
    if object.contains_key("workflow_sha256") {
        require_digest(get_string(campaign, "workflow_sha256")?, "workflow digest")?;
    }
    for key in ["initialize_run_id", "initialize_attempt"] {
        if object
            .get(key)
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .is_none()
        {
            return Err(format!(
                "invalid initialization {}",
                key.strip_prefix("initialize_").unwrap_or(key)
            ));
        }
    }
    Ok(())
}

fn chain_body(record: &Value) -> Result<Value> {
    let mut body = get_object(record)?.clone();
    body.remove("chain_digest");
    Ok(Value::Object(body))
}

fn validate_reservation(
    record: &Value,
    campaign: &Value,
    parent: &str,
    sequence: u64,
    seen: &mut BTreeSet<String>,
) -> Result<()> {
    const REQUIRED: [&str; 14] = [
        "schema",
        "kind",
        "campaign_id",
        "candidate_sha",
        "provider",
        "os",
        "allocation_id",
        "allocated_microusd",
        "run_id",
        "attempt",
        "predecessor_oid",
        "sequence",
        "chain_digest",
        "state",
    ];
    let object = get_object(record)?;
    if object.len() != REQUIRED.len() + usize::from(object.contains_key("workflow_sha256"))
        || REQUIRED.iter().any(|key| !object.contains_key(*key))
        || get_string(record, "schema")? != SCHEMA
        || get_string(record, "kind")? != "reservation"
    {
        return Err("invalid reservation record".to_owned());
    }
    if get_string(record, "campaign_id")? != get_string(campaign, "campaign_id")?
        || !PROVIDERS.contains(&get_string(record, "provider")?)
        || !OSES.contains(&get_string(record, "os")?)
    {
        return Err("reservation campaign or lane mismatch".to_owned());
    }
    if object.contains_key("workflow_sha256") {
        require_digest(get_string(record, "workflow_sha256")?, "workflow digest")?;
    }
    require_sha(get_string(record, "candidate_sha")?, "candidate sha")?;
    require_id(get_string(record, "allocation_id")?, "allocation id")?;
    if object.get("allocated_microusd").and_then(Value::as_u64) != Some(ALLOCATION)
        || get_string(record, "state")? != "reserved_unknown"
    {
        return Err("reservation amount or state differs".to_owned());
    }
    for key in ["run_id", "attempt", "sequence"] {
        if object
            .get(key)
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .is_none()
        {
            return Err("invalid reservation sequence".to_owned());
        }
    }
    if get_string(record, "predecessor_oid")? != parent
        || object.get("sequence").and_then(Value::as_u64) != Some(sequence)
    {
        return Err("reservation predecessor or sequence differs".to_owned());
    }
    if get_string(record, "chain_digest")? != digest(&chain_body(record)?)? {
        return Err("reservation chain digest differs".to_owned());
    }
    let expected = allocation_id(
        get_string(record, "provider")?,
        get_string(record, "os")?,
        get_string(record, "candidate_sha")?,
        object.get("run_id").and_then(Value::as_u64).unwrap_or(0),
        object.get("attempt").and_then(Value::as_u64).unwrap_or(0),
    );
    if get_string(record, "allocation_id")? != expected || !seen.insert(expected) {
        return Err("duplicate or noncanonical allocation".to_owned());
    }
    Ok(())
}

#[derive(Debug)]
struct VerifiedState {
    tip: String,
    root: String,
    campaign: Value,
    allocations: BTreeMap<String, Value>,
    totals: BTreeMap<String, u64>,
    commits: u64,
    campaign_ids: BTreeSet<String>,
}

fn load_verified<A: Api>(api: &mut A, expected_commit: Option<&str>) -> Result<VerifiedState> {
    let reference = api
        .reference()?
        .ok_or_else(|| "ledger branch is missing".to_owned())?;
    let mut tip = get_object(&reference)?
        .get("object")
        .ok_or_else(|| "ledger ref is invalid".to_owned())
        .and_then(|object| get_string(object, "sha").map(str::to_owned))?;
    require_sha(&tip, "ledger tip")?;
    let mut commits = Vec::new();
    let mut oid = tip.clone();
    for _ in 0..MAX_HISTORY {
        let commit = api.commit(&oid)?;
        let next_parent = {
            let parents = get_array(&commit, "parents")?;
            if parents.is_empty() {
                None
            } else if parents.len() == 1 {
                Some(get_string(&parents[0], "sha")?.to_owned())
            } else {
                return Err("ledger history is not linear".to_owned());
            }
        };
        commits.push((oid.clone(), commit));
        let Some(parent) = next_parent else {
            break;
        };
        oid = parent;
        require_sha(&oid, "ledger parent")?;
    }
    if !commits.last().is_some_and(|(_, commit)| {
        get_array(commit, "parents").is_ok_and(|parents| parents.is_empty())
    }) {
        return Err("ledger history exceeds bound".to_owned());
    }
    commits.reverse();
    if let Some(expected) = expected_commit {
        require_sha(expected, "reservation commit")?;
        let position = commits
            .iter()
            .position(|(oid, _)| oid == expected)
            .ok_or_else(|| "pinned reservation commit is not ledger ancestry".to_owned())?;
        commits.truncate(position + 1);
        tip = expected.to_owned();
    }
    let (root, root_commit) = commits
        .first()
        .ok_or_else(|| "ledger branch is missing".to_owned())?;
    let root_files = tree_files(api, root_commit)?;
    if root_files.len() != 1 || !root_files.contains_key("campaign.json") {
        return Err("ledger root is not an immutable bootstrap".to_owned());
    }
    let mut campaign = root_files
        .get("campaign.json")
        .cloned()
        .ok_or_else(|| "ledger root is not an immutable bootstrap".to_owned())?;
    validate_campaign(&campaign)?;
    if get_object(&campaign)?.len() != 8 + usize::from(campaign.get("workflow_sha256").is_some()) {
        return Err("ledger root has unexpected campaign fields".to_owned());
    }
    let mut campaign_ids = BTreeSet::from([get_string(&campaign, "campaign_id")?.to_owned()]);
    let mut allocations = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut totals = BTreeMap::from(PROVIDERS.map(|provider| (provider.to_owned(), 0_u64)));
    for (index, (_, commit)) in commits.iter().enumerate().skip(1) {
        let files = tree_files(api, commit)?;
        let parent = &commits[index - 1].0;
        if let Some(successor) = files.get("campaign.json")
            && files.len() == 1
        {
            validate_transition(successor, &campaign, parent)?;
            if !campaign_ids.insert(get_string(successor, "campaign_id")?.to_owned()) {
                return Err("campaign id already exists in ledger history".to_owned());
            }
            campaign = successor.clone();
            continue;
        }
        if files.len() != 6 || files.contains_key("campaign.json") {
            return Err("reservation commit must atomically contain all six lanes".to_owned());
        }
        let mut lanes = BTreeSet::new();
        let mut transaction = BTreeSet::new();
        for (path, record) in &files {
            let allocation = path
                .strip_prefix("reservation-")
                .and_then(|value| value.strip_suffix(".json"))
                .ok_or_else(|| "reservation path does not bind allocation".to_owned())?;
            if allocation != get_string(record, "allocation_id")? {
                return Err("reservation path does not bind allocation".to_owned());
            }
            validate_reservation(record, &campaign, parent, index as u64, &mut seen)?;
            let provider = get_string(record, "provider")?;
            let os_name = get_string(record, "os")?;
            lanes.insert((provider.to_owned(), os_name.to_owned()));
            transaction.insert((
                get_string(record, "candidate_sha")?.to_owned(),
                record.get("run_id").and_then(Value::as_u64).unwrap_or(0),
                record.get("attempt").and_then(Value::as_u64).unwrap_or(0),
                record.get("workflow_sha256").and_then(Value::as_str),
            ));
            let total = totals
                .get_mut(provider)
                .ok_or_else(|| "reservation campaign or lane mismatch".to_owned())?;
            *total = total
                .checked_add(ALLOCATION)
                .ok_or_else(|| "provider cap exceeded".to_owned())?;
            if *total > CAP {
                return Err("provider cap exceeded".to_owned());
            }
            allocations.insert(allocation.to_owned(), record.clone());
        }
        let expected_lanes: BTreeSet<_> = PROVIDERS
            .into_iter()
            .flat_map(|provider| {
                OSES.into_iter()
                    .map(move |os_name| (provider.to_owned(), os_name.to_owned()))
            })
            .collect();
        if lanes != expected_lanes || transaction.len() != 1 {
            return Err(
                "reservation commit does not bind one complete candidate transaction".to_owned(),
            );
        }
    }
    Ok(VerifiedState {
        tip,
        root: root.clone(),
        campaign,
        allocations,
        totals,
        commits: commits.len() as u64,
        campaign_ids,
    })
}

fn new_campaign(args: &Request) -> Result<Map<String, Value>> {
    require_id(&args.campaign_id, "campaign id")?;
    require_positive(args.run_id, "initialization run")?;
    require_positive(args.attempt, "initialization attempt")?;
    let mut epoch_bytes = [0_u8; 32];
    getrandom::fill(&mut epoch_bytes)
        .map_err(|error| format!("cannot create campaign epoch: {error}"))?;
    let campaign = json!({
        "schema": SCHEMA, "kind": "campaign", "campaign_id": args.campaign_id,
        "epoch": hex(&epoch_bytes), "caps_microusd": {"mistral": CAP, "gemini": CAP},
        "workflow_path": ".github/workflows/v1-provider-acceptance.yml",
        "initialize_run_id": args.run_id, "initialize_attempt": args.attempt,
    });
    Ok(get_object(&campaign)?.clone())
}

fn initialize<A: Api>(api: &mut A, args: &Request) -> Result<Value> {
    if api.reference()?.is_some() {
        return Err("ledger branch already exists; bootstrap is immutable".to_owned());
    }
    let campaign = Value::Object(new_campaign(args)?);
    let files = BTreeMap::from([("campaign.json".to_owned(), campaign)]);
    let commit = api.create_commit(&files, None)?;
    api.cas_ref(None, &commit, true)?;
    Ok(json!({"commit": commit, "campaign_id": args.campaign_id}))
}

fn validate_transition(successor: &Value, predecessor: &Value, parent: &str) -> Result<()> {
    validate_campaign(successor)?;
    if get_object(successor)?.len() != 10 + usize::from(successor.get("workflow_sha256").is_some())
        || get_string(successor, "predecessor_campaign_id")?
            != get_string(predecessor, "campaign_id")?
        || get_string(successor, "predecessor_tip")? != parent
        || get_string(successor, "campaign_id")? == get_string(predecessor, "campaign_id")?
    {
        return Err("invalid successor or predecessor campaign binding".to_owned());
    }
    Ok(())
}

fn transition<A: Api>(
    api: &mut A,
    args: &Request,
    predecessor_campaign_id: &str,
    predecessor_tip: &str,
) -> Result<Value> {
    require_id(predecessor_campaign_id, "predecessor campaign id")?;
    require_sha(predecessor_tip, "predecessor ledger tip")?;
    let state = load_verified(api, None)?;
    if state.campaign_ids.contains(&args.campaign_id) {
        return Err("campaign id already exists in ledger history".to_owned());
    }
    let mut successor = new_campaign(args)?;
    successor.insert(
        "predecessor_campaign_id".to_owned(),
        json!(predecessor_campaign_id),
    );
    successor.insert("predecessor_tip".to_owned(), json!(predecessor_tip));
    let successor = Value::Object(successor);
    validate_transition(&successor, &state.campaign, &state.tip)?;
    let records = BTreeMap::from([("campaign.json".to_owned(), successor.clone())]);
    if state.commits >= MAX_HISTORY as u64 {
        return Err("ledger history is at its bound".to_owned());
    }
    let commit = append(api, &state.tip, &records)?;
    Ok(json!({"commit": commit, "campaign": successor}))
}

fn reserve_all<A: Api>(api: &mut A, args: &Request) -> Result<Value> {
    require_id(&args.campaign_id, "campaign id")?;
    let candidate_sha = args
        .candidate_sha
        .as_deref()
        .ok_or_else(|| "--candidate-sha is required".to_owned())?;
    require_sha(candidate_sha, "candidate sha")?;
    require_positive(args.run_id, "reservation run")?;
    require_positive(args.attempt, "reservation attempt")?;
    let state = load_verified(api, None)?;
    if get_string(&state.campaign, "campaign_id")? != args.campaign_id {
        return Err("campaign id does not match active campaign".to_owned());
    }
    let workflow_sha256 = args
        .workflow_sha256
        .as_deref()
        .ok_or_else(|| "--workflow-sha256 is required".to_owned())?;
    require_digest(workflow_sha256, "workflow digest")?;
    for provider in PROVIDERS {
        if state.totals[provider]
            .checked_add((OSES.len() as u64) * ALLOCATION)
            .filter(|value| *value <= CAP)
            .is_none()
        {
            return Err("provider campaign cap would be exceeded".to_owned());
        }
    }
    let mut records = BTreeMap::new();
    for provider in PROVIDERS {
        for os_name in OSES {
            let allocation =
                allocation_id(provider, os_name, candidate_sha, args.run_id, args.attempt);
            if state.allocations.contains_key(&allocation) {
                return Err("allocation already exists; unknown work is never replayed".to_owned());
            }
            let mut record = json!({
                "schema": SCHEMA, "kind": "reservation", "campaign_id": args.campaign_id,
                "candidate_sha": candidate_sha, "provider": provider, "os": os_name,
                "workflow_sha256": workflow_sha256,
                "allocation_id": allocation, "allocated_microusd": ALLOCATION,
                "run_id": args.run_id, "attempt": args.attempt, "predecessor_oid": state.tip,
                "sequence": state.commits, "state": "reserved_unknown",
            });
            let chain_digest = digest(&chain_body(&record)?)?;
            record
                .as_object_mut()
                .ok_or_else(|| "cannot build reservation".to_owned())?
                .insert("chain_digest".to_owned(), Value::String(chain_digest));
            records.insert(format!("reservation-{allocation}.json"), record);
        }
    }
    if state.commits >= MAX_HISTORY as u64 {
        return Err("ledger history is at its bound".to_owned());
    }
    let commit = append(api, &state.tip, &records)?;
    Ok(
        json!({"commit": commit, "allocations": records.values().map(|record| get_string(record, "allocation_id").map(str::to_owned)).collect::<Result<Vec<_>>>()?}),
    )
}

fn append<A: Api>(api: &mut A, parent: &str, files: &BTreeMap<String, Value>) -> Result<String> {
    let commit = api.create_commit(files, Some(parent))?;
    let current = api.reference()?;
    if current
        .as_ref()
        .and_then(|value| value.pointer("/object/sha"))
        .and_then(Value::as_str)
        != Some(parent)
    {
        return Err("ledger conflict before ref update".to_owned());
    }
    api.cas_ref(Some(parent), &commit, false)?;
    Ok(commit)
}

fn verify<A: Api>(api: &mut A, args: &Request) -> Result<(Value, VerifiedState)> {
    let reservation_commit = args
        .reservation_commit
        .as_deref()
        .ok_or_else(|| "--reservation-commit is required".to_owned())?;
    require_sha(reservation_commit, "reservation commit")?;
    let candidate_sha = args
        .candidate_sha
        .as_deref()
        .ok_or_else(|| "--candidate-sha is required".to_owned())?;
    require_sha(candidate_sha, "candidate sha")?;
    let provider = args
        .provider
        .as_deref()
        .ok_or_else(|| "--provider is required".to_owned())?;
    let os_name = args
        .os_name
        .as_deref()
        .ok_or_else(|| "--os is required".to_owned())?;
    if !PROVIDERS.contains(&provider) || !OSES.contains(&os_name) {
        return Err("provider or OS lane is invalid".to_owned());
    }
    let state = load_verified(api, Some(reservation_commit))?;
    if get_string(&state.campaign, "campaign_id")? != args.campaign_id {
        return Err("campaign differs".to_owned());
    }
    let allocation = allocation_id(provider, os_name, candidate_sha, args.run_id, args.attempt);
    let record = state
        .allocations
        .get(&allocation)
        .ok_or_else(|| "exact durable allocation is absent".to_owned())?;
    if get_string(record, "candidate_sha")? != candidate_sha
        || record.get("sequence").and_then(Value::as_u64) != Some(state.commits - 1)
    {
        return Err("exact durable allocation is absent".to_owned());
    }
    Ok((
        json!({"reservation_commit": reservation_commit, "allocation_id": allocation, "root_commit": state.root}),
        state,
    ))
}

fn export_state<A: Api>(api: &mut A, args: &Request) -> Result<Value> {
    let (checked, state) = verify(api, args)?;
    let provider = args
        .provider
        .as_deref()
        .ok_or_else(|| "--provider is required".to_owned())?;
    let state_dir = args
        .state_dir
        .as_deref()
        .ok_or_else(|| "--state-dir is required".to_owned())?;
    let directory = create_private_state_directory(state_dir)?;
    let campaign_id = get_string(&state.campaign, "campaign_id")?;
    write_state(
        &directory,
        "campaign.json",
        &json!({"schema": "kio.provider-budget-campaign/v1", "campaign_id": campaign_id, "provider": provider, "cap_microusd": CAP}),
    )?;
    for (allocation, record) in &state.allocations {
        if get_string(record, "provider")? != provider {
            continue;
        }
        write_state(
            &directory,
            &format!("reservation-{allocation}.json"),
            &json!({
                "schema": "kio.provider-budget-reservation/v1", "campaign_id": campaign_id,
                "candidate_sha": get_string(record, "candidate_sha")?, "provider": provider,
                "allocation_id": allocation, "allocated_microusd": ALLOCATION, "state": "reserved_unknown",
            }),
        )?;
    }
    let mut result = checked
        .as_object()
        .cloned()
        .ok_or_else(|| "cannot build export result".to_owned())?;
    result.insert(
        "state_dir".to_owned(),
        Value::String(state_dir.display().to_string()),
    );
    Ok(Value::Object(result))
}

fn create_private_state_directory(path: &Path) -> Result<StoreDirectory> {
    if !path.is_absolute() {
        return Err("state directory must be absolute".to_owned());
    }
    let parent_path = path
        .parent()
        .ok_or_else(|| "state directory has no parent".to_owned())?;
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or_else(|| "state directory must name one new child".to_owned())?;
    let parent = kio_core::private_fs::verify_private_creation_parent(parent_path)
        .map_err(|error| format!("state directory parent is unsafe: {error}"))?;
    let handle = parent
        .create_directory(Path::new(leaf))
        .map_err(|error| format!("cannot create state directory: {error}"))?;
    restrict_new_private_directory(&handle)
        .map_err(|error| format!("cannot secure state directory: {error}"))?;
    StoreDirectory::from_retained(handle, path.to_owned())
        .map_err(|error| format!("cannot retain state directory: {error}"))
}

fn write_state(directory: &StoreDirectory, leaf: &str, value: &Value) -> Result<()> {
    directory
        .write_atomic(Path::new(leaf), &canonical(value)?, Publication::CreateOnly)
        .map_err(|error| format!("cannot export provider budget state: {error}"))
}

fn allocation_id(
    provider: &str,
    os_name: &str,
    candidate_sha: &str,
    run_id: u64,
    attempt: u64,
) -> String {
    format!(
        "{provider}-{os_name}-{}-r{run_id}-a{attempt}",
        &candidate_sha[..12]
    )
}

fn require_positive(value: u64, label: &str) -> Result<()> {
    if value == 0 {
        return Err(format!("invalid {label}"));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use super::*;

    #[test]
    fn ref_update_response_names_the_updated_commit() {
        let response = json!({
            "ref": format!("refs/heads/{BRANCH}"),
            "object": {"type": "commit", "sha": "a".repeat(40)},
        });
        assert_eq!(ref_sha(&response), Some("a".repeat(40).as_str()));
        assert_eq!(ref_sha(&json!({"object": {}})), None);
    }

    #[derive(Default)]
    struct FakeApi {
        head: Option<String>,
        commits: BTreeMap<String, Value>,
        trees: BTreeMap<String, Vec<Value>>,
        blobs: BTreeMap<String, Value>,
        counter: u64,
        conflict: bool,
    }
    impl Api for FakeApi {
        fn reference(&mut self) -> Result<Option<Value>> {
            Ok(self
                .head
                .as_ref()
                .map(|head| json!({"object": {"sha": head}})))
        }
        fn commit(&mut self, oid: &str) -> Result<Value> {
            self.commits
                .get(oid)
                .cloned()
                .ok_or_else(|| "missing fake commit".to_owned())
        }
        fn tree(&mut self, oid: &str) -> Result<Vec<Value>> {
            self.trees
                .get(oid)
                .cloned()
                .ok_or_else(|| "missing fake tree".to_owned())
        }
        fn blob(&mut self, oid: &str) -> Result<Value> {
            self.blobs
                .get(oid)
                .cloned()
                .ok_or_else(|| "missing fake blob".to_owned())
        }
        fn create_commit(
            &mut self,
            files: &BTreeMap<String, Value>,
            parent: Option<&str>,
        ) -> Result<String> {
            self.counter += 1;
            let oid = format!("{:040x}", self.counter);
            let tree = format!("{:040x}", self.counter + 1000);
            let mut entries = Vec::new();
            for (number, (path, value)) in files.iter().enumerate() {
                let blob = format!("{:040x}", self.counter * 100 + number as u64 + 1);
                self.blobs.insert(
                    blob.clone(),
                    json!({"encoding":"base64", "content": BASE64.encode(canonical(value)?)}),
                );
                entries.push(json!({"path":path,"mode":"100644","type":"blob","sha":blob}));
            }
            self.trees.insert(tree.clone(), entries);
            self.commits.insert(oid.clone(), json!({"tree":{"sha":tree},"parents":parent.map(|value| vec![json!({"sha":value})]).unwrap_or_default()}));
            Ok(oid)
        }
        fn cas_ref(&mut self, old: Option<&str>, new: &str, create: bool) -> Result<()> {
            if self.conflict {
                self.head = Some("f".repeat(40));
                return Err("ledger conflict before ref update".to_owned());
            }
            if (create && self.head.is_some()) || (!create && self.head.as_deref() != old) {
                return Err("stale".to_owned());
            }
            self.head = Some(new.to_owned());
            Ok(())
        }
    }
    fn args() -> Request {
        Request {
            campaign_id: "campaign-1".to_owned(),
            candidate_sha: Some("a".repeat(40)),
            workflow_sha256: Some("b".repeat(64)),
            run_id: 42,
            attempt: 3,
            reservation_commit: None,
            provider: Some("mistral".to_owned()),
            os_name: Some("linux".to_owned()),
            state_dir: None,
        }
    }
    fn successor() -> Request {
        let mut request = args();
        request.campaign_id = "campaign-2".to_owned();
        request.workflow_sha256 = Some("c".repeat(64));
        request.run_id += 1;
        request
    }

    #[test]
    fn transition_preserves_history_and_binds_successor_reservations() {
        let mut api = FakeApi::default();
        initialize(&mut api, &args()).unwrap();
        let first = reserve_all(&mut api, &args()).unwrap();
        let predecessor_tip = first["commit"].as_str().unwrap();
        let (commits, trees, blobs) = (api.commits.clone(), api.trees.clone(), api.blobs.clone());

        let mut next = successor();
        let transition_commit = transition(&mut api, &next, "campaign-1", predecessor_tip).unwrap();
        let tip = transition_commit["commit"].as_str().unwrap();
        assert_eq!(api.head.as_deref(), Some(tip));
        assert_eq!(
            api.commits[tip]["parents"],
            json!([{"sha": predecessor_tip}])
        );
        for (old, current) in [(&commits, &api.commits), (&blobs, &api.blobs)] {
            for (oid, bytes) in old {
                assert_eq!(&current[oid], bytes);
            }
        }
        for (oid, entries) in &trees {
            assert_eq!(&api.trees[oid], entries);
        }

        let mut closed = args();
        closed.run_id = 99;
        assert!(reserve_all(&mut api, &closed).is_err());
        closed.run_id = args().run_id;
        closed.reservation_commit = Some(predecessor_tip.to_owned());
        assert!(verify(&mut api, &closed).is_ok());

        let reservation = reserve_all(&mut api, &next).unwrap();
        next.reservation_commit = Some(reservation["commit"].as_str().unwrap().to_owned());
        assert!(verify(&mut api, &next).is_ok());
        let successor_run = next.run_id;
        next.run_id = args().run_id;
        assert!(verify(&mut api, &next).is_err());

        next.run_id = successor_run;
        let tree = api.commits[tip]["tree"]["sha"].as_str().unwrap();
        let blob = api.trees[tree][0]["sha"].as_str().unwrap();
        let mut invalid = transition_commit["campaign"].clone();
        invalid["predecessor_tip"] = json!("f".repeat(40));
        api.blobs.get_mut(blob).unwrap()["content"] =
            json!(BASE64.encode(canonical(&invalid).unwrap()));
        assert!(verify(&mut api, &next).unwrap_err().contains("predecessor"));
    }

    #[test]
    fn transition_rejects_non_tip_unknown_and_reused_predecessors() {
        let mut api = FakeApi::default();
        let root = initialize(&mut api, &args()).unwrap();
        reserve_all(&mut api, &args()).unwrap();
        let tip = api.head.clone().unwrap();
        let count = api.counter;
        let next = successor();
        assert!(
            transition(
                &mut api,
                &next,
                "campaign-1",
                root["commit"].as_str().unwrap()
            )
            .is_err()
        );
        assert!(transition(&mut api, &next, "campaign-1", &"f".repeat(40)).is_err());
        assert!(transition(&mut api, &next, "unknown", &tip).is_err());
        assert!(transition(&mut api, &args(), "campaign-1", &tip).is_err());
        assert_eq!(
            (api.head.as_deref(), api.counter),
            (Some(tip.as_str()), count)
        );
        transition(&mut api, &next, "campaign-1", &tip).unwrap();
        assert!(transition(&mut api, &next, "campaign-1", &tip).is_err());
    }

    #[test]
    fn successor_cap_includes_predecessor_unknown_reservations() {
        let mut api = FakeApi::default();
        initialize(&mut api, &args()).unwrap();
        for run_id in 100..132 {
            let mut request = args();
            request.run_id = run_id;
            reserve_all(&mut api, &request).unwrap();
        }
        let tip = api.head.clone().unwrap();
        let mut next = successor();
        transition(&mut api, &next, "campaign-1", &tip).unwrap();
        reserve_all(&mut api, &next).unwrap();
        let state = load_verified(&mut api, None).unwrap();
        assert_eq!(
            state.totals,
            BTreeMap::from(PROVIDERS.map(|provider| (provider.to_owned(), 9_900_000)))
        );
        let count = api.counter;
        next.run_id += 1;
        assert!(reserve_all(&mut api, &next).unwrap_err().contains("cap"));
        assert_eq!(api.counter, count);
    }

    #[test]
    fn bootstrap_reserve_and_verify_exact_six_lanes() {
        let mut api = FakeApi::default();
        let boot = initialize(&mut api, &args()).unwrap();
        let reserved = reserve_all(&mut api, &args()).unwrap();
        assert_eq!(reserved["allocations"].as_array().unwrap().len(), 6);
        let mut verify_args = args();
        verify_args.reservation_commit = Some(reserved["commit"].as_str().unwrap().to_owned());
        let verified = verify(&mut api, &verify_args).unwrap();
        assert_eq!(
            verified.0["allocation_id"],
            "mistral-linux-aaaaaaaaaaaa-r42-a3"
        );
        let state = load_verified(&mut api, verify_args.reservation_commit.as_deref()).unwrap();
        assert_eq!(state.totals["mistral"], 300000);
        assert_eq!(state.totals["gemini"], 300000);
        assert_eq!(state.root, boot["commit"].as_str().unwrap());
        assert_eq!(
            api.trees[api.commits[reserved["commit"].as_str().unwrap()]["tree"]["sha"]
                .as_str()
                .unwrap()]
            .len(),
            6
        );
    }
    #[test]
    fn edited_workflow_reserves_with_provenance_and_verifies_without_hash() {
        let mut api = FakeApi::default();
        initialize(&mut api, &args()).unwrap();
        let mut request = args();
        let workflow_sha256 = "c".repeat(64);
        request.workflow_sha256 = Some(workflow_sha256.clone());
        let reservation = reserve_all(&mut api, &request).unwrap();
        request.reservation_commit = Some(reservation["commit"].as_str().unwrap().to_owned());
        request.workflow_sha256 = None;
        let state = load_verified(&mut api, None).unwrap();
        for record in state.allocations.values() {
            assert_eq!(record["workflow_sha256"], workflow_sha256);
        }
        assert!(verify(&mut api, &request).is_ok());
    }

    #[test]
    fn successor_can_transition_again_but_campaign_id_cannot_be_reused() {
        let mut api = FakeApi::default();
        let root = initialize(&mut api, &args()).unwrap();
        let next = successor();
        let second = transition(
            &mut api,
            &next,
            "campaign-1",
            root["commit"].as_str().unwrap(),
        )
        .unwrap();
        let mut third = successor();
        third.campaign_id = "campaign-3".to_owned();
        let third_commit = transition(
            &mut api,
            &third,
            "campaign-2",
            second["commit"].as_str().unwrap(),
        )
        .unwrap();
        assert!(
            transition(
                &mut api,
                &args(),
                "campaign-3",
                third_commit["commit"].as_str().unwrap()
            )
            .is_err()
        );
        assert_eq!(
            load_verified(&mut api, None).unwrap().campaign["campaign_id"],
            "campaign-3"
        );
    }

    #[test]
    fn replay_is_refused_and_tampered_chain_is_refused() {
        let mut api = FakeApi::default();
        initialize(&mut api, &args()).unwrap();
        let reserved = reserve_all(&mut api, &args()).unwrap();
        assert!(reserve_all(&mut api, &args()).is_err());
        let tree = api.commits[reserved["commit"].as_str().unwrap()]["tree"]["sha"]
            .as_str()
            .unwrap();
        let blob = api.trees[tree]
            .iter()
            .find(|entry| entry["path"] == "reservation-mistral-linux-aaaaaaaaaaaa-r42-a3.json")
            .and_then(|entry| entry["sha"].as_str())
            .unwrap()
            .to_owned();
        let tampered = json!({
            "schema": SCHEMA,
            "kind": "reservation",
            "campaign_id": "campaign-1",
            "candidate_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "provider": "mistral",
            "os": "linux",
            "allocation_id": "mistral-linux-aaaaaaaaaaaa-r42-a3",
            "allocated_microusd": 1,
            "run_id": 42,
            "attempt": 3,
            "predecessor_oid": "0000000000000000000000000000000000000001",
            "sequence": 1,
            "chain_digest": "0".repeat(64),
            "state": "reserved_unknown",
        });
        api.blobs.insert(
            blob,
            json!({"encoding": "base64", "content": BASE64.encode(canonical(&tampered).unwrap())}),
        );
        let mut verify_args = args();
        verify_args.reservation_commit = Some(reserved["commit"].as_str().unwrap().to_owned());
        assert!(verify(&mut api, &verify_args).is_err());
    }
    #[test]
    fn ref_conflict_never_returns_a_reservation() {
        let mut api = FakeApi::default();
        initialize(&mut api, &args()).unwrap();
        api.conflict = true;
        assert!(reserve_all(&mut api, &args()).is_err());
    }
    #[test]
    fn second_candidate_accumulates_until_provider_cap() {
        let mut api = FakeApi::default();
        initialize(&mut api, &args()).unwrap();
        reserve_all(&mut api, &args()).unwrap();
        let mut next = args();
        next.candidate_sha = Some("c".repeat(40));
        next.run_id = 43;
        next.attempt = 1;
        let second = reserve_all(&mut api, &next).unwrap();
        let state = load_verified(&mut api, second["commit"].as_str()).unwrap();
        assert_eq!(state.totals["mistral"], 600000);
        assert_eq!(state.commits, 3);
    }
    #[test]
    fn cap_boundary_refuses_before_creating_a_commit() {
        let mut api = FakeApi::default();
        initialize(&mut api, &args()).unwrap();
        for run_id in 1..34 {
            let mut request = args();
            request.candidate_sha = Some(format!("{run_id:040x}"));
            request.run_id = run_id;
            request.attempt = 1;
            reserve_all(&mut api, &request).unwrap();
        }
        let head = api.head.clone();
        let counter = api.counter;
        let mut request = args();
        request.candidate_sha = Some("e".repeat(40));
        request.run_id = 34;
        request.attempt = 1;
        assert!(reserve_all(&mut api, &request).is_err());
        assert_eq!((api.head, api.counter), (head, counter));
    }
    #[test]
    fn github_wrapped_base64_is_accepted_but_non_ascii_is_not() {
        let encoded = BASE64.encode(br#"{"ok":true}"#);
        assert_eq!(decode_blob(&json!({"encoding":"base64","content":format!("{}\n{}",&encoded[..4],&encoded[4..])})).unwrap(),json!({"ok":true}));
        assert!(
            decode_blob(&json!({"encoding":"base64","content":format!("{encoded}\u{a0}")}))
                .is_err()
        );
    }
    #[test]
    fn malformed_response_is_refused() {
        let mut api = FakeApi {
            head: Some("a".repeat(40)),
            ..Default::default()
        };
        api.commits
            .insert("a".repeat(40), json!({"tree":{},"parents":"not-an-array"}));
        assert!(load_verified(&mut api, None).is_err());
    }
    #[test]
    fn export_state_is_private_and_exact() {
        let mut api = FakeApi::default();
        initialize(&mut api, &args()).unwrap();
        let reserved = reserve_all(&mut api, &args()).unwrap();
        let root = super::super::canonical_tempdir();
        let private = root.path().join("private");
        let _private = super::super::private_fixture_directory(&private);
        let mut request = args();
        request.reservation_commit = Some(reserved["commit"].as_str().unwrap().to_owned());
        request.state_dir = Some(private.join("state"));
        let output = export_state(&mut api, &request).unwrap();
        let state = output["state_dir"].as_str().unwrap();
        assert_eq!(state, private.join("state").display().to_string());
        assert!(
            fs::metadata(Path::new(state).join("campaign.json"))
                .unwrap()
                .is_file()
        );
    }
}
