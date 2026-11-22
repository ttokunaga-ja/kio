use crate::office_runtime_identity::RuntimeSnapshot;
use crate::{AdapterError, Result};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};
use tempfile::Builder;
pub(crate) struct PrivateOfficeScratch {
    original: std::sync::Arc<std::fs::File>,
    pin: Option<std::fs::File>,
    parent: kio_core::store_dir::StoreDirectory,
    leaf: PathBuf,
    path: PathBuf,
    cleanup: bool,
}
impl PrivateOfficeScratch {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
    pub(crate) fn keep(mut self) -> PathBuf {
        self.cleanup = false;
        self.path.clone()
    }
    fn remove_owned(&mut self) -> Result<()> {
        use kio_core::store_dir::{PlannedDirectoryRemoval, RemovalBudget};
        let original = kio_core::store_dir::StoreDirectory::from_retained(
            self.original.try_clone().map_err(|error| {
                AdapterError::ContractViolation(format!(
                    "cannot retain scratch cleanup authority: {error}"
                ))
            })?,
            self.path.clone(),
        )
        .map_err(|error| {
            AdapterError::ContractViolation(format!(
                "cannot validate scratch cleanup authority: {error}"
            ))
        })?;
        // An active or unretired invocation still has its SID ACE. Refuse
        // destruction on unwind as well as normal errors until owner-only.
        kio_core::private_fs::verify_private_directory_handle(&original).map_err(|error| {
            AdapterError::ContractViolation(format!(
                "scratch remains under invocation/recovery authority: {error}"
            ))
        })?;
        let current = self.parent.open_directory(&self.leaf).map_err(|error| {
            AdapterError::ContractViolation(format!("cannot bind Office scratch cleanup: {error}"))
        })?;
        let expected = kio_core::platform::directory_handle_identity(&self.original);
        if expected.is_none()
            || kio_core::platform::directory_handle_identity(&current) != expected
            || self
                .pin
                .as_ref()
                .and_then(kio_core::platform::directory_handle_identity)
                != expected
        {
            return Err(AdapterError::ContractViolation(
                "Office scratch changed identity before cleanup".to_owned(),
            ));
        }
        // The creation-time strong pin keeps the original name fixed while the
        // core captures its exact bounded tree. No absolute recursive deletion.
        let plan =
            PlannedDirectoryRemoval::capture(&self.parent, &self.leaf, &mut RemovalBudget::new())
                .map_err(|error| {
                    AdapterError::ContractViolation(format!(
                        "cannot capture Office scratch cleanup: {error}"
                    ))
                })?
                .ok_or_else(|| {
                    AdapterError::ContractViolation(
                        "Office scratch disappeared before cleanup".to_owned(),
                    )
                })?;
        drop(current);
        drop(original);
        // DELETE mutator opens must not overlap our no-delete-sharing pin.
        drop(self.pin.take());
        if !plan.remove().map_err(|error| {
            AdapterError::ContractViolation(format!("Office scratch cleanup refused: {error}"))
        })? {
            return Err(AdapterError::ContractViolation(
                "Office scratch identity changed before retirement".to_owned(),
            ));
        }
        Ok(())
    }
    pub(crate) fn close(mut self) -> Result<()> {
        self.cleanup = false;
        self.remove_owned().map_err(|error| AdapterError::ContractViolation(format!(
            "{error}; cleanup incomplete for original Office scratch {}; any core quarantine remains for recovery", self.path.display())))
    }
}
impl Drop for PrivateOfficeScratch {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = self.remove_owned();
        }
    }
}

pub(crate) fn private_temp_dir() -> Result<PrivateOfficeScratch> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
    };
    let temp = std::fs::canonicalize(std::env::temp_dir()).map_err(|error| {
        AdapterError::ContractViolation(format!("cannot bind Office temporary parent: {error}"))
    })?;
    let parent = kio_core::private_fs::verify_private_creation_parent(&temp).map_err(|error| {
        AdapterError::ContractViolation(format!("cannot retain Office temporary parent: {error}"))
    })?;
    let allocation = Builder::new()
        .prefix("kio-")
        .disable_cleanup(true)
        .make_in(&temp, |path| {
            let leaf = path
                .file_name()
                .ok_or_else(|| std::io::Error::other("invalid Office scratch leaf"))?;
            parent
                .create_directory(Path::new(leaf))
                .map_err(std::io::Error::other)
        })
        .map_err(|error| {
            AdapterError::ContractViolation(format!(
                "cannot create private Office scratch: {error}"
            ))
        })?;
    let (created, temporary_path) = allocation.into_parts();
    let path = temporary_path.to_path_buf();
    // File-ID normalization preserves the exclusively created object and drops
    // creation-only directory write rights before read-only monitor pins open.
    let directory = kio_core::store_dir::StoreDirectory::from_retained(created, path.clone())
        .map_err(|error| {
            AdapterError::ContractViolation(format!(
                "cannot retain created Office scratch: {error}"
            ))
        })?;
    kio_core::private_fs::verify_private_directory_handle(&directory).map_err(|error| {
        AdapterError::ContractViolation(format!("created Office scratch is unsafe: {error}"))
    })?;
    let original = directory.root_handle();
    let leaf = PathBuf::from(path.file_name().ok_or_else(|| {
        AdapterError::ContractViolation("invalid Office scratch name".to_owned())
    })?);
    let pin = std::fs::OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | READ_CONTROL)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path)
        .map_err(|error| {
            AdapterError::ContractViolation(format!("cannot pin private Office scratch: {error}"))
        })?;
    let expected = kio_core::platform::directory_handle_identity(&original);
    let current = parent.open_directory(&leaf).map_err(|error| {
        AdapterError::ContractViolation(format!("cannot bind created Office scratch: {error}"))
    })?;
    if expected.is_none()
        || kio_core::platform::directory_handle_identity(&pin) != expected
        || kio_core::platform::directory_handle_identity(&current) != expected
    {
        return Err(AdapterError::ContractViolation(
            "created Office scratch changed identity".to_owned(),
        ));
    }
    drop(current);
    Ok(PrivateOfficeScratch {
        original,
        pin: Some(pin),
        parent,
        leaf,
        path,
        cleanup: true,
    })
}

pub(crate) fn restrict_renderer_state_directory(
    scratch: &kio_core::store_dir::StoreDirectory,
    child: &str,
    purpose: &str,
    label: &str,
) -> Result<()> {
    kio_core::private_fs::verify_private_directory_at(scratch, Path::new(child)).map_err(
        |error| {
            AdapterError::ContractViolation(format!("unsafe private {purpose} {label}: {error}"))
        },
    )?;
    Ok(())
}
#[derive(Debug, Clone)]
pub(crate) struct OfficeRuntime {
    package: PathBuf,
    windows: PathBuf,
}
impl OfficeRuntime {
    pub(crate) fn verify_captured_signature(
        &self,
        program: &Path,
        snapshot: &RuntimeSnapshot,
        deadline: std::time::Instant,
    ) -> std::io::Result<()> {
        self.verify_runtime_signatures(program, &snapshot.pin_before_launch(deadline)?, deadline)
    }

    pub(crate) fn new(program: &Path, _deadline: std::time::Instant) -> Result<Self> {
        Ok(Self {
            package: installed_package(program)?,
            windows: windows_directory()?,
        })
    }
    #[cfg(test)]
    pub(crate) fn fixture(package: PathBuf) -> Self {
        Self {
            package,
            windows: windows_directory().unwrap(),
        }
    }
    pub(crate) fn catalog_digest(&self) -> Option<&str> {
        None
    }
    pub(crate) fn package_roots(&self, _program: &Path) -> Result<Vec<PathBuf>> {
        Ok(vec![self.package.clone()])
    }
    pub(crate) fn configure_services(
        &self,
        _scratch: &Path,
        _environment: &mut Vec<(OsString, OsString)>,
        _deadline: std::time::Instant,
    ) -> Result<()> {
        Ok(())
    }
    pub(crate) fn environment(
        &self,
        _program: &Path,
        home: Option<&Path>,
    ) -> Vec<(OsString, OsString)> {
        renderer_environment(&self.windows, home)
    }
    pub(crate) fn runtime_roots(&self, _program: &Path) -> Result<Vec<PathBuf>> {
        Ok(vec![self.windows.clone()])
    }

    pub(crate) fn verify_runtime_signatures(
        &self,
        program: &Path,
        entries: &[(&Path, std::fs::File)],
        deadline: std::time::Instant,
    ) -> std::io::Result<()> {
        use std::os::windows::fs::FileExt as _;

        let binaries = self.package.join("program");
        let renderer = binaries.join("soffice.bin");
        let mut launcher_verified = false;
        let mut renderer_verified = false;
        for &(path, ref file) in entries {
            crate::office_runtime_identity::check_verification_deadline(deadline)?;
            if file.metadata()?.is_dir() {
                continue;
            }
            let required = path == program || path == renderer;
            let mut header = [0; 2];
            if file.seek_read(&mut header, 0)? == 2 && &header == b"MZ" {
                let allow_unsigned = !required
                    && path.starts_with(&binaries)
                    && path.parent() != Some(binaries.as_path());
                verify_authenticode_until(path, file, deadline, allow_unsigned).map_err(
                    |error| {
                        std::io::Error::new(
                            error.kind(),
                            format!(
                                "Authenticode verification failed for {}: {error}",
                                path.display()
                            ),
                        )
                    },
                )?;
                launcher_verified |= path == program;
                renderer_verified |= path == renderer;
            } else if required {
                return Err(std::io::Error::other(format!(
                    "Windows Office executable is not a PE image: {}",
                    path.display()
                )));
            }
        }
        if !launcher_verified || !renderer_verified {
            return Err(std::io::Error::other(
                "Windows Office runtime lacks verified executable images",
            ));
        }
        Ok(())
    }
}

fn windows_directory() -> Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;

    let mut path = [0; windows_sys::Win32::Foundation::MAX_PATH as usize];
    // The OS writes the Windows directory into the supplied UTF-16 buffer.
    let len = unsafe { GetSystemWindowsDirectoryW(path.as_mut_ptr(), path.len() as u32) } as usize;
    if len == 0 || len >= path.len() {
        return Err(AdapterError::ContractViolation(
            "cannot determine the Windows system directory for Office".to_owned(),
        ));
    }
    Ok(PathBuf::from(OsString::from_wide(&path[..len])))
}

fn installed_package(program: &Path) -> Result<PathBuf> {
    let invalid = || {
        AdapterError::ContractViolation(format!(
            "unsupported Windows Office runtime placement for {}: expected program/soffice.exe, program/soffice.bin, program/fundamental.ini and share/",
            program.display()
        ))
    };

    let binaries = program.parent().ok_or_else(invalid)?;
    let package = binaries.parent().ok_or_else(invalid)?;
    if package.parent().is_none()
        || !program.is_absolute()
        || !binaries.ends_with("program")
        || !program.ends_with("soffice.exe")
    {
        return Err(invalid());
    }
    for directory in [package, binaries, &package.join("share")] {
        if !kio_core::platform::directory_is_real(directory).map_err(|_| invalid())? {
            return Err(invalid());
        }
    }
    for file in [
        program,
        &binaries.join("soffice.bin"),
        &binaries.join("fundamental.ini"),
    ] {
        if !kio_core::platform::regular_file_is_safe(file).map_err(|_| invalid())? {
            return Err(invalid());
        }
    }
    Ok(package.to_path_buf())
}

fn verify_authenticode_until(
    path: &Path,
    file: &std::fs::File,
    deadline: std::time::Instant,
    allow_unsigned: bool,
) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::TRUST_E_NOSIGNATURE;

    static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let path = path.to_path_buf();
    let file = file.try_clone()?;
    // The worker owns only the active image's duplicate. The snapshot's
    // other pins stay with the caller and are dropped if this wait times out.
    crate::platform::office_signature_worker::verify_until(&BUSY, deadline, move || {
        match verify_authenticode(&path, &file) {
            Err(error) if allow_unsigned && error.raw_os_error() == Some(TRUST_E_NOSIGNATURE) => {
                // WinVerifyTrust can also report NOSIGNATURE for an invalid
                // embedded signature. Confirm absence using the same pinned file.
                if has_embedded_certificate(&file)? {
                    return Err(std::io::Error::other(
                        "embedded certificate is present but no valid Authenticode signature was found",
                    ));
                }
                Ok(())
            }
            result => result,
        }
    })
}

fn has_embedded_certificate(file: &std::fs::File) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::System::Diagnostics::Debug::{
        CERT_SECTION_TYPE_ANY, ImageEnumerateCertificates,
    };

    let mut count = 0;
    // The caller serializes ImageHlp through the signature worker. Only a
    // count is requested, with a live FILE_READ_DATA handle and output pointer.
    let enumerated = unsafe {
        ImageEnumerateCertificates(
            file.as_raw_handle(),
            CERT_SECTION_TYPE_ANY as u16,
            &raw mut count,
            std::ptr::null_mut(),
            0,
        )
    };
    if enumerated == 0 {
        return Err(std::io::Error::other(format!(
            "could not inspect embedded certificates: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(count != 0)
}

fn verify_authenticode(path: &Path, file: &std::fs::File) -> std::io::Result<()> {
    use std::os::windows::{ffi::OsStrExt as _, io::AsRawHandle as _};
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        Security::WinTrust::{
            WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
            WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE, WTD_DISABLE_MD2_MD4,
            WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE, WinVerifyTrust,
        },
    };

    let wide_path: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut subject = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: wide_path.as_ptr(),
        hFile: file.as_raw_handle(),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &raw mut subject,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_DISABLE_MD2_MD4,
        ..Default::default()
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // The path, open file handle and structures stay live across VERIFY and
    // CLOSE. WinVerifyTrust returns zero for success, not an HRESULT range.
    let status = unsafe {
        WinVerifyTrust(
            INVALID_HANDLE_VALUE,
            &raw mut action,
            (&raw mut data).cast(),
        )
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        WinVerifyTrust(
            INVALID_HANDLE_VALUE,
            &raw mut action,
            (&raw mut data).cast(),
        )
    };
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status));
    }
    Ok(())
}

pub(crate) fn file_url(path: &Path) -> Result<String> {
    let text = path.to_str().ok_or_else(|| {
        AdapterError::ContractViolation("office scratch path is not valid Unicode".to_owned())
    })?;
    Ok(format!(
        "file:///{}",
        crate::office_convert::encode_file_url_path(&text.replace('\\', "/"))
    ))
}

pub(crate) fn renderer_environment(
    windows: &Path,
    home: Option<&Path>,
) -> Vec<(OsString, OsString)> {
    let mut environment = ["SystemRoot", "WINDIR"]
        .map(|name| (OsString::from(name), windows.as_os_str().to_owned()))
        .to_vec();
    if let Ok(path) = std::env::join_paths([windows.join("System32"), windows.to_path_buf()]) {
        environment.push((OsString::from("PATH"), path));
    }
    if let Some(home) = home {
        environment.push((OsString::from("HOME"), home.as_os_str().to_owned()));
        for name in ["USERPROFILE", "LOCALAPPDATA"] {
            environment.push((OsString::from(name), home.as_os_str().to_owned()));
        }
    }
    environment
}

pub(crate) fn scratch_recovery_note(scratch: PrivateOfficeScratch) -> String {
    format!(
        "; recovery scratch retained at {}",
        scratch.keep().display()
    )
}
pub(crate) fn finish_scratch(scratch: PrivateOfficeScratch) -> Result<()> {
    scratch.close()
}

pub(crate) fn office_package_launch_roots(roots: &[PathBuf]) -> &[PathBuf] {
    // Bind the installed package as part of the verified renderer runtime.
    roots
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;

    #[test]
    fn authenticode_accepts_the_system_pe_and_rejects_modified_code() {
        use std::{
            os::windows::fs::{FileExt as _, OpenOptionsExt as _},
            time::{Duration, Instant},
        };
        use windows_sys::Win32::{
            Foundation::TRUST_E_BAD_DIGEST, Storage::FileSystem::FILE_SHARE_READ,
        };

        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let system_image = windows_directory().unwrap().join("System32/ntoskrnl.exe");
        let directory = tempfile::tempdir().unwrap();
        let renderer = directory.path().canonicalize().unwrap().join("soffice.bin");
        let mut bytes = std::fs::read(system_image).unwrap();
        std::fs::write(&renderer, &bytes).unwrap();
        let verify = || {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(&renderer)
                .unwrap();
            let mut magic = [0; 2];
            assert_eq!(file.seek_read(&mut magic, 0).unwrap(), 2);
            assert_eq!(&magic, b"MZ");
            verify_authenticode_until(
                &renderer,
                &file,
                Instant::now() + Duration::from_secs(10),
                true,
            )
        };
        verify().expect(
            "an unchanged embedded-signed Windows kernel PE is accepted under the renderer name",
        );

        let pe = u32::from_le_bytes(bytes[0x3c..0x40].try_into().unwrap()) as usize;
        assert_eq!(&bytes[pe..pe + 4], b"PE\0\0");
        let fields = &bytes[pe + 4..pe + 24];
        let sections = usize::from(u16::from_le_bytes(fields[2..4].try_into().unwrap()));
        let optional_size = usize::from(u16::from_le_bytes(fields[16..18].try_into().unwrap()));
        let table = pe + 24 + optional_size;
        let text = bytes[table..table + sections * 40]
            .as_chunks::<40>()
            .0
            .iter()
            .find(|section| section.starts_with(b".text\0"))
            .expect("Windows kernel has a .text section");
        let offset = u32::from_le_bytes(text[20..24].try_into().unwrap()) as usize;
        bytes[offset] ^= 1;
        std::fs::write(&renderer, &bytes).unwrap();
        let error = verify().unwrap_err();
        assert_eq!(error.raw_os_error(), Some(TRUST_E_BAD_DIGEST), "{error}");
    }

    #[test]
    fn timed_out_verification_retains_only_the_worker_file_until_it_returns() {
        use std::{
            os::windows::fs::OpenOptionsExt as _,
            sync::{
                atomic::{AtomicBool, Ordering},
                mpsc,
            },
            time::{Duration, Instant},
        };
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

        static BUSY: AtomicBool = AtomicBool::new(false);
        let directory = tempfile::tempdir().unwrap();
        let active = directory.path().join("active.dll");
        let other = directory.path().join("other.dll");
        for path in [&active, &other] {
            std::fs::write(path, b"fixture").unwrap();
        }
        let pin = |path: &Path| {
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(path)
                .unwrap()
        };
        let active_pin = pin(&active);
        let other_pin = pin(&other);
        let worker_pin = active_pin.try_clone().unwrap();
        let (release, blocked) = mpsc::channel();
        let error = crate::platform::office_signature_worker::verify_until(
            &BUSY,
            Instant::now() + Duration::from_millis(250),
            move || {
                let _ = blocked.recv();
                drop(worker_pin);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        drop((active_pin, other_pin));
        let write = |path: &Path| std::fs::OpenOptions::new().write(true).open(path);
        assert!(write(&other).is_ok());
        assert!(write(&active).is_err());
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while BUSY.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(write(&active).is_ok());
    }

    #[test]
    fn authenticode_rejects_an_unsigned_image() {
        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().canonicalize().unwrap();
        let binaries = package.join("program");
        std::fs::create_dir(&binaries).unwrap();
        std::fs::create_dir(package.join("share")).unwrap();
        std::fs::write(binaries.join("fundamental.ini"), b"bootstrap").unwrap();
        let program = binaries.join("soffice.exe");
        let test_image = std::env::current_exe().unwrap();
        std::fs::copy(&test_image, &program).unwrap();
        std::fs::copy(&test_image, binaries.join("soffice.bin")).unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let runtime = OfficeRuntime::new(&program, deadline).unwrap();
        let sources =
            kio_process::confinement::RenderSources::resolve(&program, [package.clone()]).unwrap();
        let snapshot =
            crate::office_runtime_identity::RuntimeSnapshot::new(vec![package], &sources, deadline)
                .unwrap();
        let pins = snapshot.pin_before_launch(deadline).unwrap();
        let error = runtime
            .verify_runtime_signatures(&program, &pins, deadline)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Authenticode verification failed"),
            "{error}"
        );
        let unsigned = std::fs::File::open(&program).unwrap();
        let error = verify_authenticode(&program, &unsigned).unwrap_err();
        assert_eq!(
            error.raw_os_error(),
            Some(windows_sys::Win32::Foundation::TRUST_E_NOSIGNATURE),
            "{error}"
        );
    }

    #[test]
    fn authenticode_allows_unsigned_images_only_in_program_subdirectories() {
        use std::{
            fs,
            time::{Duration, Instant},
        };

        let _lock = kio_core::test_control::test_env_lock().lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().canonicalize().unwrap();
        let binaries = package.join("program");
        let intl = binaries.join("intl");
        fs::create_dir_all(&intl).unwrap();
        let share = package.join("share");
        fs::create_dir(&share).unwrap();
        fs::write(binaries.join("fundamental.ini"), b"bootstrap").unwrap();
        let program = binaries.join("soffice.exe");
        let signed = windows_directory().unwrap().join("System32/ntoskrnl.exe");
        fs::copy(&signed, &program).unwrap();
        fs::copy(&signed, binaries.join("soffice.bin")).unwrap();
        let unsigned = std::env::current_exe().unwrap();
        fs::copy(&unsigned, intl.join("fbintl.dll")).unwrap();

        let verify = || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let runtime = OfficeRuntime::new(&program, deadline).unwrap();
            let sources =
                kio_process::confinement::RenderSources::resolve(&program, [package.clone()])
                    .unwrap();
            let snapshot = RuntimeSnapshot::new(vec![package.clone()], &sources, deadline).unwrap();
            runtime.verify_captured_signature(&program, &snapshot, deadline)
        };
        verify().expect("an unsigned PE below program/ is permitted");
        for rejected in [binaries.join("extra.dll"), share.join("extra.dll")] {
            fs::copy(&unsigned, &rejected).unwrap();
            let error = verify().unwrap_err();
            let reason = error.to_string();
            assert!(
                reason.contains("Authenticode verification failed")
                    && reason.contains(rejected.to_string_lossy().as_ref()),
                "{reason}"
            );
            fs::remove_file(rejected).unwrap();
        }
    }

    #[test]
    fn discovers_the_package_from_its_runtime_layout() {
        let directory = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let package = base.join("Alternate Office");
        let program = package.join("program/soffice.exe");
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        std::fs::create_dir(package.join("share")).unwrap();
        std::fs::write(&program, b"launcher").unwrap();
        assert!(installed_package(&program).is_err());
        std::fs::write(package.join("program/soffice.bin"), b"renderer").unwrap();
        std::fs::write(package.join("program/fundamental.ini"), b"bootstrap").unwrap();
        assert_eq!(installed_package(&program).unwrap(), package);
    }
}

pub(crate) const DEFAULT_OFFICE_CONVERTER_PROGRAM: &str = "soffice.exe";
