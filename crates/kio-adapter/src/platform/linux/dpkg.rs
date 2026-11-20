//! Local package-manager evidence for the Debian/Ubuntu Office runtime.
use super::office::RuntimeTrust;
use crate::office_runtime_identity::{
    RecordSnapshot, check_verification_deadline, optional_metadata,
};
use md5::{Digest, Md5};
use std::{
    collections::BTreeMap,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    time::Instant,
};

type Checksum = [u8; 16];

pub(super) struct DpkgCatalog {
    entries: BTreeMap<PathBuf, Option<Checksum>>,
    pub(super) records: RecordSnapshot,
}

impl DpkgCatalog {
    pub(super) fn read(
        system: &Path,
        trust: RuntimeTrust,
        package_filter: impl Fn(&str) -> bool,
        deadline: Instant,
    ) -> io::Result<Self> {
        Self::read_with(system, trust, package_filter, deadline, || {})
    }

    fn read_with(
        system: &Path,
        trust: RuntimeTrust,
        package_filter: impl Fn(&str) -> bool,
        deadline: Instant,
        after_status: impl FnOnce(),
    ) -> io::Result<Self> {
        let dpkg = system.join("var/lib/dpkg");
        let info = dpkg.join("info");
        let updates = dpkg.join("updates");
        let status_path = dpkg.join("status");
        let ucf_database = system.join("var/lib/ucf");
        let registry = ucf_database.join("registry");
        for directory in [&info, &updates] {
            trust.verify_ancestors(directory, system)?;
        }
        let mut records = RecordSnapshot::default();
        for path in [
            &dpkg,
            &info,
            &updates,
            &status_path,
            &ucf_database,
            &registry,
        ] {
            records.observe(path)?;
        }
        if fs::read_dir(&updates)?.next().transpose()?.is_some() {
            return Err(invalid(&updates, "dpkg has pending updates"));
        }
        // Capture matching record identities before consuming status or any package list.
        for entry in fs::read_dir(&info)? {
            check_verification_deadline(deadline)?;
            let entry = entry?;
            let name = entry.file_name();
            if name.to_str().is_some_and(|name| {
                name.strip_suffix(".list")
                    .or_else(|| name.strip_suffix(".md5sums"))
                    .is_some_and(|stem| package_filter(stem.split(':').next().unwrap_or(stem)))
            }) {
                records.observe(&entry.path())?;
            }
        }
        let required = |path: &Path| {
            read_record(path, trust, deadline)?
                .ok_or_else(|| invalid(path, "missing dpkg package record"))
        };
        let status = required(&status_path)?;
        after_status();
        let mut ucf = BTreeMap::<&str, Vec<&str>>::new();
        let ucf_registry = if optional_metadata(&ucf_database)?.is_some() {
            trust.verify_ancestors(&ucf_database, system)?;
            read_record(&registry, trust, deadline)?
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
            let Some(package) =
                field("Package: ").filter(|name| valid_package_name(name) && package_filter(name))
            else {
                continue;
            };
            let state = field("Status: ").unwrap_or_default();
            let (_, status) = state.split_once(' ').unwrap_or_default();
            match status {
                "ok installed" => {}
                "ok not-installed" | "ok config-files" => continue,
                _ => {
                    return Err(io::Error::other(format!(
                        "dpkg Office package {package} is not configured: {state}"
                    )));
                }
            }
            let arch = field("Architecture: ")
                .filter(|arch| !arch.is_empty() && arch.bytes().all(valid_name_byte))
                .ok_or_else(|| invalid(&dpkg.join("status"), "invalid package architecture"))?;
            let qualified = format!("{package}:{arch}");
            let qualified_list = info.join(format!("{qualified}.list"));
            records.observe(&qualified_list)?;
            let (stem, list) = match read_record(&qualified_list, trust, deadline)? {
                Some(list) => (qualified.as_str(), list),
                None => {
                    let list = info.join(format!("{package}.list"));
                    records.observe(&list)?;
                    (package, required(&list)?)
                }
            };
            let mut sums = BTreeMap::new();
            let md5sums = info.join(format!("{stem}.md5sums"));
            records.observe(&md5sums)?;
            for line in read_record(&md5sums, trust, deadline)?
                .as_deref()
                .unwrap_or_default()
                .lines()
            {
                let (hash, path) = line
                    .split_once("  ")
                    .ok_or_else(|| invalid(&md5sums, "invalid dpkg checksum entry"))?;
                let path = package_path(Path::new("/").join(path).as_path())?;
                merge_checksum(&mut sums, path, parse_checksum(hash, &md5sums)?)?;
            }
            for line in paragraph
                .lines()
                .skip_while(|line| *line != "Conffiles:")
                .skip(1)
                .take_while(|line| line.starts_with(' '))
            {
                let (path, hash) = parse_conffile(line, &status_path)?;
                merge_checksum(&mut sums, path, hash)?;
            }
            let mut members = BTreeMap::new();
            for line in list.lines().filter(|line| *line != "/." && *line != "/") {
                let path = package_path(Path::new(line))?;
                members.insert(path.clone(), sums.get(&path).copied().flatten());
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
                // A conffile's checksum remains evidence even if a stale .list omits it.
                merge_checksum(&mut sums, relative.clone(), digest)?;
                merge_checksum(&mut members, relative, digest)?;
            }
            for (relative, digest) in members {
                let path = system.join(relative);
                if let Some(digest) = digest {
                    merge_checksum(&mut entries, path, digest)?;
                } else {
                    entries.entry(path).or_insert(None);
                }
            }
        }
        records.verify(deadline)?;
        Ok(Self { entries, records })
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

fn merge_checksum(
    checksums: &mut BTreeMap<PathBuf, Option<Checksum>>,
    path: PathBuf,
    digest: Checksum,
) -> io::Result<()> {
    let entry = checksums.entry(path.clone()).or_default();
    if entry.is_some_and(|previous| previous != digest) {
        return Err(invalid(&path, "conflicting dpkg checksums"));
    }
    *entry = Some(digest);
    Ok(())
}

fn valid_package_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(valid_name_byte)
}

pub(super) fn office_package(name: &str) -> bool {
    [
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
        .any(|prefix| name.starts_with(prefix))
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

fn parse_conffile(line: &str, record: &Path) -> io::Result<(PathBuf, Checksum)> {
    let line = line.strip_prefix(' ').expect("conffile continuation");
    // dpkg writes the path verbatim, then the digest and any flags.
    let line = line.strip_suffix(" remove-on-upgrade").unwrap_or(line);
    let line = line.strip_suffix(" obsolete").unwrap_or(line);
    let (path, hash) = line
        .rsplit_once(' ')
        .ok_or_else(|| invalid(record, "invalid dpkg conffile entry"))?;
    Ok((
        package_path(Path::new(path))?,
        parse_checksum(hash, record)?,
    ))
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

pub(super) fn open_regular(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid(path, "expected a regular package file"));
    }
    Ok(file)
}

pub(super) fn open_package_file(path: &Path, metadata: &Metadata) -> io::Result<File> {
    let file = open_regular(path)?;
    let opened = file.metadata()?;
    if (opened.dev(), opened.ino()) != (metadata.dev(), metadata.ino()) {
        return Err(invalid(path, "package file changed while opening"));
    }
    Ok(file)
}

fn verify_checksum(
    path: &Path,
    metadata: &Metadata,
    expected: &Checksum,
    deadline: Instant,
) -> io::Result<()> {
    let mut file = open_package_file(path, metadata)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::Duration};

    const PACKAGE: &str = "libreoffice-core";
    const INSTALLED: &str =
        "Package: libreoffice-core\nStatus: install ok installed\nArchitecture: amd64\n";
    const TEMPLATE: &str = "/usr/lib/libreoffice/share/.registry/config.xcd";
    const CONFIG: &str = "/etc/libreoffice/registry/config.xcd";
    const ABC: &str = "900150983cd24fb0d6963f7d28e17f72";

    fn records(root: &Path) -> io::Result<()> {
        let dpkg = root.join("var/lib/dpkg");
        fs::create_dir_all(dpkg.join("info"))?;
        fs::create_dir(dpkg.join("updates"))?;
        fs::write(dpkg.join("status"), INSTALLED)?;
        fs::write(
            dpkg.join(format!("info/{PACKAGE}.list")),
            format!("{TEMPLATE}\n{CONFIG}\n"),
        )?;
        fs::write(
            dpkg.join(format!("info/{PACKAGE}.md5sums")),
            format!("{ABC}  {}\n", &TEMPLATE[1..]),
        )?;
        fs::create_dir(root.join("var/lib/ucf"))?;
        fs::write(
            root.join("var/lib/ucf/registry"),
            format!("{PACKAGE} {CONFIG}\n"),
        )
    }

    fn read(root: &Path, after_status: impl FnOnce()) -> io::Result<DpkgCatalog> {
        DpkgCatalog::read_with(
            root,
            RuntimeTrust::fixture(root),
            office_package,
            Instant::now() + Duration::from_secs(10),
            after_status,
        )
    }

    #[test]
    fn dpkg_rejects_pending_update_records() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        records(root).unwrap();
        assert!(read(root, || {}).is_ok());
        fs::write(
            root.join("var/lib/dpkg/updates/0000"),
            INSTALLED.replace("ok installed", "ok unpacked"),
        )
        .unwrap();
        let error = read(root, || {})
            .err()
            .expect("pending update must refuse admission");
        assert!(error.to_string().contains("updates"), "{error}");
    }

    #[test]
    fn dpkg_rejects_relevant_unconfigured_package() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        records(root).unwrap();
        fs::write(
            root.join("var/lib/dpkg/status"),
            INSTALLED.replace("ok installed", "ok half-configured"),
        )
        .unwrap();
        let error = read(root, || {})
            .err()
            .expect("unconfigured Office package must refuse admission");
        assert!(error.to_string().contains("half-configured"), "{error}");
    }

    #[test]
    fn dpkg_rejects_status_replaced_during_catalog_read() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        records(root).unwrap();
        let error = read(root, || {
            let replacement = root.join("var/lib/dpkg/status-new");
            fs::write(&replacement, INSTALLED).unwrap();
            fs::rename(replacement, root.join("var/lib/dpkg/status")).unwrap();
        })
        .err()
        .expect("records from a changing database must refuse admission");
        assert!(error.to_string().contains("changed"), "{error}");
    }

    #[test]
    fn dpkg_rejects_in_place_checksum_record_change_during_catalog_read() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        records(root).unwrap();
        let error = read(root, || {
            fs::write(
                root.join(format!("var/lib/dpkg/info/{PACKAGE}.md5sums")),
                format!("d41d8cd98f00b204e9800998ecf8427e  {}\n", &TEMPLATE[1..]),
            )
            .unwrap();
        })
        .err()
        .expect("an in-place database update must refuse admission");
        assert!(error.to_string().contains("changed"), "{error}");
    }

    #[test]
    fn dpkg_rejects_ucf_digest_conflicting_with_same_package_conffile() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        records(root).unwrap();
        let catalog = read(root, || {}).unwrap();
        assert_eq!(
            catalog.entries[&root.join(&CONFIG[1..])],
            Some(parse_checksum(ABC, Path::new(CONFIG)).unwrap())
        );
        fs::write(
            root.join("var/lib/dpkg/status"),
            format!("{INSTALLED}Conffiles:\n {CONFIG} d41d8cd98f00b204e9800998ecf8427e\n"),
        )
        .unwrap();
        let error = read(root, || {})
            .err()
            .expect("different conffile and ucf digests must refuse admission");
        assert!(
            error.to_string().contains("conflicting dpkg checksums"),
            "{error}"
        );
    }
    #[test]
    fn dpkg_merges_the_full_spaced_conffile_path_with_ucf() {
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        records(root).unwrap();
        let config = CONFIG.replace("config.xcd", "office config.xcd");
        let template = TEMPLATE.replace("config.xcd", "office config.xcd");
        let db = root.join("var/lib/dpkg");
        fs::write(
            db.join(format!("info/{PACKAGE}.list")),
            format!("{template}\n{config}\n"),
        )
        .unwrap();
        fs::write(
            db.join(format!("info/{PACKAGE}.md5sums")),
            format!("{ABC}  {}\n", &template[1..]),
        )
        .unwrap();
        fs::write(
            root.join("var/lib/ucf/registry"),
            format!("{PACKAGE} {config}\n"),
        )
        .unwrap();

        for flags in [
            "",
            " obsolete",
            " remove-on-upgrade",
            " obsolete remove-on-upgrade",
        ] {
            fs::write(
                db.join("status"),
                format!("{INSTALLED}Conffiles:\n {config} {ABC}{flags}\n"),
            )
            .unwrap();
            assert!(read(root, || {}).is_ok(), "{flags}");
            fs::write(
                db.join("status"),
                format!(
                    "{INSTALLED}Conffiles:\n {config} d41d8cd98f00b204e9800998ecf8427e{flags}\n"
                ),
            )
            .unwrap();
            let error = read(root, || {})
                .err()
                .expect("the complete conffile path must take part in checksum merging");
            assert!(
                error.to_string().contains("conflicting dpkg checksums")
                    && error.to_string().contains(&config[1..]),
                "{error}"
            );
        }
    }
}
