//! Local package-manager evidence for the Debian/Ubuntu Office runtime.
use super::office::{RuntimeTrust, optional_metadata};
use crate::office_runtime_identity::check_verification_deadline;
use md5::{Digest, Md5};
use std::{
    collections::BTreeMap,
    fs::{File, Metadata, OpenOptions},
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    time::Instant,
};

type Checksum = [u8; 16];

pub(super) struct DpkgCatalog {
    entries: BTreeMap<PathBuf, Option<Checksum>>,
}

impl DpkgCatalog {
    pub(super) fn read(system: &Path, trust: RuntimeTrust, deadline: Instant) -> io::Result<Self> {
        let dpkg = system.join("var/lib/dpkg");
        let info = dpkg.join("info");
        trust.verify_ancestors(&info, system)?;
        let required = |path: &Path| {
            read_record(path, trust, deadline)?
                .ok_or_else(|| invalid(path, "missing dpkg package record"))
        };
        let status = required(&dpkg.join("status"))?;
        let ucf_database = system.join("var/lib/ucf");
        let mut ucf = BTreeMap::<&str, Vec<&str>>::new();
        let ucf_registry = if optional_metadata(&ucf_database)?.is_some() {
            trust.verify_ancestors(&ucf_database, system)?;
            read_record(&ucf_database.join("registry"), trust, deadline)?
        } else {
            None
        };
        for line in ucf_registry.as_deref().unwrap_or_default().lines() {
            if let Some((owner, path)) = line.split_once(char::is_whitespace)
                && path.trim().starts_with("/etc/libreoffice/registry/")
            {
                ucf.entry(owner).or_default().push(path.trim());
            }
        }

        let mut entries = BTreeMap::new();
        for paragraph in status.split("\n\n") {
            check_verification_deadline(deadline)?;
            let field = |name: &str| paragraph.lines().find_map(|line| line.strip_prefix(name));
            let Some(package) = field("Package: ").filter(|name| office_package(name)) else {
                continue;
            };
            if !field("Status: ").is_some_and(|status| status.ends_with(" ok installed")) {
                continue;
            }
            let arch = field("Architecture: ")
                .filter(|arch| !arch.is_empty() && arch.bytes().all(valid_name_byte))
                .ok_or_else(|| invalid(&dpkg.join("status"), "invalid package architecture"))?;
            let qualified = format!("{package}:{arch}");
            let qualified_list = info.join(format!("{qualified}.list"));
            let (stem, list) = match read_record(&qualified_list, trust, deadline)? {
                Some(list) => (qualified.as_str(), list),
                None => (package, required(&info.join(format!("{package}.list")))?),
            };
            let mut sums = BTreeMap::new();
            let md5sums = info.join(format!("{stem}.md5sums"));
            for line in read_record(&md5sums, trust, deadline)?
                .as_deref()
                .unwrap_or_default()
                .lines()
            {
                let (hash, path) = line
                    .split_once("  ")
                    .ok_or_else(|| invalid(&md5sums, "invalid dpkg checksum entry"))?;
                let path = package_path(Path::new("/").join(path).as_path())?;
                sums.insert(path, parse_checksum(hash, &md5sums)?);
            }
            for line in paragraph
                .lines()
                .skip_while(|line| *line != "Conffiles:")
                .skip(1)
                .take_while(|line| line.starts_with(' '))
            {
                let mut fields = line.split_ascii_whitespace();
                if let (Some(path), Some(hash), None) =
                    (fields.next(), fields.next(), fields.next())
                {
                    let path = package_path(Path::new(path))?;
                    sums.insert(path, parse_checksum(hash, &dpkg.join("status"))?);
                }
            }
            let mut members = BTreeMap::new();
            for line in list.lines().filter(|line| *line != "/." && *line != "/") {
                let path = package_path(Path::new(line))?;
                members.insert(path.clone(), sums.get(&path).copied());
            }
            // Debian/Ubuntu install the registry configuration via ucf. Bind each
            // copied file to its installed owner's checksummed package template.
            for path in ucf
                .get(package)
                .into_iter()
                .chain(ucf.get(stem).filter(|_| stem != package))
                .flatten()
            {
                let relative = package_path(Path::new(path))?;
                let suffix = relative
                    .strip_prefix("etc/libreoffice/registry")
                    .expect("filtered registry path");
                let template = Path::new("usr/lib/libreoffice/share/.registry").join(suffix);
                let digest = members.get(&template).copied().flatten().ok_or_else(|| {
                    invalid(
                        Path::new(path),
                        "missing checksummed dpkg registry template",
                    )
                })?;
                members.insert(relative, Some(digest));
            }
            for (relative, digest) in members {
                let path = system.join(relative);
                let entry = entries.entry(path.clone()).or_insert(None);
                if let Some(digest) = digest {
                    if entry.is_some_and(|previous| previous != digest) {
                        return Err(invalid(&path, "conflicting dpkg checksums"));
                    }
                    *entry = Some(digest);
                }
            }
        }
        Ok(Self { entries })
    }

    pub(super) fn verify(
        &self,
        path: &Path,
        metadata: &Metadata,
        deadline: Instant,
    ) -> io::Result<()> {
        let digest = self
            .entries
            .get(path)
            .ok_or_else(|| invalid(path, "unlisted in installed Office packages"))?;
        match (digest, metadata.is_file()) {
            (Some(expected), true) => verify_checksum(path, metadata, expected, deadline),
            (None, false) => Ok(()),
            _ => Err(invalid(
                path,
                "missing dpkg checksum or unexpected package file type",
            )),
        }
    }
}

fn office_package(name: &str) -> bool {
    name.bytes().all(valid_name_byte)
        && ([
            "libreoffice",
            "ure",
            "uno-libs-private",
            "libunoloader-java",
            "liblibreoffice-java",
            "python3-uno",
        ]
        .contains(&name)
            || [
                "libreoffice-",
                "libuno-",
                "ure-",
                "libhsqldb",
                "libwebsocket-api",
                "libjava-websocket",
            ]
            .iter()
            .any(|prefix| name.starts_with(prefix)))
}

fn valid_name_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"+.-".contains(&byte)
}

fn package_path(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| component == Component::ParentDir)
    {
        return Err(invalid(path, "invalid dpkg package path"));
    }
    Ok(path
        .strip_prefix("/")
        .expect("absolute package path")
        .into())
}

fn parse_checksum(hash: &str, record: &Path) -> io::Result<Checksum> {
    if hash.len() != 32 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid(record, "invalid dpkg MD5 checksum"));
    }
    Ok(std::array::from_fn(|index| {
        u8::from_str_radix(&hash[index * 2..index * 2 + 2], 16).expect("validated MD5 digits")
    }))
}

fn read_record(path: &Path, trust: RuntimeTrust, deadline: Instant) -> io::Result<Option<String>> {
    const MAX_RECORD_BYTES: u64 = 16 * 1024 * 1024;
    check_verification_deadline(deadline)?;
    let file = match open_regular(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    trust.verify_entry(path, &file.metadata()?)?;
    let mut record = String::new();
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_string(&mut record)?;
    if record.len() as u64 > MAX_RECORD_BYTES {
        return Err(invalid(path, "oversized package record"));
    }
    check_verification_deadline(deadline)?;
    Ok(Some(record))
}

fn open_regular(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid(path, "expected a regular package file"));
    }
    Ok(file)
}

fn verify_checksum(
    path: &Path,
    metadata: &Metadata,
    expected: &Checksum,
    deadline: Instant,
) -> io::Result<()> {
    let mut file = open_regular(path)?;
    let opened = file.metadata()?;
    if (opened.dev(), opened.ino()) != (metadata.dev(), metadata.ino()) {
        return Err(invalid(path, "package file changed while opening"));
    }
    let mut hash = Md5::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        check_verification_deadline(deadline)?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    if <[u8; 16]>::from(hash.finalize()) != *expected {
        return Err(invalid(path, "dpkg checksum mismatch"));
    }
    Ok(())
}

fn invalid(path: &Path, reason: &str) -> io::Error {
    io::Error::other(format!("{reason}: {}", path.display()))
}
