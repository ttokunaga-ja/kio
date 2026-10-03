//! Offline, create-only materialization of a controller-pinned embedding bundle.
use cap_primitives::fs as cap_fs;
use clap::{Args as ClapArgs, Subcommand};
use kio_core::{
    private_fs::{
        verify_private_creation_parent, verify_private_directory, verify_private_directory_handle,
    },
    store_dir::{StoreDirectory, restrict_new_private_directory},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const SCHEMA: &str = "kio.local-gpu.model-files/v1";
const MODEL: &str = "Qwen/Qwen3-VL-Embedding-2B";
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_MODEL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const BUFFER: usize = 1024 * 1024;
static STAGE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[arg(long)]
    pub manifest: PathBuf,
    #[arg(long)]
    pub expected_manifest_sha: String,
    #[command(subcommand)]
    pub command: Command,
}
#[derive(Subcommand, Debug)]
pub enum Command {
    Verify {
        #[arg(long)]
        bundle: PathBuf,
    },
    Materialize {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        source_root: PathBuf,
    },
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    files: Vec<Entry>,
    model: String,
    revision: String,
    schema: String,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    bytes: u64,
    path: String,
    sha256: String,
    source_relpath: String,
}

pub fn run(args: Args) -> Result<(), String> {
    let bytes = read_regular_path(&args.manifest, MAX_MANIFEST, "model manifest")?;
    if sha256_hex(&bytes) != args.expected_manifest_sha {
        return Err("model manifest differs from controller pin".into());
    }
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid model manifest: {e}"))?;
    let mut canonical = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    canonical.push(b'\n');
    if canonical != bytes {
        return Err("model manifest is not strict canonical JSON".into());
    }
    validate(&manifest)?;
    match args.command {
        Command::Verify { bundle } => verify_bundle(&bundle, &manifest),
        Command::Materialize {
            bundle,
            source_root,
        } => materialize(&source_root, &bundle, &manifest),
    }
}

fn validate(m: &Manifest) -> Result<(), String> {
    if m.schema != SCHEMA || m.model != MODEL || !revision(&m.revision) {
        return Err("unexpected model manifest identity".into());
    }
    if m.files.is_empty() {
        return Err("model manifest has no files".into());
    }
    let mut total = 0u64;
    let mut previous = "";
    let mut names = BTreeSet::new();
    for e in &m.files {
        if !safe_leaf(&e.path)
            || !digest(&e.sha256)
            || !source_path(&e.source_relpath, &m.revision, &e.path)
            || e.path.as_str() <= previous
            || !names.insert(&e.path)
        {
            return Err("unsafe, duplicate, or unsorted model manifest entry".into());
        }
        total = total
            .checked_add(e.bytes)
            .ok_or("model bundle size overflows")?;
        previous = &e.path;
    }
    if total > MAX_MODEL_BYTES {
        return Err("model bundle exceeds 8 GiB budget".into());
    }
    Ok(())
}
fn revision(v: &str) -> bool {
    v.len() == 40
        && v.bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}
fn digest(v: &str) -> bool {
    v.len() == 64
        && v.bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}
fn safe_leaf(v: &str) -> bool {
    !v.is_empty()
        && Path::new(v).components().count() == 1
        && matches!(Path::new(v).components().next(), Some(Component::Normal(_)))
}
fn source_path(v: &str, revision: &str, leaf: &str) -> bool {
    let parts: Vec<_> = Path::new(v).components().collect();
    let names: Vec<_> = parts
        .iter()
        .filter_map(|p| match p {
            Component::Normal(x) => x.to_str(),
            _ => None,
        })
        .collect();
    let prefix: &[&str] = if names.first() == Some(&"hub") {
        &["hub", "models--Qwen--Qwen3-VL-Embedding-2B", "snapshots"]
    } else {
        &["models--Qwen--Qwen3-VL-Embedding-2B", "snapshots"]
    };
    parts.len() == names.len()
        && names.len() == prefix.len() + 2
        && names[..prefix.len()] == *prefix
        && names[prefix.len()] == revision
        && names[prefix.len() + 1] == leaf
}

fn materialize(source_root: &Path, bundle: &Path, m: &Manifest) -> Result<(), String> {
    let source =
        verify_private_directory(source_root).map_err(private_error("model cache root"))?;
    let (parent_path, leaf) = parent_leaf(bundle)?;
    let parent = verify_private_creation_parent(&parent_path)
        .map_err(private_error("model bundle parent"))?;
    if parent
        .contains_entry(Path::new(&leaf))
        .map_err(store_error)?
    {
        return Err("refusing to overwrite existing model bundle".into());
    }
    let stage_name = reserve_stage(&parent, &leaf)?;
    let stage_file = parent
        .create_directory(Path::new(&stage_name))
        .map_err(store_error)?;
    restrict_new_private_directory(&stage_file).map_err(store_error)?;
    let stage = StoreDirectory::from_retained(stage_file, parent.path().join(&stage_name))
        .map_err(store_error)?;
    let result = (|| {
        verify_private_directory_handle(&stage)
            .map_err(private_error("model staging directory"))?;
        for e in &m.files {
            copy_source(&source, &stage, e, &m.revision)?;
        }
        verify_dir(&stage, m)?;
        stage.sync().map_err(store_error)?;
        parent
            .rename_directory_create_only(Path::new(&stage_name), Path::new(&leaf))
            .map_err(|e| format!("atomic create-only publication failed: {e}"))?;
        parent.sync().map_err(store_error)?;
        let published = parent
            .open_directory(Path::new(&leaf))
            .map_err(store_error)?;
        let published = StoreDirectory::from_retained(published, parent.path().join(&leaf))
            .map_err(store_error)?;
        verify_dir(&published, m)
    })();
    // Failed stages are forensic evidence and are never reused or removed.
    result
}
fn reserve_stage(parent: &StoreDirectory, leaf: &str) -> Result<String, String> {
    for _ in 0..32 {
        let name = format!(
            ".{leaf}.stage.{:016x}",
            STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        if !parent
            .contains_entry(Path::new(&name))
            .map_err(store_error)?
        {
            return Ok(name);
        }
    }
    Err("unable to reserve a private model staging directory".into())
}
fn copy_source(
    root: &StoreDirectory,
    stage: &StoreDirectory,
    e: &Entry,
    revision: &str,
) -> Result<(), String> {
    if !source_path(&e.source_relpath, revision, &e.path) {
        return Err("unsafe model source path".into());
    }
    let rel = Path::new(&e.source_relpath);
    let parent_path = rel.parent().ok_or("model source has no parent")?;
    let parent_file = root.open_directory(parent_path).map_err(store_error)?;
    trusted_directory(&parent_file, "cached model ancestor")?;
    let parent = StoreDirectory::from_retained(parent_file, root.path().join(parent_path))
        .map_err(store_error)?;
    let source_leaf = rel.file_name().ok_or("model source has no leaf")?;
    let mut input = parent
        .open_regular_read(Path::new(source_leaf), e.bytes)
        .map_err(store_error)?;
    regular(&input, e.bytes, "cached model file")?;
    let mut options = cap_fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = cap_fs::open(stage.root_handle().as_ref(), Path::new(&e.path), &options)
        .map_err(|x| format!("cannot create staged model file {}: {x}", e.path))?;
    let mut hasher = Sha256::new();
    let mut remaining = e.bytes;
    let mut buffer = vec![0; BUFFER];
    while remaining > 0 {
        let want = remaining.min(BUFFER as u64) as usize;
        let count = input.read(&mut buffer[..want]).map_err(|x| x.to_string())?;
        if count == 0 {
            return Err(format!(
                "cached model file ended before expected size: {}",
                e.path
            ));
        }
        output
            .write_all(&buffer[..count])
            .map_err(|x| x.to_string())?;
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
    }
    if input.read(&mut [0; 1]).map_err(|x| x.to_string())? != 0 {
        return Err(format!("cached model file grew while copied: {}", e.path));
    }
    output.sync_all().map_err(|x| x.to_string())?;
    regular(&input, e.bytes, "cached model file")?;
    private_regular(&output, e.bytes, "staged model file")?;
    if hex_digest(hasher.finalize()) != e.sha256 {
        return Err(format!("cached model hash differs: {}", e.path));
    }
    Ok(())
}
fn verify_bundle(path: &Path, m: &Manifest) -> Result<(), String> {
    let d = verify_private_directory(path).map_err(private_error("model bundle"))?;
    verify_dir(&d, m)
}
fn verify_dir(d: &StoreDirectory, m: &Manifest) -> Result<(), String> {
    verify_private_directory_handle(d).map_err(private_error("model bundle"))?;
    let actual: BTreeSet<_> = d
        .entries(Path::new(""))
        .map_err(store_error)?
        .into_iter()
        .map(|x| x.name)
        .collect();
    let expected: BTreeSet<_> = m.files.iter().map(|x| x.path.clone().into()).collect();
    if actual != expected {
        return Err("model bundle contains missing or foreign entries".into());
    }
    for e in &m.files {
        let mut f = d
            .open_regular_read(Path::new(&e.path), e.bytes)
            .map_err(store_error)?;
        private_regular(&f, e.bytes, "model bundle file")?;
        let actual = digest_file(&mut f, e.bytes, "model bundle file")?;
        private_regular(&f, e.bytes, "model bundle file")?;
        if actual != e.sha256 {
            return Err(format!("model bundle hash differs: {}", e.path));
        }
    }
    Ok(())
}
fn digest_file(f: &mut File, expected: u64, label: &str) -> Result<String, String> {
    let mut h = Sha256::new();
    let mut remain = expected;
    let mut buffer = vec![0; BUFFER];
    while remain > 0 {
        let n = f
            .read(&mut buffer[..remain.min(BUFFER as u64) as usize])
            .map_err(|x| x.to_string())?;
        if n == 0 {
            return Err(format!("{label} ended before expected size"));
        }
        h.update(&buffer[..n]);
        remain -= n as u64;
    }
    if f.read(&mut [0; 1]).map_err(|x| x.to_string())? != 0 {
        return Err(format!("{label} grew while read"));
    }
    Ok(hex_digest(h.finalize()))
}
fn read_regular_path(path: &Path, max: u64, label: &str) -> Result<Vec<u8>, String> {
    let parent = path.parent().ok_or("model manifest has no parent")?;
    let leaf = path
        .file_name()
        .and_then(|x| x.to_str())
        .filter(|x| safe_leaf(x))
        .ok_or("unsafe model manifest path")?;
    let d = StoreDirectory::open(parent).map_err(store_error)?;
    let f = d
        .open_regular_read(Path::new(leaf), max)
        .map_err(store_error)?;
    let size = f.metadata().map_err(|x| x.to_string())?.len();
    let mut out = Vec::with_capacity(size as usize);
    f.take(max + 1)
        .read_to_end(&mut out)
        .map_err(|x| x.to_string())?;
    if out.len() as u64 != size || out.len() as u64 > max {
        return Err(format!("{label} exceeds its bound or changed while read"));
    }
    Ok(out)
}
fn parent_leaf(path: &Path) -> Result<(PathBuf, String), String> {
    let parent = path
        .parent()
        .filter(|x| x.is_absolute())
        .ok_or("model bundle parent must be absolute")?;
    let leaf = path
        .file_name()
        .and_then(|x| x.to_str())
        .filter(|x| safe_leaf(x))
        .ok_or("unsafe model bundle name")?;
    Ok((parent.into(), leaf.into()))
}
fn regular(f: &File, size: u64, label: &str) -> Result<(), String> {
    let m = f.metadata().map_err(|x| x.to_string())?;
    if !m.is_file() || m.len() != size {
        return Err(format!("{label} has unexpected type or size"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if m.nlink() != 1 {
            return Err(format!("{label} must not have multiple links"));
        }
    }
    Ok(())
}
fn private_regular(f: &File, size: u64, label: &str) -> Result<(), String> {
    regular(f, size, label)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = f.metadata().map_err(|error| error.to_string())?;
        let uid = unsafe { libc::geteuid() };
        if metadata.uid() != uid || metadata.mode() & 0o777 != 0o600 {
            return Err(format!("{label} is not owner-private"));
        }
    }
    Ok(())
}
fn trusted_directory(f: &File, label: &str) -> Result<(), String> {
    let m = f.metadata().map_err(|x| x.to_string())?;
    if !m.is_dir() {
        return Err(format!("{label} is not a directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let uid = unsafe { libc::geteuid() };
        if (m.uid() != uid && m.uid() != 0) || m.mode() & 0o022 != 0 {
            return Err(format!("{label} is writable by an untrusted principal"));
        }
    }
    Ok(())
}
fn sha256_hex(b: &[u8]) -> String {
    hex_digest(Sha256::digest(b))
}
fn hex_digest(d: impl AsRef<[u8]>) -> String {
    d.as_ref().iter().map(|x| format!("{x:02x}")).collect()
}
fn store_error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn private_error(label: &'static str) -> impl FnOnce(kio_core::KioError) -> String {
    move |e| format!("{label} is unsafe: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sha(v: &[u8]) -> String {
        sha256_hex(v)
    }
    fn fixture(r: &str) -> Manifest {
        let c = b"{}";
        let w = b"weights";
        Manifest {
            files: vec![
                Entry {
                    bytes: c.len() as u64,
                    path: "config.json".into(),
                    sha256: sha(c),
                    source_relpath: format!(
                        "hub/models--Qwen--Qwen3-VL-Embedding-2B/snapshots/{r}/config.json"
                    ),
                },
                Entry {
                    bytes: w.len() as u64,
                    path: "model.safetensors".into(),
                    sha256: sha(w),
                    source_relpath: format!(
                        "models--Qwen--Qwen3-VL-Embedding-2B/snapshots/{r}/model.safetensors"
                    ),
                },
            ],
            model: MODEL.into(),
            revision: r.into(),
            schema: SCHEMA.into(),
        }
    }
    fn source(root: &Path, r: &str) {
        let directory = StoreDirectory::open(root).unwrap();
        for (p, b) in [
            (
                format!("hub/models--Qwen--Qwen3-VL-Embedding-2B/snapshots/{r}/config.json"),
                b"{}".as_slice(),
            ),
            (
                format!("models--Qwen--Qwen3-VL-Embedding-2B/snapshots/{r}/model.safetensors"),
                b"weights".as_slice(),
            ),
        ] {
            let p = Path::new(&p);
            let parent = directory.create_directory_all(p.parent().unwrap()).unwrap();
            let parent =
                StoreDirectory::from_retained(parent, root.join(p.parent().unwrap())).unwrap();
            parent
                .write_atomic(
                    Path::new(p.file_name().unwrap()),
                    b,
                    kio_core::store_dir::Publication::CreateOnly,
                )
                .unwrap();
        }
    }
    #[test]
    fn materializes_and_refuses_overwrite() {
        let t = super::super::canonical_tempdir();
        let r = "0123456789abcdef0123456789abcdef01234567";
        let c = t.path().join("cache");
        let _cache = super::super::private_fixture_directory(&c);
        source(&c, r);
        let p = t.path().join("models");
        std::fs::create_dir(&p).unwrap();
        let b = p.join("bundle");
        let m = fixture(r);
        materialize(&c, &b, &m).unwrap();
        verify_bundle(&b, &m).unwrap();
        assert!(materialize(&c, &b, &m).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn rejects_linked_source() {
        use std::os::unix::fs::symlink;
        let t = super::super::canonical_tempdir();
        let r = "0123456789abcdef0123456789abcdef01234567";
        let c = t.path().join("cache");
        let _cache = super::super::private_fixture_directory(&c);
        source(&c, r);
        let p = t.path().join("models");
        std::fs::create_dir(&p).unwrap();
        let b = p.join("bundle");
        let m = fixture(r);
        let f = c.join(format!(
            "hub/models--Qwen--Qwen3-VL-Embedding-2B/snapshots/{r}/config.json"
        ));
        let victim = t.path().join("victim");
        std::fs::write(&victim, b"{}").unwrap();
        std::fs::remove_file(&f).unwrap();
        symlink(&victim, &f).unwrap();
        assert!(materialize(&c, &b, &m).is_err());
    }
}
