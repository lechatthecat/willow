//! Versioned, executable-relative native library distribution contract.
//! Negotiation export: `uint32_t negotiate(uint32_t requested)`; return the
//! requested version on success, or zero when unsupported. Negotiation must not
//! recursively initialize the loader (including via another thread). No Rust ABI crosses
//! this interface. The symbol API returns null on policy/load/negotiation errors.
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const SCHEMA: u32 = 1;
pub const MAX_POLICY_BYTES: u64 = 16 * 1024 * 1024;
pub const POLICY_FILE: &str = "manifest.json";
pub type Negotiate = unsafe extern "C" fn(u32) -> u32;
pub type Symbol =
    unsafe extern "C" fn(*const std::ffi::c_char, *const std::ffi::c_char) -> *mut std::ffi::c_void;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LoaderPolicy {
    pub distribution_name: String,
    pub abi_symbol: String,
    pub version: u32,
    pub symbols: Vec<String>,
    /// Explicit consent to the platform loader's transitive OS dependencies.
    /// The primary library itself is always executable-relative and hashed.
    pub dependency_search: DependencySearch,
    pub platform_provider: String,
    pub platform_version: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DependencySearch {
    Platform,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Library {
    pub name: String,
    pub digest: String,
    pub policy: LoaderPolicy,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Distribution {
    pub schema: u32,
    /// Dependencies precede their consumers. Only explicitly permitted entries
    /// are materialized; adapters cannot supply their own paths.
    pub libraries: Vec<Library>,
}
pub fn identifier(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
pub fn filename(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
    {
        return false;
    }

    !name.is_empty()
        && name.len() <= 255
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        && !name.ends_with('.')
        && name != POLICY_FILE
}
impl LoaderPolicy {
    pub fn valid(&self) -> bool {
        !self.platform_provider.is_empty()
            && self.platform_provider.len() <= 255
            && !self.platform_version.is_empty()
            && self.platform_version.len() <= 255
            && filename(&self.distribution_name)
            && identifier(&self.abi_symbol)
            && self.version != 0
            && !self.symbols.is_empty()
            && self.symbols.iter().all(|s| identifier(s))
            && self.symbols.iter().collect::<HashSet<_>>().len() == self.symbols.len()
    }
}
impl Distribution {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != SCHEMA {
            return Err("native_policy_version");
        }
        let mut names = HashSet::new();
        let mut files = HashSet::new();
        for library in &self.libraries {
            if !identifier(&library.name)
                || !library.policy.valid()
                || library.digest.len() != 64
                || !library
                    .digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                || !names.insert(&library.name)
                || !files.insert(library.policy.distribution_name.to_ascii_lowercase())
            {
                return Err("native_policy_invalid");
            }
        }
        Ok(())
    }
}
pub fn directory(executable: &Path) -> Result<PathBuf, &'static str> {
    let mut name = executable
        .file_name()
        .ok_or("native_executable_path")?
        .to_os_string();
    name.push(".native");
    Ok(executable.with_file_name(name))
}
