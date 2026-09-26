//! Rust-owned construction contract for the sealed macOS comparator runtime.
//!
//! The administrator-only Rust command owns the complete transaction: pin
//! authentication, image creation/attachment, Mach-O closure rewriting,
//! payload re-walking, and manifest publication.
use super::QhardError;
#[cfg(target_os = "macos")]
use super::{RuntimeMountIdentity, observe_runtime_mount};
#[cfg(target_os = "macos")]
use kio_core::cas::hash_bytes;
#[cfg(target_os = "macos")]
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Component, Path},
};

#[cfg(target_os = "macos")]
const MANAGED_ROOT: &str = "/Library/KioComparatorRuntime";
#[cfg(target_os = "macos")]
const RUNTIME_ROOT: &str = "/Library/KioComparatorRuntime/v1";
#[cfg(target_os = "macos")]
const IMAGE_PATH: &str = "/Library/KioComparatorRuntime/v1.dmg";
#[cfg(target_os = "macos")]
const MANIFEST_PATH: &str = "/Library/KioComparatorRuntime/v1.manifest.json";
#[cfg(target_os = "macos")]
const BUILD_PARENT: &str = "/private/tmp";
#[cfg(target_os = "macos")]
const VOLUME_NAME: &str = "KioComparatorRuntime-v1";
#[cfg(target_os = "macos")]
const CANONICAL_EVALUATOR: &str = "/usr/local/bin/kio-eval";

#[cfg(target_os = "macos")]
const CONFIG_BYTES: &[u8] = br#"{"custom_adapters":[]}"#;
#[cfg(target_os = "macos")]
const MAX_FILE_BYTES: u64 = 512 * 1024 * 1024;
#[cfg(target_os = "macos")]
const SYSTEM_PREFIXES: [&str; 2] = ["/usr/lib/", "/System/Library/"];

/// The only public result of the administrator-owned installer.  All paths
/// are fixed production authority, rather than caller-selected options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComparatorRuntimeInstallSummary {
    pub runtime_root: PathBuf,
    pub image: PathBuf,
    pub manifest: PathBuf,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewedPin {
    pub path: &'static str,
    /// Lowercase digest body used only in this compiled reviewed table.
    pub sha256_hex: &'static str,
    pub bytes: u64,
}

// Closed, reviewed input set.  Any version change requires an explicit repin.
const REVIEWED_PINS: &[ReviewedPin] = &[
    ReviewedPin {
        path: "/opt/homebrew/Cellar/fontconfig/2.17.1/lib/libfontconfig.1.dylib",
        sha256_hex: "0a960b13c03e85926cc2fecdd73ea89b352f3a90ce4792b2c2612f224fe7ed48",
        bytes: 304544,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/freetype/2.14.1_2/lib/libfreetype.6.dylib",
        sha256_hex: "9de156e3493b53e42060e91d15627926b1b55e7b854bf1800fecee8ede469d0d",
        bytes: 638192,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/gettext/1.0/lib/libintl.8.dylib",
        sha256_hex: "0c6d618e75fea85cc3d631e164a71766fba9341d19ce1f723300c52e63037c51",
        bytes: 228800,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/gmp/6.3.0/lib/libgmp.10.dylib",
        sha256_hex: "14123464af436d67ef69114810aa9e1e74de50e4097166fe8c110397b3ba6961",
        bytes: 452352,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/gpgme/2.0.1/lib/libgpgme.45.dylib",
        sha256_hex: "69c0e16bee0d16d0ccb68cad0143fef4dbcb47395921d03f89ed611636d07544",
        bytes: 345392,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/gpgmepp/2.0.0/lib/libgpgmepp.7.0.0.dylib",
        sha256_hex: "403f6cd87b492dbdfcea5665b3136734449b596d6b3b045a3cc4cc62388aade3",
        bytes: 414640,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/jpeg-turbo/3.1.3/lib/libjpeg.8.3.2.dylib",
        sha256_hex: "b61e868fffc3c13501417e78d70fafadb4daccad593590f9e96e59f4cefdd20b",
        bytes: 486672,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/libassuan/3.0.2/lib/libassuan.9.dylib",
        sha256_hex: "1c45b3dd61f6f07249149723358e4d8448af5ced1a6b279a99ddbd7a906d1ff6",
        bytes: 116320,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/libgpg-error/1.59/lib/libgpg-error.0.dylib",
        sha256_hex: "a6dded3a14c1adc1465b65b517640bab484012ae37071d87c20fdf87c2262495",
        bytes: 198720,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/libpng/1.6.55/lib/libpng16.16.dylib",
        sha256_hex: "a665b05d0a9fc37b96e6f6651cf1ba182db93bcf7992e73f5e8d5cdbb4700ee6",
        bytes: 208272,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/libtiff/4.7.1_1/lib/libtiff.6.dylib",
        sha256_hex: "f65bfa09fe4b3710e308d53707d081644eede6e57f06df6c376ad7f5bc6ffcb2",
        bytes: 539248,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/little-cms2/2.18/lib/liblcms2.2.dylib",
        sha256_hex: "2b01b3d4983f379da0c7a433b926144340a5210390019f9aaf15c3b3ede6abfa",
        bytes: 372080,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/nspr/4.38.2/lib/libnspr4.dylib",
        sha256_hex: "7f85b5d639f28836895dd93717685cf891def04f1f91d41b6a6f9543297ade6f",
        bytes: 238752,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/nspr/4.38.2/lib/libplc4.dylib",
        sha256_hex: "8945b7af3ae90a3fa1d49482be01ff78f0a1380ca4bb685b59454abb4aae4fe8",
        bytes: 70768,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/nspr/4.38.2/lib/libplds4.dylib",
        sha256_hex: "24627ef67deda78448f7cab363f554b857fae595f3d0cdba86ec97f1bfff1418",
        bytes: 69632,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/nss/3.121/lib/libnss3.dylib",
        sha256_hex: "2bd3c828466d9b6aeb985b62d45e6a77c0dfd4e9177bb72530e80dfcc19f4794",
        bytes: 1174848,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/nss/3.121/lib/libnssutil3.dylib",
        sha256_hex: "7891381b35027b011965293667987ddeef5a2e58cfbab9a589bf09c1a28422cd",
        bytes: 222048,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/nss/3.121/lib/libsmime3.dylib",
        sha256_hex: "ea59d0432a835d3c8a9e8e31b4b3584e26336d2b104c0b7464a3f37caaa21091",
        bytes: 218912,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/nss/3.121/lib/libssl3.dylib",
        sha256_hex: "090acb80d058254c9f9e44c5836334a401d86744991804c3bdf441a9cf4cffb7",
        bytes: 383520,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/openjpeg/2.5.4/lib/libopenjp2.2.5.4.dylib",
        sha256_hex: "3b46324a48881d5ef030a096a5c242d0641299f85576895611ff0deb1505cbca",
        bytes: 324160,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/pandoc/3.10.1/bin/pandoc",
        sha256_hex: "61574e53a089110eae07817b91510ff150e826807ac020aa744e0ade23025e0d",
        bytes: 277080112,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/pcre2/10.47_1/lib/libpcre2-8.0.dylib",
        sha256_hex: "fc0491cc252c2938b6c37d1b6b4d7bfedffb9edb2519c47cef577637eddb73d5",
        bytes: 588224,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/poppler/26.02.0_1/bin/pdftotext",
        sha256_hex: "e75be019b2ab471970560493262458a3b4be1b9f9584d004bb8a624d5487c9b6",
        bytes: 82456,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/poppler/26.02.0_1/lib/libpoppler.157.0.0.dylib",
        sha256_hex: "688a66fbad757086fc64ae2262585953d13a2868f49a7cfadf7f5857297ba371",
        bytes: 3419584,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/ripgrep/15.1.0/bin/rg",
        sha256_hex: "2fb61b6e5b3e2d89b115fe6c18fd8805670fdf4bdfde85954d40855a76830e5f",
        bytes: 6154240,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/ripgrep-all/0.10.10/bin/rga",
        sha256_hex: "279d3f49b1ebf9db88d6f2ab58906bf43182be51df63a3555ade27ba611a9a5c",
        bytes: 7700968,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/ripgrep-all/0.10.10/bin/rga-preproc",
        sha256_hex: "4f583ec9b9edbe5956ad82fd40d3df6876e2d1b084935a44e87a1cc999964196",
        bytes: 9177616,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/xz/5.8.3/lib/liblzma.5.dylib",
        sha256_hex: "3d5bfa2f097c31463642b1daab5e662b44368bb4da368f85e412e7f9adcbaa10",
        bytes: 184512,
    },
    ReviewedPin {
        path: "/opt/homebrew/Cellar/zstd/1.5.7_1/lib/libzstd.1.5.7.dylib",
        sha256_hex: "e2847c4613b386683c234913ae3b7b04299254096caf7616e3b3cd9bb97a39ab",
        bytes: 649648,
    },
];
pub fn reviewed_pins() -> &'static [ReviewedPin] {
    REVIEWED_PINS
}

/// Every externally recorded digest uses the repository-wide tagged form.
#[cfg(target_os = "macos")]
fn reviewed_hash(pin: ReviewedPin) -> String {
    format!("sha256:{}", pin.sha256_hex)
}

#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct FileDigest {
    path: String,
    sha256: String,
    bytes: u64,
}
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Preimage {
    schema_version: u8,
    runtime_root: String,
    config_sha256: String,
    sources_before: Vec<FileDigest>,
    payload_files: Vec<FileDigest>,
    closure_images: Vec<String>,
}
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Serialize)]
struct Manifest {
    #[serde(flatten)]
    preimage: Preimage,
    image_sha256: String,
    sources_after: Vec<FileDigest>,
    image_xattr_policy: String,
    image_allowed_xattrs: Vec<String>,
    image_attach_cache_policy: String,
    runtime_xattr_policy: String,
    runtime_allowed_xattrs: Vec<XattrObservation>,
    manifest_xattr_policy: String,
    manifest_allowed_xattrs: Vec<String>,
}
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct XattrObservation {
    path: String,
    names: Vec<String>,
}
fn err(message: impl Into<String>) -> QhardError {
    QhardError::Input(message.into())
}
#[cfg(target_os = "macos")]
fn is_relative(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
#[cfg(target_os = "macos")]
fn image_xattr_names_allowed(names: &BTreeSet<String>, permit_attach_cache: bool) -> bool {
    let mut allowed = BTreeSet::from([
        "com.apple.FinderInfo".to_owned(),
        "com.apple.provenance".to_owned(),
    ]);
    if permit_attach_cache {
        allowed.insert("com.apple.diskimages.recentcksum".to_owned());
    }
    names.is_subset(&allowed)
}
#[cfg(target_os = "macos")]
fn runtime_root_matches_preimage(runtime_root: &Path, preimage: &Preimage) -> bool {
    runtime_root == Path::new(&preimage.runtime_root)
}
#[cfg(target_os = "macos")]
fn insert_macho_alias<'a>(
    aliases: &mut BTreeMap<String, &'a str>,
    alias: &str,
    pin: &'a str,
) -> Result<(), QhardError> {
    match aliases.entry(alias.to_owned()) {
        std::collections::btree_map::Entry::Vacant(slot) => {
            slot.insert(pin);
            Ok(())
        }
        std::collections::btree_map::Entry::Occupied(slot) if *slot.get() == pin => Ok(()),
        std::collections::btree_map::Entry::Occupied(_) => Err(err("ambiguous LC_ID_DYLIB alias")),
    }
}
#[cfg(target_os = "macos")]
fn digest(path: &Path, maximum: u64) -> Result<(String, u64), QhardError> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let meta = fs::symlink_metadata(path).map_err(|e| err(e.to_string()))?;
    if !meta.file_type().is_file() || meta.len() > maximum {
        return Err(err(format!("unsafe runtime file: {}", path.display())));
    }
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| err(e.to_string()))?;
    let opened = f.metadata().map_err(|e| err(e.to_string()))?;
    if !opened.is_file()
        || opened.dev() != meta.dev()
        || opened.ino() != meta.ino()
        || opened.len() != meta.len()
    {
        return Err(err("runtime file changed while opening for hashing"));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    f.read_to_end(&mut bytes).map_err(|e| err(e.to_string()))?;
    let after = f.metadata().map_err(|e| err(e.to_string()))?;
    let named = fs::symlink_metadata(path).map_err(|e| err(e.to_string()))?;
    if bytes.len() as u64 != meta.len()
        || after.dev() != opened.dev()
        || after.ino() != opened.ino()
        || named.dev() != opened.dev()
        || named.ino() != opened.ino()
    {
        return Err(err("file changed while hashing"));
    }
    Ok((hash_bytes(&bytes), meta.len()))
}
#[cfg(target_os = "macos")]
fn validate(pre: &Preimage) -> Result<(), QhardError> {
    if pre.schema_version != 1
        || pre.runtime_root != "/Library/KioComparatorRuntime/v1"
        || pre.config_sha256 != hash_bytes(CONFIG_BYTES)
    {
        return Err(err("invalid runtime preimage header"));
    }
    let want = REVIEWED_PINS
        .iter()
        .map(|p| (p.path, reviewed_hash(*p), p.bytes))
        .collect::<BTreeSet<_>>();
    let got = pre
        .sources_before
        .iter()
        .map(|p| (p.path.as_str(), p.sha256.clone(), p.bytes))
        .collect::<BTreeSet<_>>();
    if want != got
        || pre.payload_files.is_empty()
        || pre.payload_files.len() > 128
        || pre.closure_images.len() != REVIEWED_PINS.len()
    {
        return Err(err("preimage pin or cardinality mismatch"));
    }
    let mut paths = BTreeSet::new();
    for p in &pre.payload_files {
        if !is_relative(&p.path)
            || p.bytes > MAX_FILE_BYTES
            || p.sha256.len() != 71
            || !p.sha256.starts_with("sha256:")
            || !p.sha256[7..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || !paths.insert(&p.path)
        {
            return Err(err("invalid preimage payload entry"));
        }
    }
    let closure = pre.closure_images.iter().collect::<BTreeSet<_>>();
    if closure.len() != pre.closure_images.len()
        || pre.closure_images.iter().any(|p| !is_relative(p))
        || pre.closure_images.iter().any(|p| !paths.contains(p))
    {
        return Err(err("invalid closure image path"));
    }
    Ok(())
}

pub fn install_comparator_runtime() -> Result<ComparatorRuntimeInstallSummary, QhardError> {
    #[cfg(target_os = "macos")]
    {
        macos::install()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(err("comparator runtime installation requires macOS"))
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use crate::runner::{BoundedProcessOptions, DEFAULT_PROCESS_TIMEOUT, run_bounded_command};
    use std::{
        ffi::CString,
        os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        os::unix::io::{AsRawFd, FromRawFd},
        os::unix::process::CommandExt,
        process::Command,
    };
    const SEEDS: [(&str, &str); 5] = [
        ("rga", "/opt/homebrew/Cellar/ripgrep-all/0.10.10/bin/rga"),
        (
            "rga-preproc",
            "/opt/homebrew/Cellar/ripgrep-all/0.10.10/bin/rga-preproc",
        ),
        ("pandoc", "/opt/homebrew/Cellar/pandoc/3.10.1/bin/pandoc"),
        (
            "pdftotext",
            "/opt/homebrew/Cellar/poppler/26.02.0_1/bin/pdftotext",
        ),
        ("rg", "/opt/homebrew/Cellar/ripgrep/15.1.0/bin/rg"),
    ];
    #[derive(Default)]
    pub(super) struct Macho {
        pub(super) loads: Vec<String>,
        pub(super) rpaths: Vec<String>,
        ids: Vec<String>,
        loaders: Vec<String>,
        environment: bool,
    }
    pub(super) fn command(bin: &str, args: &[&str]) -> Result<Vec<u8>, QhardError> {
        let mut child = Command::new(bin);
        child
            .args(args)
            .env_clear()
            .env("HOME", "/var/root")
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .env("TZ", "UTC");
        // hdiutil creates the image itself.  A fixed restrictive umask closes
        // the creation-to-chmod window even when the invoking root process
        // inherited a permissive umask.
        unsafe {
            child.pre_exec(|| {
                libc::umask(0o077);
                Ok(())
            });
        }
        let o = run_bounded_command(
            &mut child,
            BoundedProcessOptions {
                timeout: DEFAULT_PROCESS_TIMEOUT,
                max_stdout_bytes: 524_288,
                max_stderr_bytes: 524_288,
            },
            None,
        )
        .map_err(|e| err(format!("cannot run {bin}: {e}")))?;
        if !o.status.success() {
            return Err(err(format!("{bin} failed or emitted excessive output")));
        }
        Ok(o.stdout.into_bytes())
    }
    pub(super) fn parse_otool(bytes: &[u8]) -> Result<Macho, QhardError> {
        let text = std::str::from_utf8(bytes).map_err(|_| err("non-UTF8 otool output"))?;
        let mut m = Macho::default();
        let mut current = "";
        for line in text.lines() {
            let l = line.trim();
            if let Some(v) = l.strip_prefix("cmd ") {
                current = v;
                if v == "LC_DYLD_ENVIRONMENT" {
                    m.environment = true
                }
                continue;
            }
            let value = if current == "LC_RPATH" {
                l.strip_prefix("path ")
            } else {
                l.strip_prefix("name ")
            };
            let Some(v) = value.and_then(|x| x.split(" (offset ").next()) else {
                continue;
            };
            match current {
                "LC_LOAD_DYLIB"
                | "LC_LOAD_WEAK_DYLIB"
                | "LC_REEXPORT_DYLIB"
                | "LC_LOAD_UPWARD_DYLIB"
                | "LC_LAZY_LOAD_DYLIB" => m.loads.push(v.into()),
                "LC_ID_DYLIB" => m.ids.push(v.into()),
                "LC_LOAD_DYLINKER" => m.loaders.push(v.into()),
                "LC_RPATH" => m.rpaths.push(v.into()),
                _ => {}
            }
        }
        if m.loads.len() > 256 || m.rpaths.len() > 64 || m.ids.len() > 1 || m.loaders.len() > 1 {
            return Err(err("malformed Mach-O metadata"));
        }
        Ok(m)
    }
    fn macho(p: &Path) -> Result<Macho, QhardError> {
        parse_otool(&command(
            "/usr/bin/otool",
            &["-arch", "arm64", "-l", &p.to_string_lossy()],
        )?)
    }
    fn base(p: &str) -> Result<&str, QhardError> {
        Path::new(p)
            .file_name()
            .and_then(|x| x.to_str())
            .filter(|x| !x.is_empty())
            .ok_or_else(|| err("invalid Mach-O basename"))
    }
    fn system(p: &str) -> bool {
        p == "/usr/lib/dyld" || SYSTEM_PREFIXES.iter().any(|x| p.starts_with(x))
    }
    fn policy(m: &Macho, seed: bool) -> Result<(), QhardError> {
        if m.environment
            || m.ids.len() > 1
            || (seed && m.loaders != ["/usr/lib/dyld"])
            || (!seed && !m.loaders.is_empty())
        {
            Err(err("unsafe Mach-O loader metadata"))
        } else {
            Ok(())
        }
    }
    fn stage(pin: ReviewedPin, to: &Path) -> Result<(), QhardError> {
        let source = Path::new(pin.path);
        if fs::canonicalize(source).map_err(|e| err(e.to_string()))? != source {
            return Err(err("reviewed pin is not canonical"));
        }
        let mut input = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(source)
            .map_err(|e| err(e.to_string()))?;
        let before = input.metadata().map_err(|e| err(e.to_string()))?;
        if !before.is_file() || before.len() != pin.bytes {
            return Err(err("reviewed pin type/size changed"));
        }
        let mut bytes = Vec::with_capacity(pin.bytes as usize);
        input
            .read_to_end(&mut bytes)
            .map_err(|e| err(e.to_string()))?;
        let after = input.metadata().map_err(|e| err(e.to_string()))?;
        let named = fs::symlink_metadata(source).map_err(|e| err(e.to_string()))?;
        if before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.dev() != named.dev()
            || before.ino() != named.ino()
            || bytes.len() as u64 != pin.bytes
            || hash_bytes(&bytes) != reviewed_hash(pin)
        {
            return Err(err("reviewed pin changed during nofollow copy"));
        }
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(to)
            .map_err(|e| err(e.to_string()))?;
        output
            .write_all(&bytes)
            .and_then(|_| output.sync_all())
            .map_err(|e| err(e.to_string()))?;
        Ok(())
    }
    fn files(root: &Path) -> Result<Vec<PathBuf>, QhardError> {
        let mut stack = vec![root.to_owned()];
        let mut answer = Vec::new();
        while let Some(dir) = stack.pop() {
            let mut entries = fs::read_dir(dir)
                .map_err(|e| err(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| err(e.to_string()))?;
            entries.sort_by_key(|x| x.file_name());
            for e in entries {
                let p = e.path();
                let t = fs::symlink_metadata(&p)
                    .map_err(|e| err(e.to_string()))?
                    .file_type();
                if t.is_symlink() {
                    return Err(err("symlink in payload"));
                }
                if t.is_dir() {
                    stack.push(p)
                } else if t.is_file() {
                    answer.push(p)
                } else {
                    return Err(err("nonregular payload entry"));
                }
            }
        }
        answer.sort();
        Ok(answer)
    }
    fn rewalk(root: &Path, expected: &BTreeSet<String>) -> Result<Vec<FileDigest>, QhardError> {
        let mut names = BTreeMap::new();
        let mut result = Vec::new();
        for f in files(root)? {
            let rel = f
                .strip_prefix(root)
                .map_err(|_| err("payload escape"))?
                .to_string_lossy()
                .to_string();
            let (x, n) = digest(&f, MAX_FILE_BYTES)?;
            result.push(FileDigest {
                path: rel.clone(),
                sha256: x,
                bytes: n,
            });
            if (rel.starts_with("bin/") || rel.starts_with("lib/"))
                && names.insert(base(&rel)?.to_owned(), rel).is_some()
            {
                return Err(err("payload basename collision"));
            }
        }
        if names.values().cloned().collect::<BTreeSet<_>>() != *expected
            || fs::read(root.join("config/rga-config.json")).map_err(|e| err(e.to_string()))?
                != CONFIG_BYTES
        {
            return Err(err("payload differs from sealed construction contract"));
        }
        for rel in names.values() {
            let m = macho(&root.join(rel))?;
            let is_bin = rel.starts_with("bin/");
            policy(&m, is_bin)?;
            if m.rpaths
                != [if is_bin {
                    "@loader_path/../lib"
                } else {
                    "@loader_path"
                }]
            {
                return Err(err("unexpected payload rpath"));
            }
            for load in m.loads {
                if system(&load) {
                    continue;
                }
                let name = load
                    .strip_prefix("@rpath/")
                    .filter(|x| !x.contains('/'))
                    .ok_or_else(|| err("unsealed payload dependency"))?;
                if !names.get(name).is_some_and(|p| p.starts_with("lib/")) {
                    return Err(err("unresolved payload dependency"));
                }
            }
        }
        result.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(result)
    }
    fn image_xattrs(image: &Path) -> Result<Vec<String>, QhardError> {
        super::super::require_no_extended_acl(image, "comparator runtime image")?;
        let mut names = super::super::macos_xattr::list(image, false)?;
        if names.contains("com.apple.diskimages.recentcksum") {
            if !image_xattr_names_allowed(&names, true) {
                return Err(err("runtime image has forbidden extended attributes"));
            }
            super::super::macos_xattr::remove_named(image, "com.apple.diskimages.recentcksum")?;
            names = super::super::macos_xattr::list(image, false)?;
        }
        if !image_xattr_names_allowed(&names, false) {
            return Err(err("runtime image has forbidden extended attributes"));
        }
        Ok(names.into_iter().collect())
    }
    fn runtime_xattrs(root: &Path) -> Result<Vec<XattrObservation>, QhardError> {
        let mut paths = vec![root.to_path_buf()];
        paths.extend(files(root)?);
        let mut observed = Vec::new();
        for path in paths {
            let names = super::super::macos_xattr::list(&path, false)?;
            if !super::super::runtime_xattr_names_allowed(&names) {
                return Err(err("mounted runtime has forbidden extended attributes"));
            }
            if !names.is_empty() {
                observed.push(XattrObservation {
                    path: path
                        .strip_prefix(root)
                        .ok()
                        .and_then(|p| {
                            (!p.as_os_str().is_empty()).then(|| p.to_string_lossy().to_string())
                        })
                        .unwrap_or_else(|| ".".into()),
                    names: names.into_iter().collect(),
                });
            }
        }
        observed.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(observed)
    }
    fn manifest_xattrs(path: &Path) -> Result<Vec<String>, QhardError> {
        super::super::require_no_extended_acl(path, "comparator runtime manifest")?;
        let names = super::super::macos_xattr::list(path, false)?;
        if !super::super::runtime_xattr_names_allowed(&names) {
            return Err(err("runtime manifest has forbidden extended attributes"));
        }
        Ok(names.into_iter().collect())
    }
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Identity {
        dev: u64,
        ino: u64,
        uid: u32,
        gid: u32,
        mode: u32,
    }
    struct RetainedAuthority {
        path: PathBuf,
        identity: Identity,
        descriptor: fs::File,
    }
    fn identity_metadata(metadata: &fs::Metadata) -> Identity {
        Identity {
            dev: metadata.dev(),
            ino: metadata.ino(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            mode: metadata.mode(),
        }
    }
    fn identity(path: &Path) -> Result<Identity, QhardError> {
        let metadata = fs::symlink_metadata(path).map_err(|e| err(e.to_string()))?;
        Ok(identity_metadata(&metadata))
    }
    fn unchanged(path: &Path, expected: Identity) -> bool {
        identity(path).is_ok_and(|actual| actual == expected)
    }
    /// Every component from `anchor` to `target` is authority.  Binding only
    /// the leaf permits a post-check rename by a writable ancestor, so retain
    /// nofollow descriptors and exact identities for the whole namespace.
    fn require_authority_from(
        anchor: &Path,
        target: &Path,
        expected_uid: u32,
        expected_gid: u32,
        label: &str,
    ) -> Result<Vec<RetainedAuthority>, QhardError> {
        if !anchor.is_absolute()
            || !target.is_absolute()
            || fs::canonicalize(anchor).map_err(|e| err(e.to_string()))? != anchor
            || fs::canonicalize(target).map_err(|e| err(e.to_string()))? != target
        {
            return Err(err(format!("{label} is not canonical")));
        }
        let descendants = target
            .strip_prefix(anchor)
            .map_err(|_| err(format!("{label} is outside its authority anchor")))?;
        let components = descendants
            .components()
            .map(|component| match component {
                Component::Normal(name) => Ok(name),
                _ => Err(err(format!("{label} has an invalid authority component"))),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if components.is_empty() {
            return Err(err(format!("{label} must not equal its authority anchor")));
        }

        fn retain(
            path: &Path,
            expected_uid: u32,
            expected_gid: u32,
            directory: bool,
            label: &str,
        ) -> Result<RetainedAuthority, QhardError> {
            let metadata = fs::symlink_metadata(path).map_err(|e| err(e.to_string()))?;
            if metadata.file_type().is_symlink()
                || metadata.uid() != expected_uid
                || metadata.gid() != expected_gid
                || metadata.mode() & 0o022 != 0
                || (directory && !metadata.is_dir())
                || (!directory && !metadata.is_file())
            {
                return Err(err(format!(
                    "{label} has unsafe authority component: {}",
                    path.display()
                )));
            }
            super::super::require_no_extended_acl(path, label)?;
            let descriptor = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | if directory { libc::O_DIRECTORY } else { 0 })
                .open(path)
                .map_err(|e| err(format!("cannot retain {label} authority component: {e}")))?;
            let named = identity(path)?;
            if identity_metadata(&descriptor.metadata().map_err(|e| err(e.to_string()))?) != named {
                return Err(err(format!(
                    "{label} authority changed while binding: {}",
                    path.display()
                )));
            }
            Ok(RetainedAuthority {
                path: path.to_owned(),
                identity: named,
                descriptor,
            })
        }

        let mut current = anchor.to_owned();
        let mut retained = vec![retain(&current, expected_uid, expected_gid, true, label)?];
        for (index, component) in components.iter().enumerate() {
            current.push(component);
            retained.push(retain(
                &current,
                expected_uid,
                expected_gid,
                index + 1 != components.len(),
                label,
            )?);
        }
        recheck_authority(&retained, label)?;
        Ok(retained)
    }
    /// The evaluator itself and every namespace component used to reach it
    /// are production authority, rooted at the system namespace.
    fn require_root_authority(
        path: &Path,
        label: &str,
    ) -> Result<Vec<RetainedAuthority>, QhardError> {
        require_authority_from(Path::new("/"), path, 0, 0, label)
    }
    fn recheck_authority(retained: &[RetainedAuthority], label: &str) -> Result<(), QhardError> {
        for component in retained {
            if identity_metadata(
                &component
                    .descriptor
                    .metadata()
                    .map_err(|e| err(e.to_string()))?,
            ) != component.identity
                || !unchanged(&component.path, component.identity)
            {
                return Err(err(format!(
                    "{label} authority changed: {}",
                    component.path.display()
                )));
            }
            super::super::require_no_extended_acl(&component.path, label)?;
        }
        Ok(())
    }
    /// Create a directory only through a descriptor for its already-bound
    /// parent.  The named path is used solely to prove that the object opened
    /// through that descriptor is the object visible to subsequent fixed-path
    /// system tools; a replacement at either point is indeterminate.
    fn mkdirat_bound(
        parent: &RetainedAuthority,
        name: &str,
        path: &Path,
        _expected_uid: u32,
        _expected_gid: u32,
        label: &str,
    ) -> Result<RetainedAuthority, QhardError> {
        if name.is_empty() || name.bytes().any(|byte| byte == 0 || byte == b'/') {
            return Err(err("invalid capability-relative directory name"));
        }
        if identity_metadata(
            &parent
                .descriptor
                .metadata()
                .map_err(|e| err(e.to_string()))?,
        ) != parent.identity
            || !unchanged(&parent.path, parent.identity)
        {
            return Err(QhardError::Indeterminate(format!(
                "{label} parent changed before mkdirat"
            )));
        }
        // ACLs are not represented in stat identity.  Check them immediately
        // before this capability-relative mutation so an inheritable ACL
        // cannot be used after the parent was initially bound.
        super::super::require_no_extended_acl(&parent.path, label)?;
        let name = CString::new(name).map_err(|_| err("invalid directory name"))?;
        // SAFETY: `name` is NUL-free and the retained descriptor refers to a
        // directory that was opened with O_NOFOLLOW.
        if unsafe { libc::mkdirat(parent.descriptor.as_raw_fd(), name.as_ptr(), 0o755) } != 0 {
            return Err(err(format!(
                "cannot create {label}: {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: this opens the just-created child relative to the retained
        // parent descriptor.  `name` is NUL-free and no pathname traversal is
        // involved.  `File` takes ownership of the successful descriptor.
        let raw = unsafe {
            libc::openat(
                parent.descriptor.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            let open_error = std::io::Error::last_os_error();
            // Even though mkdirat returned success, no child identity was
            // retained.  A replacement cannot be excluded, so preserve the
            // fixed name instead of guessing that it is ours to unlink.
            return Err(QhardError::Indeterminate(format!(
                "cannot retain created {label}; preserving unowned name: {}",
                open_error
            )));
        }
        // SAFETY: `raw` is a newly returned owned descriptor above.
        let descriptor = unsafe { fs::File::from_raw_fd(raw) };
        let named = identity(path)?;
        let opened = identity_metadata(&descriptor.metadata().map_err(|e| err(e.to_string()))?);
        if opened != named {
            return Err(QhardError::Indeterminate(format!(
                "created {label} changed while binding"
            )));
        }
        if identity_metadata(
            &parent
                .descriptor
                .metadata()
                .map_err(|e| err(e.to_string()))?,
        ) != parent.identity
            || !unchanged(&parent.path, parent.identity)
            || !unchanged(path, named)
        {
            return Err(QhardError::Indeterminate(format!(
                "{label} changed after mkdirat"
            )));
        }
        Ok(RetainedAuthority {
            path: path.to_owned(),
            identity: named,
            descriptor,
        })
    }
    fn recheck_created(component: &RetainedAuthority, label: &str) -> Result<(), QhardError> {
        let descriptor = identity_metadata(
            &component
                .descriptor
                .metadata()
                .map_err(|e| err(e.to_string()))?,
        );
        if descriptor != component.identity || !unchanged(&component.path, component.identity) {
            return Err(QhardError::Indeterminate(format!(
                "{label} changed after creation"
            )));
        }
        super::super::require_no_extended_acl(&component.path, label)?;
        let xattrs = super::super::macos_xattr::list(&component.path, false)?;
        // macOS may add provenance while creating a directory.  It is the
        // sole permitted annotation; any inherited/user-controlled name is a
        // hard failure and is rechecked at every transaction boundary.
        if !super::super::runtime_xattr_names_allowed(&xattrs) {
            return Err(QhardError::Indeterminate(format!(
                "{label} acquired forbidden extended attributes"
            )));
        }
        Ok(())
    }
    fn validate_created(
        component: &RetainedAuthority,
        expected_uid: u32,
        expected_gid: u32,
        label: &str,
    ) -> Result<(), QhardError> {
        let metadata = fs::symlink_metadata(&component.path).map_err(|e| err(e.to_string()))?;
        if !metadata.is_dir()
            || metadata.uid() != expected_uid
            || metadata.gid() != expected_gid
            || metadata.mode() & 0o022 != 0
        {
            return Err(QhardError::Indeterminate(format!(
                "created {label} has unsafe ownership or mode"
            )));
        }
        recheck_created(component, label)
    }
    fn sync_retained_parent(parent: &(Identity, fs::File)) -> Result<(), QhardError> {
        if identity_metadata(&parent.1.metadata().map_err(|e| err(e.to_string()))?) != parent.0 {
            return Err(QhardError::Indeterminate(
                "publication parent identity changed".into(),
            ));
        }
        parent.1.sync_all().map_err(|e| err(e.to_string()))
    }
    fn remove_if_owned(path: &Path, expected: Identity, directory: bool) -> Result<(), QhardError> {
        if !unchanged(path, expected) {
            return Err(QhardError::Indeterminate(format!(
                "owned rollback target changed: {}",
                path.display()
            )));
        }
        (if directory {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        })
        .map_err(|e| {
            QhardError::Indeterminate(format!(
                "cannot remove owned rollback target {}: {e}",
                path.display()
            ))
        })
    }
    fn snapshot_build_tree(root: &Path) -> Result<Vec<(PathBuf, Identity, bool)>, QhardError> {
        fn visit(
            path: &Path,
            entries: &mut Vec<(PathBuf, Identity, bool)>,
        ) -> Result<(), QhardError> {
            let metadata = fs::symlink_metadata(path).map_err(|e| err(e.to_string()))?;
            if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
                return Err(err("unsafe build entry"));
            }
            let directory = metadata.is_dir();
            entries.push((path.to_owned(), identity(path)?, directory));
            if directory {
                for child in fs::read_dir(path).map_err(|e| err(e.to_string()))? {
                    visit(&child.map_err(|e| err(e.to_string()))?.path(), entries)?;
                }
            }
            Ok(())
        }
        let mut entries = Vec::new();
        visit(root, &mut entries)?;
        Ok(entries)
    }
    fn remove_build_root(entries: &[(PathBuf, Identity, bool)]) -> Result<(), QhardError> {
        for (path, expected, directory) in entries.iter().rev() {
            remove_if_owned(path, *expected, *directory)?;
        }
        Ok(())
    }
    fn create_private_build_root() -> Result<PathBuf, QhardError> {
        let parent = Path::new(BUILD_PARENT);
        let parent_identity = identity(parent)?;
        for _ in 0..32 {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random)
                .map_err(|e| err(format!("cannot sample build-root nonce: {e}")))?;
            let name = random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let candidate = parent.join(format!("kio-comparator-runtime-v1-{name}"));
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&candidate) {
                Ok(()) => {
                    if !unchanged(parent, parent_identity) {
                        return Err(QhardError::Indeterminate(
                            "build parent changed during create".into(),
                        ));
                    }
                    return Ok(candidate);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(err(error.to_string())),
            }
        }
        Err(err("could not allocate unique comparator build root"))
    }
    fn reconcile_created_file(
        path: &Path,
        recorded: &mut Option<Identity>,
        operation_succeeded: bool,
        label: &str,
    ) -> Result<(), QhardError> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            // A failed operation may leave a path, but it is not possible to
            // distinguish that partial effect from an intervening foreign
            // publication.  Never journal or delete an unprovable effect.
            Ok(_) if !operation_succeeded => Err(QhardError::Indeterminate(format!(
                "{label} appeared after failed operation; ownership is unknown"
            ))),
            Ok(metadata) if metadata.file_type().is_file() => {
                *recorded = Some(identity(path)?);
                Ok(())
            }
            Ok(_) => Err(QhardError::Indeterminate(format!(
                "operation left a non-file {label} target"
            ))),
            Err(error) => Err(QhardError::Indeterminate(format!(
                "cannot reconcile {label} effect: {error}"
            ))),
        }
    }
    fn reconcile_created_image(
        journal: &mut Journal,
        create_succeeded: bool,
    ) -> Result<(), QhardError> {
        reconcile_created_file(
            Path::new(IMAGE_PATH),
            &mut journal.image,
            create_succeeded,
            "runtime image",
        )
    }
    fn require_private_unpublished_image() -> Result<(), QhardError> {
        let metadata = fs::symlink_metadata(IMAGE_PATH).map_err(|e| err(e.to_string()))?;
        if !metadata.file_type().is_file()
            || metadata.uid() != 0
            || metadata.gid() != 0
            || metadata.mode() & 0o022 != 0
        {
            return Err(err("new comparator image is writable or not root-owned"));
        }
        Ok(())
    }
    pub(super) fn mounted_effect(
        baseline: &RuntimeMountIdentity,
        actual: RuntimeMountIdentity,
    ) -> Result<Option<RuntimeMountIdentity>, QhardError> {
        if &actual == baseline {
            return Ok(None);
        }
        if actual.mount_point != RUNTIME_ROOT {
            return Err(QhardError::Indeterminate(format!(
                "attach mounted comparator runtime at unexpected mountpoint: {}",
                actual.mount_point
            )));
        }
        if !actual.read_only {
            return Err(QhardError::Indeterminate(
                "attach changed mountpoint to a writable filesystem".into(),
            ));
        }
        Ok(Some(actual))
    }

    /// An observed mount is rollback-owned only when it is an exact effect at
    /// our requested mountpoint.  An ancestor/foreign mount cannot be safely
    /// attributed to this invocation, so it must remain undetached.
    pub(super) fn rollback_owned_mount_effect(
        baseline: &RuntimeMountIdentity,
        actual: &RuntimeMountIdentity,
    ) -> Option<RuntimeMountIdentity> {
        (actual != baseline && actual.mount_point == RUNTIME_ROOT).then(|| actual.clone())
    }

    fn reconcile_attached_mount(
        journal: &mut Journal,
        attach_succeeded: bool,
    ) -> Result<(), QhardError> {
        let baseline = journal.mount_baseline.as_ref().ok_or_else(|| {
            QhardError::Indeterminate("missing mount observation baseline".into())
        })?;
        let actual = observe_runtime_mount(Path::new(RUNTIME_ROOT)).map_err(|error| {
            QhardError::Indeterminate(format!("cannot reconcile mount effect: {error}"))
        })?;
        if !attach_succeeded {
            if &actual != baseline {
                return Err(QhardError::Indeterminate(
                    "mount changed after failed hdiutil attach; ownership is unknown".into(),
                ));
            }
            return Ok(());
        }
        // Only a successful invocation may be attributed to this process.
        // A concurrent foreign mount after an attach error is never detached.
        journal.mount = rollback_owned_mount_effect(baseline, &actual);
        mounted_effect(baseline, actual)?;
        Ok(())
    }
    fn reconcile_created_manifest(
        journal: &mut Journal,
        finalize_succeeded: bool,
    ) -> Result<(), QhardError> {
        reconcile_created_file(
            Path::new(MANIFEST_PATH),
            &mut journal.manifest,
            finalize_succeeded,
            "runtime manifest",
        )
    }
    struct Journal {
        /// `/` and `/Library` are retained for the lifetime of the
        /// transaction.  No mutation below is authorised by a merely named
        /// `/Library` path.
        library_authority: Vec<RetainedAuthority>,
        managed_binding: Option<RetainedAuthority>,
        runtime_binding: Option<RetainedAuthority>,
        managed_root: Option<Identity>,
        runtime_root: Option<Identity>,
        build_root: Option<(PathBuf, Identity)>,
        build_entries: Vec<(PathBuf, Identity, bool)>,
        image: Option<Identity>,
        manifest: Option<Identity>,
        /// Full `statfs` observation before attachment.  This mount can be
        /// writable: it is the ordinary directory used as hdiutil's owned
        /// mountpoint and is restored after detach.
        mount_baseline: Option<RuntimeMountIdentity>,
        /// Full `statfs` observation after attachment.  A detach is allowed
        /// only when this exact mounted filesystem is still present.
        mount: Option<RuntimeMountIdentity>,
        publication_parent: Option<(Identity, fs::File)>,
    }
    fn recheck_transaction(
        executable_authority: &[RetainedAuthority],
        journal: &Journal,
    ) -> Result<(), QhardError> {
        recheck_authority(executable_authority, "kio-eval")?;
        recheck_authority(&journal.library_authority, "/Library")?;
        if let Some(managed) = &journal.managed_binding {
            recheck_created(managed, "managed runtime root")?;
        }
        if let Some(mount) = &journal.mount {
            let actual = observe_runtime_mount(Path::new(RUNTIME_ROOT)).map_err(|error| {
                QhardError::Indeterminate(format!("cannot recheck mounted runtime: {error}"))
            })?;
            if &actual != mount {
                return Err(QhardError::Indeterminate(
                    "mounted runtime changed during transaction".into(),
                ));
            }
        } else if let Some(runtime) = &journal.runtime_binding {
            recheck_created(runtime, "runtime mountpoint")?;
        }
        if let Some(parent) = &journal.publication_parent {
            sync_retained_parent(parent)?;
        }
        for (path, expected, label) in [
            (IMAGE_PATH, journal.image, "runtime image"),
            (MANIFEST_PATH, journal.manifest, "runtime manifest"),
        ] {
            if let Some(expected) = expected
                && !unchanged(Path::new(path), expected)
            {
                return Err(QhardError::Indeterminate(format!(
                    "{label} changed during transaction"
                )));
            }
        }
        Ok(())
    }
    impl Journal {
        fn rollback(&mut self) -> Result<(), QhardError> {
            // A namespace replacement means the remaining fixed names may be
            // foreign.  Preserve them all rather than attempting best-effort
            // cleanup through a changed parent.
            recheck_authority(&self.library_authority, "/Library")?;
            if let Some(managed) = &self.managed_binding {
                recheck_created(managed, "managed runtime root")?;
            }
            if self.mount.is_none()
                && let Some(runtime) = &self.runtime_binding
            {
                recheck_created(runtime, "runtime mountpoint")?;
            }
            if let Some(mount) = self.mount.take() {
                let actual = observe_runtime_mount(Path::new(RUNTIME_ROOT)).map_err(|error| {
                    QhardError::Indeterminate(format!(
                        "cannot inspect mounted runtime before detach: {error}"
                    ))
                })?;
                if actual != mount {
                    return Err(QhardError::Indeterminate(
                        "mounted runtime observation changed before detach".into(),
                    ));
                }
                command("/usr/bin/hdiutil", &["detach", RUNTIME_ROOT]).map_err(|error| {
                    QhardError::Indeterminate(format!("cannot detach mounted runtime: {error}"))
                })?;
                let baseline = self.mount_baseline.as_ref().ok_or_else(|| {
                    QhardError::Indeterminate("missing mount observation baseline".into())
                })?;
                let restored = observe_runtime_mount(Path::new(RUNTIME_ROOT)).map_err(|error| {
                    QhardError::Indeterminate(format!(
                        "cannot verify mountpoint restoration after detach: {error}"
                    ))
                })?;
                if &restored != baseline {
                    return Err(QhardError::Indeterminate(
                        "detach did not restore exact mount observation".into(),
                    ));
                }
                if let Some(runtime) = &self.runtime_binding {
                    recheck_created(runtime, "runtime mountpoint")?;
                }
            }
            if let Some(identity) = self.manifest.take() {
                remove_if_owned(Path::new(MANIFEST_PATH), identity, false)?;
            }
            if let Some(identity) = self.image.take() {
                remove_if_owned(Path::new(IMAGE_PATH), identity, false)?;
            }
            if let Some((path, identity)) = self.build_root.take() {
                if self
                    .build_entries
                    .first()
                    .is_some_and(|(root, retained, _)| root == &path && *retained == identity)
                {
                    remove_build_root(&self.build_entries)?;
                } else {
                    return Err(QhardError::Indeterminate(
                        "cannot prove private build tree ownership for rollback".into(),
                    ));
                }
            }
            if let Some(identity) = self.runtime_root.take() {
                remove_if_owned(Path::new(RUNTIME_ROOT), identity, true)?;
            }
            if let Some(identity) = self.managed_root.take() {
                remove_if_owned(Path::new(MANAGED_ROOT), identity, true)?;
            }
            if self
                .build_entries
                .iter()
                .any(|(path, retained, _)| unchanged(path, *retained))
            {
                return Err(QhardError::Indeterminate(
                    "rollback could not prove all owned effects were removed".into(),
                ));
            }
            Ok(())
        }
    }
    fn install_inner(journal: &mut Journal) -> Result<ComparatorRuntimeInstallSummary, QhardError> {
        if unsafe { libc::geteuid() } != 0 {
            return Err(err("comparator runtime installation requires root"));
        }
        if std::env::args_os().next().as_deref() != Some(std::ffi::OsStr::new(CANONICAL_EVALUATOR))
        {
            return Err(err(
                "installer argv[0] must be canonical /usr/local/bin/kio-eval",
            ));
        }
        let executable = std::env::current_exe().map_err(|e| err(e.to_string()))?;
        if executable != Path::new(CANONICAL_EVALUATOR) {
            return Err(err(
                "installer must run from canonical /usr/local/bin/kio-eval",
            ));
        }
        let authority = require_root_authority(&executable, "kio-eval")?;
        journal.library_authority = require_root_authority(Path::new("/Library"), "/Library")?;
        for path in [MANAGED_ROOT, RUNTIME_ROOT, IMAGE_PATH, MANIFEST_PATH] {
            if fs::symlink_metadata(path).is_ok() {
                return Err(err(format!("create-only target already exists: {path}")));
            }
        }
        let library = journal
            .library_authority
            .last()
            .ok_or_else(|| err("missing /Library authority binding"))?;
        let managed = mkdirat_bound(
            library,
            "KioComparatorRuntime",
            Path::new(MANAGED_ROOT),
            0,
            0,
            "managed runtime root",
        )?;
        journal.managed_root = Some(managed.identity);
        journal.managed_binding = Some(managed);
        validate_created(
            journal.managed_binding.as_ref().expect("just retained"),
            0,
            0,
            "managed runtime root",
        )?;
        let managed = journal.managed_binding.as_ref().expect("just retained");
        journal.publication_parent = Some((
            managed.identity,
            managed
                .descriptor
                .try_clone()
                .map_err(|e| err(e.to_string()))?,
        ));
        let runtime = mkdirat_bound(
            managed,
            "v1",
            Path::new(RUNTIME_ROOT),
            0,
            0,
            "runtime mountpoint",
        )?;
        journal.runtime_root = Some(runtime.identity);
        journal.runtime_binding = Some(runtime);
        validate_created(
            journal.runtime_binding.as_ref().expect("just retained"),
            0,
            0,
            "runtime mountpoint",
        )?;
        journal.mount_baseline = Some(observe_runtime_mount(Path::new(RUNTIME_ROOT)).map_err(
            |error| {
                QhardError::Indeterminate(format!(
                    "cannot observe comparator mountpoint before attach: {error}"
                ))
            },
        )?);
        recheck_transaction(&authority, journal)?;
        let build_root = create_private_build_root()?;
        let build_identity = identity(&build_root)?;
        journal.build_root = Some((build_root.clone(), build_identity));
        let preimage = match prepare(&build_root) {
            Ok(preimage) => preimage,
            Err(error) => {
                // The root is owner-only and every entry was just made by this
                // process.  Snapshot before rollback so partial construction
                // has the same exact-identity cleanup contract as success.
                journal.build_entries = snapshot_build_tree(&build_root)?;
                return Err(error);
            }
        };
        journal.build_entries = snapshot_build_tree(&build_root)?;
        recheck_transaction(&authority, journal)?;
        let payload = build_root.join("payload");
        let payload = payload.to_str().ok_or_else(|| err("non-UTF8 build path"))?;
        let create = command(
            "/usr/bin/hdiutil",
            &[
                "create",
                "-srcfolder",
                payload,
                "-format",
                "UDRO",
                "-fs",
                "Case-sensitive APFS",
                "-volname",
                VOLUME_NAME,
                "-srcowners",
                "on",
                "-noanyowners",
                IMAGE_PATH,
            ],
        );
        if let Err(create_error) = create {
            reconcile_created_image(journal, false)?;
            recheck_transaction(&authority, journal)?;
            return Err(create_error);
        }
        reconcile_created_image(journal, true)?;
        recheck_transaction(&authority, journal)?;
        require_private_unpublished_image()?;
        fs::set_permissions(IMAGE_PATH, fs::Permissions::from_mode(0o444))
            .map_err(|e| err(e.to_string()))?;
        // chmod mutates the identity contract (mode is part of it), so the
        // rollback journal must retain the post-chmod observation.
        journal.image = Some(identity(Path::new(IMAGE_PATH))?);
        recheck_transaction(&authority, journal)?;
        let attach = command(
            "/usr/bin/hdiutil",
            &[
                "attach",
                "-readonly",
                "-owners",
                "on",
                "-nobrowse",
                "-noautoopen",
                "-mountpoint",
                RUNTIME_ROOT,
                IMAGE_PATH,
            ],
        );
        reconcile_attached_mount(journal, attach.is_ok())?;
        recheck_transaction(&authority, journal)?;
        if attach.is_ok() && journal.mount.is_none() {
            return Err(QhardError::Indeterminate(
                "hdiutil attach reported success without a mounted filesystem effect".into(),
            ));
        }
        attach?;
        recheck_transaction(&authority, journal)?;
        let finalized = finalize(
            Path::new(RUNTIME_ROOT),
            preimage,
            Path::new(IMAGE_PATH),
            Path::new(MANIFEST_PATH),
        );
        reconcile_created_manifest(journal, finalized.is_ok())?;
        recheck_transaction(&authority, journal)?;
        finalized?;
        recheck_transaction(&authority, journal)?;
        let (_, build_identity) = journal
            .build_root
            .as_ref()
            .ok_or_else(|| err("missing build journal"))?;
        if !journal
            .build_entries
            .first()
            .is_some_and(|(root, retained, _)| root == &build_root && *retained == *build_identity)
        {
            return Err(QhardError::Indeterminate(
                "missing exact build cleanup journal".into(),
            ));
        }
        remove_build_root(&journal.build_entries)?;
        if fs::symlink_metadata(&build_root).is_ok() {
            return Err(err("comparator runtime build-root cleanup failed"));
        }
        journal.build_root = None;
        journal.build_entries.clear();
        recheck_transaction(&authority, journal)?;
        Ok(ComparatorRuntimeInstallSummary {
            runtime_root: RUNTIME_ROOT.into(),
            image: IMAGE_PATH.into(),
            manifest: MANIFEST_PATH.into(),
        })
    }
    pub(super) fn install() -> Result<ComparatorRuntimeInstallSummary, QhardError> {
        let mut journal = Journal {
            managed_root: None,
            runtime_root: None,
            build_root: None,
            build_entries: Vec::new(),
            library_authority: Vec::new(),
            managed_binding: None,
            runtime_binding: None,
            image: None,
            manifest: None,
            mount_baseline: None,
            mount: None,
            publication_parent: None,
        };
        match install_inner(&mut journal) {
            Ok(summary) => Ok(summary),
            Err(error) => match journal.rollback() {
                Ok(()) => Err(error),
                Err(rollback) => Err(rollback),
            },
        }
    }
    fn prepare(build_root: &Path) -> Result<Preimage, QhardError> {
        let metadata = fs::symlink_metadata(build_root).map_err(|e| err(e.to_string()))?;
        if !metadata.file_type().is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(err("unsafe comparator runtime build root"));
        }
        let staged = build_root.join("reviewed-sources");
        let payload = build_root.join("payload");
        for d in [
            &staged,
            &payload,
            &payload.join("bin"),
            &payload.join("lib"),
            &payload.join("config"),
        ] {
            fs::create_dir(d).map_err(|e| err(e.to_string()))?
        }
        let mut sources = BTreeMap::new();
        for pin in REVIEWED_PINS {
            let dst = staged.join(hash_bytes(pin.path.as_bytes()));
            stage(*pin, &dst)?;
            sources.insert(pin.path, dst);
        }
        let mut aliases = BTreeMap::new();
        for pin in REVIEWED_PINS {
            let image = macho(&sources[pin.path])?;
            policy(&image, SEEDS.iter().any(|(_, p)| *p == pin.path))?;
            for a in [
                Some(base(pin.path)?),
                image.ids.first().map(|x| base(x)).transpose()?,
            ]
            .into_iter()
            .flatten()
            {
                insert_macho_alias(&mut aliases, a, pin.path)?;
            }
        }
        let mut closure = BTreeSet::new();
        let mut todo = SEEDS.iter().map(|(_, p)| *p).collect::<Vec<_>>();
        while let Some(src) = todo.pop() {
            if !closure.insert(src) {
                continue;
            }
            for load in macho(&sources[src])?.loads {
                if !system(&load) {
                    todo.push(
                        *aliases
                            .get(base(&load)?)
                            .ok_or_else(|| err("dependency absent from reviewed pins"))?,
                    )
                }
            }
        }
        if closure != REVIEWED_PINS.iter().map(|p| p.path).collect() {
            return Err(err("Mach-O closure does not equal reviewed pin set"));
        }
        let mut dest = BTreeMap::new();
        for src in &closure {
            let entry = SEEDS.iter().find_map(|(n, p)| (*p == *src).then_some(*n));
            let d = payload
                .join(if entry.is_some() { "bin" } else { "lib" })
                .join(entry.unwrap_or(base(src)?));
            if dest.values().any(|x: &PathBuf| x == &d) {
                return Err(err("payload basename collision"));
            }
            fs::copy(&sources[src], &d).map_err(|e| err(e.to_string()))?;
            dest.insert(*src, d);
        }
        for src in &closure {
            let m = macho(&sources[src])?;
            let d = &dest[src];
            for load in m.loads {
                if !system(&load) {
                    let dep = aliases
                        .get(base(&load)?)
                        .ok_or_else(|| err("unreviewed dependency"))?;
                    command(
                        "/usr/bin/install_name_tool",
                        &[
                            "-change",
                            &load,
                            &format!("@rpath/{}", base(&dest[dep].to_string_lossy())?),
                            &d.to_string_lossy(),
                        ],
                    )?;
                }
            }
            for r in m.rpaths {
                command(
                    "/usr/bin/install_name_tool",
                    &["-delete_rpath", &r, &d.to_string_lossy()],
                )?;
            }
            if SEEDS.iter().any(|(_, p)| *p == *src) {
                command(
                    "/usr/bin/install_name_tool",
                    &["-add_rpath", "@loader_path/../lib", &d.to_string_lossy()],
                )?;
            } else {
                command(
                    "/usr/bin/install_name_tool",
                    &[
                        "-id",
                        &format!("@rpath/{}", base(&d.to_string_lossy())?),
                        &d.to_string_lossy(),
                    ],
                )?;
                command(
                    "/usr/bin/install_name_tool",
                    &["-add_rpath", "@loader_path", &d.to_string_lossy()],
                )?;
            }
            command(
                "/usr/bin/codesign",
                &[
                    "--force",
                    "--sign",
                    "-",
                    "--timestamp=none",
                    &d.to_string_lossy(),
                ],
            )?;
            command(
                "/usr/bin/codesign",
                &["--verify", "--strict", &d.to_string_lossy()],
            )?;
        }
        fs::write(payload.join("config/rga-config.json"), CONFIG_BYTES)
            .map_err(|e| err(e.to_string()))?;
        let images = dest
            .values()
            .map(|p| {
                p.strip_prefix(&payload)
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect::<BTreeSet<_>>();
        let payload_files = rewalk(&payload, &images)?;
        for f in files(&payload)? {
            let mut p = fs::metadata(&f)
                .map_err(|e| err(e.to_string()))?
                .permissions();
            p.set_mode(if f.starts_with(payload.join("bin")) {
                0o555
            } else {
                0o444
            });
            fs::set_permissions(f, p).map_err(|e| err(e.to_string()))?
        }
        let pre = Preimage {
            schema_version: 1,
            runtime_root: "/Library/KioComparatorRuntime/v1".into(),
            config_sha256: hash_bytes(CONFIG_BYTES),
            sources_before: REVIEWED_PINS
                .iter()
                .map(|p| FileDigest {
                    path: p.path.into(),
                    sha256: reviewed_hash(*p),
                    bytes: p.bytes,
                })
                .collect(),
            payload_files,
            closure_images: images.into_iter().collect(),
        };
        validate(&pre)?;
        Ok(pre)
    }
    fn finalize(
        runtime_root: &Path,
        pre: Preimage,
        image: &Path,
        out: &Path,
    ) -> Result<(), QhardError> {
        validate(&pre)?;
        if !runtime_root_matches_preimage(runtime_root, &pre) {
            return Err(err("runtime root does not match preimage binding"));
        }
        let runtime = super::super::ComparatorRuntime::bind(runtime_root)?;
        let runtime_allowed_xattrs = runtime_xattrs(runtime_root)?;
        let actual = rewalk(runtime_root, &pre.closure_images.iter().cloned().collect())?;
        if actual != pre.payload_files {
            return Err(err("mounted runtime differs from preimage"));
        }
        let mut after = Vec::new();
        for pin in REVIEWED_PINS {
            let (s, b) = digest(Path::new(pin.path), pin.bytes)?;
            if s != reviewed_hash(*pin) || b != pin.bytes {
                return Err(err("reviewed source changed before publication"));
            }
            after.push(FileDigest {
                path: pin.path.into(),
                sha256: s,
                bytes: b,
            })
        }
        if fs::canonicalize(image).map_err(|e| err(e.to_string()))? != image {
            return Err(err("runtime image path is not canonical"));
        }
        let image_metadata = fs::symlink_metadata(image).map_err(|e| err(e.to_string()))?;
        if image_metadata.file_type().is_symlink()
            || !image_metadata.file_type().is_file()
            || image_metadata.uid() != 0
            || image_metadata.gid() != 0
            || image_metadata.mode() & 0o022 != 0
        {
            return Err(err("runtime image ownership or mode is unsafe"));
        }
        let image_allowed_xattrs = image_xattrs(image)?;
        let (image_sha256, _) = digest(image, MAX_FILE_BYTES)?;
        let bytes = serde_jcs::to_vec(&Manifest {
            preimage: pre,
            image_sha256,
            sources_after: after,
            image_xattr_policy: "subset:com.apple.FinderInfo,com.apple.provenance".into(),
            image_allowed_xattrs,
            image_attach_cache_policy: "delete:com.apple.diskimages.recentcksum".into(),
            runtime_xattr_policy: "only-com.apple.provenance".into(),
            runtime_allowed_xattrs,
            manifest_xattr_policy: "only-com.apple.provenance".into(),
            // This field is necessarily empty: allowing an xattr to appear
            // after serialization would make the manifest self-attestation
            // circular, so publication rejects such a mutation below.
            manifest_allowed_xattrs: Vec::new(),
        })
        .map_err(|e| err(e.to_string()))?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(out)
            .map_err(|e| err(e.to_string()))?;
        output
            .write_all(&bytes)
            .and_then(|_| output.write_all(b"\n"))
            .and_then(|_| output.sync_all())
            .map_err(|e| err(e.to_string()))?;
        // A macOS provenance marker is permitted but never interpreted.  The
        // payload/image observations above are serialized; the manifest's own
        // marker is only an OS annotation and cannot be made self-describing
        // without a circular digest dependency.
        let _ = manifest_xattrs(out)?;
        let mut expected = bytes;
        expected.push(b'\n');
        if fs::read(out).map_err(|e| err(e.to_string()))? != expected {
            return Err(err("published manifest readback differs"));
        }
        runtime.recheck(true)?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{
            os::unix::fs::{MetadataExt, PermissionsExt},
            process::Command,
        };
        use tempfile::TempDir;

        fn fixture() -> (TempDir, PathBuf, PathBuf, u32, u32) {
            let temporary = tempfile::tempdir().unwrap();
            let anchor = fs::canonicalize(temporary.path()).unwrap();
            let metadata = fs::symlink_metadata(&anchor).unwrap();
            let (uid, gid) = (metadata.uid(), metadata.gid());
            assert_eq!(uid, unsafe { libc::getuid() });
            // macOS children inherit their parent directory's group, which
            // can differ from getgid() when TMPDIR is beneath /private/tmp.
            let parent = anchor.join("trusted");
            fs::create_dir(&parent).unwrap();
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
            let leaf = parent.join("kio-eval");
            fs::write(&leaf, b"sealed evaluator").unwrap();
            fs::set_permissions(&leaf, fs::Permissions::from_mode(0o600)).unwrap();
            for path in [&parent, &leaf] {
                let metadata = fs::symlink_metadata(path).unwrap();
                assert_eq!((metadata.uid(), metadata.gid()), (uid, gid));
            }
            (temporary, anchor, leaf, uid, gid)
        }

        fn bind(anchor: &Path, leaf: &Path, uid: u32, gid: u32) -> Vec<RetainedAuthority> {
            require_authority_from(anchor, leaf, uid, gid, "test evaluator").unwrap()
        }

        #[test]
        fn authority_binding_accepts_a_canonical_private_tree() {
            let (_temporary, anchor, leaf, uid, gid) = fixture();
            let retained = bind(&anchor, &leaf, uid, gid);
            assert_eq!(retained.len(), 3);
            recheck_authority(&retained, "test evaluator").unwrap();
        }

        #[test]
        fn authority_binding_rejects_a_mismatched_expected_group() {
            let (_temporary, anchor, leaf, uid, gid) = fixture();
            assert!(
                require_authority_from(&anchor, &leaf, uid, gid.wrapping_add(1), "test evaluator")
                    .is_err()
            );
        }

        #[test]
        fn authority_binding_rejects_a_writable_intermediate_ancestor() {
            let (_temporary, anchor, leaf, uid, gid) = fixture();
            let parent = leaf.parent().unwrap();
            fs::set_permissions(parent, fs::Permissions::from_mode(0o722)).unwrap();
            assert!(require_authority_from(&anchor, &leaf, uid, gid, "test evaluator").is_err());
        }

        #[test]
        fn authority_binding_rejects_an_acl_bearing_intermediate_ancestor() {
            let (_temporary, anchor, leaf, uid, gid) = fixture();
            let parent = leaf.parent().unwrap();
            let user = std::env::var("USER").unwrap();
            let output = Command::new("/bin/chmod")
                .args([
                    "+a",
                    &format!("user:{user} allow read"),
                    &parent.display().to_string(),
                ])
                .output()
                .unwrap();
            assert!(output.status.success(), "chmod +a did not add a test ACL");
            assert!(require_authority_from(&anchor, &leaf, uid, gid, "test evaluator").is_err());
            let cleanup = Command::new("/bin/chmod")
                .args(["-a#", "0", &parent.display().to_string()])
                .output()
                .unwrap();
            assert!(
                cleanup.status.success(),
                "chmod -a# did not remove the test ACL"
            );
        }

        #[test]
        fn authority_binding_rejects_a_symlink_component() {
            let (_temporary, anchor, leaf, uid, gid) = fixture();
            let outside = anchor.join("outside");
            fs::write(&outside, b"sealed evaluator").unwrap();
            fs::remove_file(&leaf).unwrap();
            std::os::unix::fs::symlink(&outside, &leaf).unwrap();
            assert!(require_authority_from(&anchor, &leaf, uid, gid, "test evaluator").is_err());
        }

        #[test]
        fn authority_recheck_rejects_a_same_content_leaf_replacement() {
            let (_temporary, anchor, leaf, uid, gid) = fixture();
            let retained = bind(&anchor, &leaf, uid, gid);
            fs::remove_file(&leaf).unwrap();
            fs::write(&leaf, b"sealed evaluator").unwrap();
            fs::set_permissions(&leaf, fs::Permissions::from_mode(0o600)).unwrap();
            assert!(recheck_authority(&retained, "test evaluator").is_err());
        }

        #[test]
        fn authority_recheck_rejects_a_parent_replacement() {
            let (_temporary, anchor, leaf, uid, gid) = fixture();
            let retained = bind(&anchor, &leaf, uid, gid);
            let parent = leaf.parent().unwrap();
            fs::remove_file(&leaf).unwrap();
            fs::remove_dir(parent).unwrap();
            fs::create_dir(parent).unwrap();
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(&leaf, b"sealed evaluator").unwrap();
            fs::set_permissions(&leaf, fs::Permissions::from_mode(0o600)).unwrap();
            assert!(recheck_authority(&retained, "test evaluator").is_err());
        }

        #[test]
        fn capability_relative_creation_binds_and_detects_replacement() {
            let (_temporary, anchor, leaf, uid, gid) = fixture();
            let retained = require_authority_from(&anchor, &leaf, uid, gid, "test parent").unwrap();
            let parent = &retained[retained.len() - 2];
            let created_path = parent.path.join("created");
            let created = mkdirat_bound(
                parent,
                "created",
                &created_path,
                uid,
                gid,
                "test created root",
            )
            .unwrap();
            validate_created(&created, uid, gid, "test created root").unwrap();
            recheck_created(&created, "test created root").unwrap();
            fs::remove_dir(&created_path).unwrap();
            fs::create_dir(&created_path).unwrap();
            fs::set_permissions(&created_path, fs::Permissions::from_mode(0o700)).unwrap();
            assert!(recheck_created(&created, "test created root").is_err());
        }

        #[test]
        fn created_directory_rejects_an_inherited_acl() {
            let (_temporary, anchor, leaf, uid, gid) = fixture();
            let retained = require_authority_from(&anchor, &leaf, uid, gid, "test parent").unwrap();
            let parent = &retained[retained.len() - 2];
            let user = std::env::var("USER").unwrap();
            let acl = Command::new("/bin/chmod")
                .args([
                    "+a",
                    &format!("user:{user} allow read,write,file_inherit,directory_inherit"),
                    &parent.path.display().to_string(),
                ])
                .output()
                .unwrap();
            assert!(acl.status.success(), "chmod +a did not add inherited ACL");
            let path = parent.path.join("created-acl");
            assert!(
                mkdirat_bound(parent, "created-acl", &path, uid, gid, "test created root").is_err()
            );
            assert!(!path.exists());
            let cleanup = Command::new("/bin/chmod")
                .args(["-a#", "0", &parent.path.display().to_string()])
                .output()
                .unwrap();
            assert!(
                cleanup.status.success(),
                "chmod -a# did not remove test ACL"
            );
        }

        #[test]
        fn failed_file_effect_is_not_journalled_or_removed() {
            let temporary = tempfile::tempdir().unwrap();
            for name in ["runtime.dmg", "runtime.manifest.json"] {
                let path = temporary.path().join(name);
                fs::write(&path, b"foreign").unwrap();
                let mut recorded = None;
                assert!(reconcile_created_file(&path, &mut recorded, false, name).is_err());
                assert!(recorded.is_none());
                assert_eq!(fs::read(&path).unwrap(), b"foreign");
            }
        }

        #[test]
        fn cleanup_never_removes_a_foreign_replacement_or_create_only_collision() {
            let temporary = tempfile::tempdir().unwrap();
            let path = temporary.path().join("owned");
            fs::write(&path, b"owned").unwrap();
            let owned = identity(&path).unwrap();
            fs::remove_file(&path).unwrap();
            fs::write(&path, b"foreign").unwrap();
            assert!(remove_if_owned(&path, owned, false).is_err());
            assert_eq!(fs::read(&path).unwrap(), b"foreign");
            assert!(
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .is_err()
            );
        }

        #[test]
        fn private_build_root_is_owner_only_and_exactly_cleaned_up() {
            let root = create_private_build_root().unwrap();
            let metadata = fs::symlink_metadata(&root).unwrap();
            assert_eq!(metadata.mode() & 0o777, 0o700);
            assert_eq!(metadata.uid(), unsafe { libc::getuid() });
            let retained = identity(&root).unwrap();
            let entries = snapshot_build_tree(&root).unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].0, root);
            assert!(unchanged(&root, retained));
            remove_build_root(&entries).unwrap();
            assert!(fs::symlink_metadata(&root).is_err());
        }

        #[test]
        fn post_chmod_image_identity_allows_exact_rollback_cleanup() {
            let temporary = tempfile::tempdir().unwrap();
            let image = temporary.path().join("runtime.dmg");
            fs::write(&image, b"image").unwrap();
            fs::set_permissions(&image, fs::Permissions::from_mode(0o600)).unwrap();
            let before_chmod = identity(&image).unwrap();
            fs::set_permissions(&image, fs::Permissions::from_mode(0o444)).unwrap();
            let after_chmod = identity(&image).unwrap();
            assert_ne!(
                before_chmod, after_chmod,
                "mode participates in rollback identity"
            );
            remove_if_owned(&image, after_chmod, false).unwrap();
            assert!(fs::symlink_metadata(&image).is_err());
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    fn observed_mount(
        fsid: &str,
        mount_point: &str,
        mounted_from: &str,
        filesystem_type: &str,
        flags: u64,
        read_only: bool,
    ) -> RuntimeMountIdentity {
        RuntimeMountIdentity {
            fsid: fsid.into(),
            mount_point: mount_point.into(),
            mounted_from: mounted_from.into(),
            filesystem_type: filesystem_type.into(),
            flags,
            read_only,
        }
    }

    #[test]
    fn attach_effect_preserves_full_mount_observation_and_rejects_writable_effects() {
        let baseline = observed_mount(
            "fsid(1,2)",
            "/Library/KioComparatorRuntime/v1",
            "/dev/disk1s1",
            "apfs",
            0,
            false,
        );
        let mounted = observed_mount(
            "fsid(9,10)",
            "/Library/KioComparatorRuntime/v1",
            "/dev/disk9s1",
            "apfs",
            1,
            true,
        );
        assert_eq!(
            macos::mounted_effect(&baseline, baseline.clone()).unwrap(),
            None
        );
        assert_eq!(
            macos::mounted_effect(&baseline, mounted.clone()).unwrap(),
            Some(mounted.clone())
        );

        let mut writable = mounted.clone();
        writable.flags = 0;
        writable.read_only = false;
        assert!(matches!(
            macos::mounted_effect(&baseline, writable.clone()),
            Err(QhardError::Indeterminate(_))
        ));
        assert_eq!(
            macos::rollback_owned_mount_effect(&baseline, &writable),
            Some(writable.clone())
        );

        let mut elsewhere = mounted;
        elsewhere.mount_point = "/Library/KioComparatorRuntime".into();
        assert!(matches!(
            macos::mounted_effect(&baseline, elsewhere.clone()),
            Err(QhardError::Indeterminate(_))
        ));
        assert_eq!(
            macos::rollback_owned_mount_effect(&baseline, &elsewhere),
            None,
            "an ancestor/foreign mount must never be detached through RUNTIME_ROOT"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn administrator_tools_run_with_the_fixed_clean_environment() {
        let output = macos::command("/usr/bin/env", &[]).unwrap();
        let actual = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            actual,
            BTreeSet::from([
                "HOME=/var/root".into(),
                "LANG=C".into(),
                "LC_ALL=C".into(),
                "PATH=/usr/bin:/bin:/usr/sbin:/sbin".into(),
                "TZ=UTC".into(),
            ])
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn administrator_tools_force_a_private_creation_umask() {
        // This remains true even if the parent process has a permissive umask:
        // `command` installs 077 in the child immediately before exec.
        assert_eq!(
            macos::command("/bin/sh", &["-c", "umask"]).unwrap(),
            b"0077\n"
        );
    }

    #[test]
    fn pins_are_closed() {
        assert_eq!(reviewed_pins().len(), 29);
        assert_eq!(
            reviewed_pins().iter().map(|p| p.bytes).sum::<u64>(),
            312_045_232
        );
        assert!(reviewed_pins().iter().all(|p| {
            p.path.starts_with("/opt/homebrew/Cellar/")
                && p.sha256_hex.len() == 64
                && p.sha256_hex
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        }))
    }
    #[test]
    fn externally_recorded_hashes_are_lowercase_tagged_sha256() {
        let hash = reviewed_hash(reviewed_pins()[0]);
        assert!(hash.starts_with("sha256:"));
        assert_eq!(hash.len(), 71);
        assert!(
            hash[7..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }
    #[test]
    fn image_xattr_contract_only_permits_the_attach_cache_transiently() {
        let cache = BTreeSet::from(["com.apple.diskimages.recentcksum".to_owned()]);
        assert!(image_xattr_names_allowed(&cache, true));
        assert!(!image_xattr_names_allowed(&cache, false));
        assert!(image_xattr_names_allowed(
            &BTreeSet::from([
                "com.apple.FinderInfo".to_owned(),
                "com.apple.provenance".to_owned()
            ]),
            false
        ));
        assert!(!image_xattr_names_allowed(
            &BTreeSet::from(["com.apple.quarantine".to_owned()]),
            true
        ));
    }
    #[test]
    fn macho_aliases_allow_one_pin_to_repeat_its_own_basename_only() {
        let mut aliases = BTreeMap::new();
        insert_macho_alias(&mut aliases, "libfontconfig.1.dylib", "/pin/fontconfig").unwrap();
        insert_macho_alias(&mut aliases, "libfontconfig.1.dylib", "/pin/fontconfig").unwrap();
        assert_eq!(aliases["libfontconfig.1.dylib"], "/pin/fontconfig");
        assert!(insert_macho_alias(&mut aliases, "libfontconfig.1.dylib", "/pin/other").is_err());
    }
    #[test]
    fn malformed_preimage_fails_closed() {
        let mut p = Preimage {
            schema_version: 1,
            runtime_root: "/Library/KioComparatorRuntime/v1".into(),
            config_sha256: hash_bytes(CONFIG_BYTES),
            sources_before: reviewed_pins()
                .iter()
                .map(|x| FileDigest {
                    path: x.path.into(),
                    sha256: reviewed_hash(*x),
                    bytes: x.bytes,
                })
                .collect(),
            payload_files: (0..reviewed_pins().len())
                .map(|index| FileDigest {
                    path: format!("lib/{index}"),
                    sha256: format!("sha256:{}", "a".repeat(64)),
                    bytes: 1,
                })
                .collect(),
            closure_images: (0..reviewed_pins().len())
                .map(|index| format!("lib/{index}"))
                .collect(),
        };
        assert!(validate(&p).is_ok());
        assert!(runtime_root_matches_preimage(
            Path::new("/Library/KioComparatorRuntime/v1"),
            &p
        ));
        assert!(!runtime_root_matches_preimage(
            Path::new("/tmp/runtime"),
            &p
        ));
        p.payload_files[0].path = "../rga".into();
        assert!(validate(&p).is_err());
    }
    #[test]
    fn malformed_macho_metadata_vectors_fail_closed() {
        assert!(
            macos::parse_otool(
                b" cmd LC_ID_DYLIB\n name a (offset 1)\n cmd LC_ID_DYLIB\n name b (offset 1)\n"
            )
            .is_err()
        );
        let parsed = macos::parse_otool(
            b" cmd LC_LOAD_DYLIB\n name @rpath/libx.dylib (offset 1)\n cmd LC_RPATH\n path @loader_path (offset 1)\n",
        )
        .expect("well formed bounded vector");
        assert_eq!(parsed.loads, ["@rpath/libx.dylib"]);
        assert_eq!(parsed.rpaths, ["@loader_path"]);
    }
}

#[cfg(all(test, not(target_os = "macos")))]
mod non_macos_tests {
    use super::*;

    #[test]
    fn installation_fails_closed_off_macos() {
        assert!(install_comparator_runtime().is_err());
    }
}
