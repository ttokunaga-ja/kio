//! Offline, fixed-shape GPU controller identity and observation records.
use clap::{Args as ClapArgs, Subcommand};
use kio_core::{
    private_fs::{read_private_file_at, verify_private_directory},
    store_dir::Publication,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

const IDENTITY_SCHEMA: &str = "kio.local-gpu.identity/v1";
const OBSERVED_SCHEMA: &str = "kio.v1.local_gpu.phase_observed.v1";
const MAX_IDENTITY: u64 = 64 * 1024;
const MAX_SOURCE: u64 = 16 * 1024 * 1024;
const MAX_PROXY_BINARY: u64 = 512 * 1024 * 1024;

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    Create(Create),
    Check(Check),
    Observe(Observe),
}

/// Inputs which bind an identity record to the controller's fixed staging.
#[derive(ClapArgs, Debug, Clone)]
pub struct Binding {
    #[arg(long)]
    pub state_dir: PathBuf,
    #[arg(long)]
    pub controller: PathBuf,
    #[arg(long)]
    pub proxy: PathBuf,
    #[arg(long)]
    pub entrypoint: PathBuf,
    #[arg(long)]
    pub ocr_compose: PathBuf,
    #[arg(long)]
    pub embedding_compose: PathBuf,
    #[arg(long)]
    pub backend_config: PathBuf,
    #[arg(long)]
    pub ca_cert: PathBuf,
    #[arg(long)]
    pub leaf_cert: PathBuf,
    #[arg(long)]
    pub ocr_image: String,
    #[arg(long)]
    pub ocr_revision: String,
    #[arg(long)]
    pub ocr_weight_sha256: String,
    #[arg(long)]
    pub embedding_image: String,
    #[arg(long)]
    pub embedding_revision: String,
    #[arg(long)]
    pub embedding_weight_sha256: String,
}

#[derive(ClapArgs, Debug)]
pub struct Create {
    #[command(flatten)]
    pub binding: Binding,
    #[arg(long)]
    pub run_nonce: String,
}

#[derive(ClapArgs, Debug)]
pub struct Check {
    #[command(flatten)]
    pub binding: Binding,
    #[arg(long)]
    pub staged_proxy: PathBuf,
    #[arg(long)]
    pub staged_entrypoint: PathBuf,
}

#[derive(ClapArgs, Debug)]
pub struct Observe {
    #[arg(long)]
    pub state_dir: PathBuf,
    #[arg(long)]
    pub phase: String,
    #[arg(long)]
    pub first_image_id: String,
    #[arg(long)]
    pub second_image_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Identity {
    schema: String,
    run_nonce: String,
    controller_sha256: String,
    proxy_sha256: String,
    entrypoint_sha256: String,
    ocr_compose_sha256: String,
    embedding_compose_sha256: String,
    backend_config_sha256: String,
    ca_sha256: String,
    leaf_cert_sha256: String,
    ocr_endpoint: String,
    embedding_endpoint: String,
    ocr_image: String,
    ocr_revision: String,
    ocr_weight_sha256: String,
    embedding_image: String,
    embedding_revision: String,
    embedding_weight_sha256: String,
}

#[derive(Debug, Serialize)]
struct Observed {
    schema: &'static str,
    phase: String,
    identity_sha256: String,
    running_image_ids: Vec<String>,
}

pub fn run(args: Args) -> Result<(), String> {
    match args.command {
        Command::Create(args) => create(args),
        Command::Check(args) => check(args),
        Command::Observe(args) => observe(args),
    }
}

fn create(args: Create) -> Result<(), String> {
    if !nonce(&args.run_nonce) {
        return Err("identity run nonce is invalid".into());
    }
    let state = state(&args.binding.state_dir)?;
    let value = identity(&args.binding, args.run_nonce)?;
    let bytes = canonical(&value)?;
    state
        .write_atomic(Path::new("identity.json"), &bytes, Publication::CreateOnly)
        .map_err(|_| "refusing to overwrite controller identity".to_string())?;
    state
        .sync()
        .map_err(|_| "cannot sync controller identity".to_string())
}

fn check(args: Check) -> Result<(), String> {
    let state = state(&args.binding.state_dir)?;
    let bytes = read_private_file_at(&state, "identity.json", MAX_IDENTITY)
        .map_err(|_| "identity is absent or unsafe".to_string())?;
    let actual: Identity =
        serde_json::from_slice(&bytes).map_err(|_| "identity is not JSON".to_string())?;
    if canonical(&actual)? != bytes || !valid_identity(&actual) {
        return Err("identity is not strict canonical JSON".into());
    }
    if digest_file(&args.binding.proxy, MAX_PROXY_BINARY)?
        != digest_file(&args.staged_proxy, MAX_PROXY_BINARY)?
        || digest_file(&args.binding.entrypoint, MAX_SOURCE)?
            != digest_file(&args.staged_entrypoint, MAX_SOURCE)?
    {
        return Err("staged proxy or entrypoint differs from controller source".into());
    }
    let expected = identity(&args.binding, actual.run_nonce.clone())?;
    if actual != expected {
        return Err("identity does not bind current controller staging".into());
    }
    Ok(())
}

fn observe(args: Observe) -> Result<(), String> {
    if !matches!(args.phase.as_str(), "ocr" | "embedding")
        || !image_id(&args.first_image_id)
        || args
            .second_image_id
            .as_deref()
            .is_some_and(|v| !image_id(v))
    {
        return Err("invalid observed phase or image id".into());
    }
    if args.phase == "embedding" && args.second_image_id.is_some() {
        return Err("embedding has one running image".into());
    }
    let state = state(&args.state_dir)?;
    let identity = read_private_file_at(&state, "identity.json", MAX_IDENTITY)
        .map_err(|_| "identity is absent or unsafe".to_string())?;
    let _: Identity = strict_identity(&identity)?;
    let mut running_image_ids = vec![args.first_image_id];
    if let Some(second) = args.second_image_id {
        running_image_ids.push(second);
    }
    let observed = Observed {
        schema: OBSERVED_SCHEMA,
        phase: args.phase.clone(),
        identity_sha256: sha256_id(&identity),
        running_image_ids,
    };
    let mut bytes = canonical(&observed)?;
    bytes.push(b'\n');
    state
        .write_atomic(
            Path::new(&format!("{}-observed.json", args.phase)),
            &bytes,
            Publication::CreateOnly,
        )
        .map_err(|_| "refusing to overwrite observed identity".to_string())?;
    state
        .sync()
        .map_err(|_| "cannot sync observed identity".to_string())
}

fn state(path: &Path) -> Result<kio_core::store_dir::StoreDirectory, String> {
    if !path.is_absolute() {
        return Err("state directory must be absolute".into());
    }
    verify_private_directory(path).map_err(|_| "state directory is not private".to_string())
}

fn identity(b: &Binding, run_nonce: String) -> Result<Identity, String> {
    if !nonce(&run_nonce)
        || !image_digest(&b.ocr_image)
        || b.ocr_revision != "PaddleOCR-VL-1.6"
        || !sha256_id_value(&b.ocr_weight_sha256)
        || !image_digest(&b.embedding_image)
        || !hex(&b.embedding_revision, 40)
        || !sha256_id_value(&b.embedding_weight_sha256)
    {
        return Err("identity properties are invalid".into());
    }
    Ok(Identity {
        schema: IDENTITY_SCHEMA.into(),
        run_nonce,
        controller_sha256: digest_file(&b.controller, MAX_SOURCE)?,
        proxy_sha256: digest_file(&b.proxy, MAX_PROXY_BINARY)?,
        entrypoint_sha256: digest_file(&b.entrypoint, MAX_SOURCE)?,
        ocr_compose_sha256: digest_file(&b.ocr_compose, MAX_SOURCE)?,
        embedding_compose_sha256: digest_file(&b.embedding_compose, MAX_SOURCE)?,
        backend_config_sha256: digest_file(&b.backend_config, MAX_SOURCE)?,
        ca_sha256: digest_file(&b.ca_cert, MAX_SOURCE)?,
        leaf_cert_sha256: digest_file(&b.leaf_cert, MAX_SOURCE)?,
        ocr_endpoint: "https://127.0.0.1:18443".into(),
        embedding_endpoint: "https://127.0.0.1:18444".into(),
        ocr_image: b.ocr_image.clone(),
        ocr_revision: b.ocr_revision.clone(),
        ocr_weight_sha256: b.ocr_weight_sha256.clone(),
        embedding_image: b.embedding_image.clone(),
        embedding_revision: b.embedding_revision.clone(),
        embedding_weight_sha256: b.embedding_weight_sha256.clone(),
    })
}

fn strict_identity(bytes: &[u8]) -> Result<Identity, String> {
    let value: Identity =
        serde_json::from_slice(bytes).map_err(|_| "identity is not JSON".to_string())?;
    if canonical(&value)? != bytes || !valid_identity(&value) {
        return Err("identity is not strict canonical JSON".into());
    }
    Ok(value)
}
fn valid_identity(v: &Identity) -> bool {
    v.schema == IDENTITY_SCHEMA
        && nonce(&v.run_nonce)
        && v.ocr_endpoint == "https://127.0.0.1:18443"
        && v.embedding_endpoint == "https://127.0.0.1:18444"
        && v.ocr_revision == "PaddleOCR-VL-1.6"
        && image_digest(&v.ocr_image)
        && image_digest(&v.embedding_image)
        && hex(&v.embedding_revision, 40)
        && [
            &v.controller_sha256,
            &v.proxy_sha256,
            &v.entrypoint_sha256,
            &v.ocr_compose_sha256,
            &v.embedding_compose_sha256,
            &v.backend_config_sha256,
            &v.ca_sha256,
            &v.leaf_cert_sha256,
            &v.ocr_weight_sha256,
            &v.embedding_weight_sha256,
        ]
        .into_iter()
        .all(|v| sha256_id_value(v))
}
fn digest_file(path: &Path, max_bytes: u64) -> Result<String, String> {
    if !path.is_absolute() {
        return Err("fixed source must be absolute".into());
    }
    let parent = path.parent().ok_or("fixed source has no parent")?;
    let leaf = path.file_name().ok_or("fixed source has no leaf")?;
    let directory = kio_core::store_dir::StoreDirectory::open(parent)
        .map_err(|_| "fixed source is unsafe".to_string())?;
    let mut file = directory
        .open_regular_read(Path::new(leaf), max_bytes)
        .map_err(|_| "fixed source is unsafe".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if file
            .metadata()
            .map_err(|_| "fixed source is unsafe")?
            .nlink()
            != 1
        {
            return Err("fixed source is unsafe".into());
        }
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    let mut total = 0u64;
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|_| "fixed source is unreadable")?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n as u64)
            .ok_or("fixed source too large")?;
        if total > max_bytes {
            return Err("fixed source too large".into());
        }
        digest.update(&buffer[..n]);
    }
    Ok(sha256_id(digest.finalize()))
}
fn canonical<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    serde_jcs::to_vec(value).map_err(|_| "canonical encoding failed".to_string())
}
fn sha256_id(bytes: impl AsRef<[u8]>) -> String {
    let hex: String = bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{hex}")
}
fn nonce(v: &str) -> bool {
    !v.is_empty() && v.len() <= 256 && v.is_ascii()
}
fn property(v: &str) -> bool {
    !v.is_empty() && v.len() <= 512 && v.is_ascii() && !v.bytes().any(|c| c.is_ascii_control())
}
fn hex(v: &str, n: usize) -> bool {
    v.len() == n
        && v.bytes()
            .all(|c| c.is_ascii_digit() || matches!(c, b'a'..=b'f'))
}
fn sha256_id_value(v: &str) -> bool {
    v.strip_prefix("sha256:").is_some_and(|v| hex(v, 64))
}
fn image_id(v: &str) -> bool {
    sha256_id_value(v)
}
fn image_digest(v: &str) -> bool {
    property(v)
        && v.rsplit_once("@sha256:")
            .is_some_and(|(name, digest)| !name.is_empty() && hex(digest, 64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[cfg(unix)]
    fn private(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn fixture(root: &Path) -> (Binding, PathBuf, PathBuf) {
        let state = root.join("state");
        fs::create_dir(&state).unwrap();
        #[cfg(unix)]
        private(&state);
        let source = root.join("source");
        fs::write(&source, b"controller").unwrap();
        let binding = Binding {
            state_dir: state,
            controller: source.clone(),
            proxy: source.clone(),
            entrypoint: source.clone(),
            ocr_compose: source.clone(),
            embedding_compose: source.clone(),
            backend_config: source.clone(),
            ca_cert: source.clone(),
            leaf_cert: source.clone(),
            ocr_image: format!("ocr@sha256:{}", "a".repeat(64)),
            ocr_revision: "PaddleOCR-VL-1.6".into(),
            ocr_weight_sha256: format!("sha256:{}", "0".repeat(64)),
            embedding_image: format!("embedding@sha256:{}", "b".repeat(64)),
            embedding_revision: "0".repeat(40),
            embedding_weight_sha256: format!("sha256:{}", "0".repeat(64)),
        };
        (binding, source.clone(), source)
    }
    #[test]
    fn canonical_identity_rejects_extra_and_bad_digest() {
        let value = Identity {
            schema: IDENTITY_SCHEMA.into(),
            run_nonce: "a".into(),
            controller_sha256:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000".into(),
            proxy_sha256: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .into(),
            entrypoint_sha256:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000".into(),
            ocr_compose_sha256:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000".into(),
            embedding_compose_sha256:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000".into(),
            backend_config_sha256:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000".into(),
            ca_sha256: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .into(),
            leaf_cert_sha256:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000".into(),
            ocr_endpoint: "https://127.0.0.1:18443".into(),
            embedding_endpoint: "https://127.0.0.1:18444".into(),
            ocr_image: format!("ocr@sha256:{}", "a".repeat(64)),
            ocr_revision: "PaddleOCR-VL-1.6".into(),
            ocr_weight_sha256:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000".into(),
            embedding_image: format!("embedding@sha256:{}", "b".repeat(64)),
            embedding_revision: "0000000000000000000000000000000000000000".into(),
            embedding_weight_sha256:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000".into(),
        };
        let bytes = canonical(&value).unwrap();
        assert!(!bytes.ends_with(b"\n"));
        assert_eq!(strict_identity(&bytes).unwrap(), value);
        assert!(strict_identity(b"{\"schema\":\"x\"}").is_err());
        let mut bad = value;
        bad.ca_sha256 = "sha256:no".into();
        assert!(!valid_identity(&bad));
    }
    #[test]
    fn ids_and_bounds_are_strict() {
        assert!(image_id(&format!("sha256:{}", "a".repeat(64))));
        assert!(!image_id("a\n"));
        assert!(hex("abcdef", 6));
        assert!(!hex("ABCDEF", 6));
    }
    #[test]
    fn create_check_and_inconsistent_source_are_deterministic() {
        let temp = super::super::canonical_tempdir();
        let (binding, source, staged) = fixture(temp.path());
        create(Create {
            binding: binding.clone(),
            run_nonce: "nonce".into(),
        })
        .unwrap();
        check(Check {
            binding: binding.clone(),
            staged_proxy: staged.clone(),
            staged_entrypoint: staged.clone(),
        })
        .unwrap();
        fs::write(source, b"changed").unwrap();
        assert!(
            check(Check {
                binding,
                staged_proxy: staged.clone(),
                staged_entrypoint: staged
            })
            .is_err()
        );
    }
}
