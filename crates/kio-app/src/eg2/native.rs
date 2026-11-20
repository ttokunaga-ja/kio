//! The v1.0.0 C ABI from LiteRT-LM v0.19.1. Only the worker calls this
//! module. Handles and their destructors never outlive the loaded image.

use super::runtime::VerifiedLibrary;
use super::{DIMENSIONS, TOKEN_CAP, WireInput, WorkerError, valid_vector};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::ptr::{null, null_mut};

type Handle = *mut c_void;
type Delete = unsafe extern "C" fn(Handle);
type Status = c_int;

pub(super) const C_API_VERSION: &str = "1.0.0";
pub(super) const NORMALIZE: bool = true;
pub(super) const INSERT_SPECIAL_TOKENS: bool = true;

macro_rules! api {
    ($($name:ident: $signature:ty),+ $(,)?) => {
        pub(super) struct Api {
            $($name: $signature,)+
            _library: Library,
        }

        impl Api {
            fn load(library: Library) -> Result<Self, WorkerError> {
                Ok(Self {
                    $($name: {
                        let name = CStr::from_bytes_with_nul(
                            concat!("litert_lm_", stringify!($name), "\0").as_bytes()
                        ).expect("static C export name");
                        let symbol = library.symbol(name)?;
                        // SAFETY: the verified library exports this exact
                        // function signature from the pinned C API headers.
                        unsafe { std::mem::transmute::<*mut c_void, $signature>(symbol) }
                    },)+
                    _library: library,
                })
            }
        }
    }
}

api! {
    embedding_engine_settings_create: unsafe extern "C" fn(*const c_char, *const c_char, *const c_char, *const c_char, *mut Handle) -> Status,
    embedding_engine_settings_delete: Delete,
    embedding_engine_settings_set_num_threads: unsafe extern "C" fn(Handle, c_int) -> Status,
    embedding_engine_settings_set_max_input_length: unsafe extern "C" fn(Handle, c_int) -> Status,
    embedding_engine_settings_set_cache_dir: unsafe extern "C" fn(Handle, *const c_char) -> Status,
    embedding_engine_create: unsafe extern "C" fn(*const c_void, *mut Handle) -> Status,
    embedding_engine_delete: Delete,
    embedding_options_create: unsafe extern "C" fn(*mut Handle) -> Status,
    embedding_options_delete: Delete,
    embedding_options_set_normalize: unsafe extern "C" fn(Handle, bool) -> Status,
    embedding_options_set_insert_special_tokens: unsafe extern "C" fn(Handle, bool) -> Status,
    embedding_options_set_input_overflow_strategy: unsafe extern "C" fn(Handle, c_int) -> Status,
    embedding_options_set_output_size: unsafe extern "C" fn(Handle, c_int) -> Status,
    input_data_create: unsafe extern "C" fn(c_int, *const c_void, usize, *mut Handle) -> Status,
    input_data_delete: Delete,
    embedding_engine_compute_embedding: unsafe extern "C" fn(Handle, *const *const c_void, usize, *const c_void, *mut Handle) -> Status,
    embedding_response_delete: Delete,
    embedding_response_get_size: unsafe extern "C" fn(*const c_void, *mut usize) -> Status,
    embedding_response_get_values: unsafe extern "C" fn(*const c_void, *mut *const f32) -> Status,
    get_last_error_message: unsafe extern "C" fn() -> *const c_char,
    clear_last_error: unsafe extern "C" fn(),
}

struct Object {
    ptr: Handle,
    delete: Delete,
}

impl Object {
    fn create(
        api: &Api,
        delete: Delete,
        create: impl FnOnce(*mut Handle) -> Status,
    ) -> Result<Self, WorkerError> {
        let mut ptr = null_mut();
        api.check(create(&mut ptr))?;
        if ptr.is_null() {
            return Err(WorkerError::Inference);
        }
        Ok(Self { ptr, delete })
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        // SAFETY: Object owns the handle returned by the corresponding create.
        unsafe { (self.delete)(self.ptr) }
    }
}

pub(super) struct Engine {
    options: Object,
    engine: Object,
    // Rust drops fields in declaration order. Delete handles first.
    api: Api,
}

impl Engine {
    pub(super) fn open(
        api: Api,
        model: &Path,
        cache: &Path,
        threads: i32,
    ) -> Result<Self, WorkerError> {
        let model = c_path(model)?;
        let cache = c_path(cache)?;
        // SAFETY: every handle comes from its matching constructor, strings
        // live for the calls that use them, and there is no concurrent use.
        let initialized = unsafe {
            (|| {
                let settings = Object::create(&api, api.embedding_engine_settings_delete, |out| {
                    (api.embedding_engine_settings_create)(
                        model.as_ptr(),
                        c"cpu".as_ptr(),
                        c"cpu".as_ptr(),
                        null(),
                        out,
                    )
                })?;
                api.check((api.embedding_engine_settings_set_num_threads)(
                    settings.ptr,
                    threads,
                ))?;
                api.check((api.embedding_engine_settings_set_max_input_length)(
                    settings.ptr,
                    TOKEN_CAP as c_int,
                ))?;
                api.check((api.embedding_engine_settings_set_cache_dir)(
                    settings.ptr,
                    cache.as_ptr(),
                ))?;
                let options = Object::create(&api, api.embedding_options_delete, |out| {
                    (api.embedding_options_create)(out)
                })?;
                api.check((api.embedding_options_set_normalize)(
                    options.ptr,
                    NORMALIZE,
                ))?;
                api.check((api.embedding_options_set_insert_special_tokens)(
                    options.ptr,
                    INSERT_SPECIAL_TOKENS,
                ))?;
                api.check((api.embedding_options_set_input_overflow_strategy)(
                    options.ptr,
                    2,
                ))?;
                api.check((api.embedding_options_set_output_size)(
                    options.ptr,
                    DIMENSIONS as c_int,
                ))?;
                let engine = Object::create(&api, api.embedding_engine_delete, |out| {
                    (api.embedding_engine_create)(settings.ptr, out)
                })?;
                Ok::<_, WorkerError>((options, engine))
            })()
        };
        let (options, engine) = initialized.map_err(|reason| match reason {
            WorkerError::MemoryLimit => reason,
            _ => WorkerError::RuntimeUnavailable,
        })?;
        Ok(Self {
            options,
            engine,
            api,
        })
    }

    pub(super) fn embed(&mut self, input: WireInput<'_>) -> Result<Vec<f32>, WorkerError> {
        let api = &self.api;
        let (kind, bytes) = match input {
            WireInput::Text(text) => (0, text.as_bytes()),
            WireInput::Image(bytes) => (1, bytes),
        };
        // SAFETY: constructors copy the bounded byte slice; the pointer array
        // and all handles stay live through compute. The response owns values.
        unsafe {
            let data = Object::create(api, api.input_data_delete, |out| {
                (api.input_data_create)(kind, bytes.as_ptr().cast(), bytes.len(), out)
            })?;
            let inputs = [data.ptr.cast_const()];
            let response = Object::create(api, api.embedding_response_delete, |out| {
                (api.embedding_engine_compute_embedding)(
                    self.engine.ptr,
                    inputs.as_ptr(),
                    inputs.len(),
                    self.options.ptr,
                    out,
                )
            })?;
            let mut len = 0;
            api.check((api.embedding_response_get_size)(response.ptr, &mut len))?;
            if len != DIMENSIONS {
                return Err(WorkerError::Protocol);
            }
            let mut values = null();
            api.check((api.embedding_response_get_values)(
                response.ptr,
                &mut values,
            ))?;
            if values.is_null() {
                return Err(WorkerError::Protocol);
            }
            let vector = std::slice::from_raw_parts(values, len);
            if !valid_vector(vector) {
                return Err(WorkerError::Protocol);
            }
            Ok(vector.to_vec())
        }
    }
}

impl Api {
    pub(super) fn open(library: VerifiedLibrary) -> Result<Self, WorkerError> {
        Self::load(Library::open(library)?)
    }

    fn check(&self, status: Status) -> Result<(), WorkerError> {
        if status == 0 {
            return Ok(());
        }
        // SAFETY: the API returns a thread-local NUL-terminated diagnostic.
        // Only inspect a small prefix; raw diagnostics never leave this process.
        let mut bytes = [0; 128];
        let mut len = 0;
        unsafe {
            if status == 3 {
                let message = (self.get_last_error_message)();
                if !message.is_null() {
                    for (index, byte) in bytes.iter_mut().enumerate() {
                        *byte = *message.add(index) as u8;
                        if *byte == 0 {
                            len = index;
                            break;
                        }
                    }
                }
            }
            (self.clear_last_error)();
        }
        Err(classify_error(status, &bytes[..len]))
    }
}

pub(super) fn classify_error(status: Status, diagnostic: &[u8]) -> WorkerError {
    match status {
        3 if is_input_overflow(diagnostic) => WorkerError::InputOverflow,
        8 => WorkerError::MemoryLimit,
        _ => WorkerError::Inference,
    }
}

fn is_input_overflow(bytes: &[u8]) -> bool {
    const PREFIX: &str = "Input sequence length (";
    const SUFFIX: &str = ") exceeds maximum supported signature length (2048).";
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|message| message.strip_prefix(PREFIX)?.strip_suffix(SUFFIX))
        .filter(|digits| digits.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|digits| digits.parse::<u64>().ok())
        .is_some_and(|length| length > u64::from(TOKEN_CAP))
}

fn c_path(path: &Path) -> Result<CString, WorkerError> {
    #[cfg(unix)]
    let bytes = path.as_os_str().as_encoded_bytes();
    #[cfg(windows)]
    let bytes = path.to_str().ok_or(WorkerError::Startup)?.as_bytes();
    CString::new(bytes).map_err(|_| WorkerError::Startup)
}

struct Library {
    handle: *mut c_void,
    _verified: VerifiedLibrary,
}

impl Library {
    #[cfg(unix)]
    fn open(verified: VerifiedLibrary) -> Result<Self, WorkerError> {
        #[cfg(target_os = "linux")]
        let path = {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::MetadataExt;
            // Confirm the verified file is still the distribution entry and
            // load that retained descriptor, even if the name is replaced.
            let bound = verified
                .file
                .metadata()
                .map_err(|_| WorkerError::RuntimeUnavailable)?;
            let named = verified
                .path
                .symlink_metadata()
                .map_err(|_| WorkerError::RuntimeUnavailable)?;
            if bound.dev() != named.dev() || bound.ino() != named.ino() {
                return Err(WorkerError::RuntimeUnavailable);
            }
            std::path::PathBuf::from(format!("/proc/self/fd/{}", verified.file.as_raw_fd()))
        };
        #[cfg(not(target_os = "linux"))]
        let path = {
            use std::os::unix::fs::MetadataExt;
            let bound = verified
                .file
                .metadata()
                .map_err(|_| WorkerError::RuntimeUnavailable)?;
            let named = verified
                .path
                .symlink_metadata()
                .map_err(|_| WorkerError::RuntimeUnavailable)?;
            if bound.dev() != named.dev() || bound.ino() != named.ino() {
                return Err(WorkerError::RuntimeUnavailable);
            }
            verified.path.clone()
        };
        let path = c_path(&path)?;
        // SAFETY: path is a live C string for the verified image.
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(WorkerError::RuntimeUnavailable);
        }
        Ok(Self {
            handle,
            _verified: verified,
        })
    }

    #[cfg(windows)]
    fn open(verified: VerifiedLibrary) -> Result<Self, WorkerError> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::System::LibraryLoader::{
            LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
        };

        let path: Vec<_> = verified.path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: path is fully qualified and NUL-terminated. The verification
        // handle denies writes/deletion; dependent libraries resolve in System32.
        let handle =
            unsafe { LoadLibraryExW(path.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        if handle.is_null() {
            return Err(WorkerError::RuntimeUnavailable);
        }
        Ok(Self {
            handle,
            _verified: verified,
        })
    }

    fn symbol(&self, name: &CStr) -> Result<*mut c_void, WorkerError> {
        // SAFETY: the loaded image and terminated export name are both live.
        #[cfg(unix)]
        let ptr = unsafe { libc::dlsym(self.handle, name.as_ptr()) };
        #[cfg(windows)]
        let ptr = unsafe {
            windows_sys::Win32::System::LibraryLoader::GetProcAddress(
                self.handle,
                name.as_ptr().cast(),
            )
            .map_or(null_mut(), |symbol| symbol as *mut c_void)
        };
        if ptr.is_null() {
            Err(WorkerError::RuntimeUnavailable)
        } else {
            Ok(ptr)
        }
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: Library owns one loader reference to a non-null image.
        #[cfg(unix)]
        unsafe {
            libc::dlclose(self.handle);
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::FreeLibrary(self.handle);
        }
    }
}
