//! Explicit native loading, owned by the runtime. A distribution policy beside
//! the executable is the authority; no caller-supplied host path is accepted.
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::io::Read;
use std::path::Path;
use std::sync::OnceLock;
use willow_abi::native::{Distribution, MAX_POLICY_BYTES, POLICY_FILE};

struct Handle(*mut c_void);
// OS loader handles are process-wide; lookup is finished before publication and
// no handle is unloaded while a Registry's pointers can be used.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            os::close(self.0);
        }
    }
}
struct Loaded {
    _handle: Handle,
    symbols: HashMap<String, usize>,
}
struct Registry {
    libraries: HashMap<String, Loaded>,
}
impl Registry {
    fn load(executable: &Path) -> Result<Self, String> {
        let root = willow_abi::native::directory(executable).map_err(str::to_owned)?;
        let metadata = std::fs::symlink_metadata(&root)
            .map_err(|e| format!("native_distribution_missing: {e}"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err("native_distribution_escape".into());
        }
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        let policy_path = root.join(POLICY_FILE);
        let metadata = std::fs::symlink_metadata(&policy_path).map_err(|e| e.to_string())?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_POLICY_BYTES
        {
            return Err("native_policy_invalid".into());
        }
        let mut bytes = Vec::new();
        std::fs::File::open(policy_path)
            .map_err(|e| e.to_string())?
            .take(MAX_POLICY_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_POLICY_BYTES {
            return Err("native_policy_invalid".into());
        }
        let distribution: Distribution =
            serde_json::from_slice(&bytes).map_err(|e| format!("native_policy_invalid: {e}"))?;
        distribution.validate().map_err(str::to_owned)?;
        // Validate every payload before invoking any library code.
        let mut paths = Vec::with_capacity(distribution.libraries.len());
        for library in &distribution.libraries {
            let path = root.join(&library.policy.distribution_name);
            let metadata = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err("native_distribution_escape".into());
            }
            let path = path.canonicalize().map_err(|e| e.to_string())?;
            if !path.starts_with(&root) {
                return Err("native_distribution_escape".into());
            }
            let mut file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
            let mut hasher = Sha256::new();
            let mut buffer = [0; 65536];
            loop {
                let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
            }
            let actual: String = hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            if actual != library.digest {
                return Err(format!("native_digest_mismatch: {}", library.name));
            }
            paths.push(path);
        }
        let mut libraries = HashMap::new();
        for (library, path) in distribution.libraries.into_iter().zip(paths) {
            let handle = Handle(unsafe { os::open(&path)? });
            let negotiation = CString::new(library.policy.abi_symbol).unwrap();
            let pointer = unsafe { os::symbol(handle.0, &negotiation)? };
            let negotiate: willow_abi::native::Negotiate = unsafe { std::mem::transmute(pointer) };
            if unsafe { negotiate(library.policy.version) } != library.policy.version {
                return Err(format!("native_abi_mismatch: {}", library.name));
            }
            let mut symbols = HashMap::new();
            for symbol in library.policy.symbols {
                let name = CString::new(symbol.as_str()).unwrap();
                let pointer = unsafe { os::symbol(handle.0, &name)? };
                symbols.insert(symbol, pointer as usize);
            }
            libraries.insert(
                library.name,
                Loaded {
                    _handle: handle,
                    symbols,
                },
            );
        }
        Ok(Self { libraries })
    }
    fn symbol(&self, library: &str, symbol: &str) -> Result<*mut c_void, String> {
        self.libraries
            .get(library)
            .and_then(|l| l.symbols.get(symbol))
            .map(|&p| p as *mut c_void)
            .ok_or_else(|| format!("native_symbol_not_permitted: {library}/{symbol}"))
    }
}

/// Return an allowlisted pointer after digest verification and ABI negotiation.
/// Null denotes a validation/load error (diagnostic written to stderr).
/// Libraries and returned pointers remain alive for the process lifetime.
///
/// # Safety
/// Arguments must point to valid NUL-terminated UTF-8 names. The adapter must use
/// the declared C signature for the returned pointer; this API cannot infer it.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_native_symbol_v1(
    library: *const c_char,
    symbol: *const c_char,
) -> *mut c_void {
    static REGISTRY: OnceLock<Result<Registry, String>> = OnceLock::new();
    thread_local! { static INITIALIZING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            INITIALIZING.set(false);
        }
    }
    let result = (|| {
        if library.is_null() || symbol.is_null() {
            return Err("native_invalid_name".into());
        }
        let library = unsafe { CStr::from_ptr(library) }
            .to_str()
            .map_err(|_| "native_invalid_name".to_owned())?;
        let symbol = unsafe { CStr::from_ptr(symbol) }
            .to_str()
            .map_err(|_| "native_invalid_name".to_owned())?;
        if INITIALIZING.get() {
            return Err("native_loader_reentrant".into());
        }
        let registry = REGISTRY
            .get_or_init(|| {
                INITIALIZING.set(true);
                let _reset = Reset;
                let executable = std::env::current_exe().map_err(|e| e.to_string())?;
                Registry::load(&executable)
            })
            .as_ref()
            .map_err(Clone::clone)?;
        registry.symbol(library, symbol)
    })();
    match result {
        Ok(pointer) => pointer,
        Err(error) => {
            eprintln!("{error}");
            std::ptr::null_mut()
        }
    }
}

#[cfg(unix)]
mod os {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    pub unsafe fn open(path: &Path) -> Result<*mut c_void, String> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            Err("native_load_failed".into())
        } else {
            Ok(handle)
        }
    }
    pub unsafe fn symbol(handle: *mut c_void, name: &CStr) -> Result<*mut c_void, String> {
        unsafe {
            libc::dlerror();
        }
        let pointer = unsafe { libc::dlsym(handle, name.as_ptr()) };
        if pointer.is_null() || !unsafe { libc::dlerror() }.is_null() {
            Err(format!("native_symbol_missing: {}", name.to_string_lossy()))
        } else {
            Ok(pointer)
        }
    }
    pub unsafe fn close(handle: *mut c_void) {
        unsafe {
            libc::dlclose(handle);
        }
    }
}
#[cfg(windows)]
mod os {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LoadLibraryExW(path: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
        fn GetProcAddress(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn FreeLibrary(handle: *mut c_void) -> i32;
    }
    pub unsafe fn open(path: &Path) -> Result<*mut c_void, String> {
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // Search dependencies only in the library's own directory and System32.
        let handle = unsafe { LoadLibraryExW(path.as_ptr(), std::ptr::null_mut(), 0x100 | 0x800) };
        if handle.is_null() {
            Err("native_load_failed".into())
        } else {
            Ok(handle)
        }
    }
    pub unsafe fn symbol(handle: *mut c_void, name: &CStr) -> Result<*mut c_void, String> {
        let pointer = unsafe { GetProcAddress(handle, name.as_ptr()) };
        if pointer.is_null() {
            Err(format!("native_symbol_missing: {}", name.to_string_lossy()))
        } else {
            Ok(pointer)
        }
    }
    pub unsafe fn close(handle: *mut c_void) {
        unsafe {
            FreeLibrary(handle);
        }
    }
}
#[cfg(test)]
mod tests;
