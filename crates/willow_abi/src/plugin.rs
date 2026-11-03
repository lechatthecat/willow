//! Owned-byte plugin protocol. Integer fields are little endian; no native layout is sent.
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const HEADER_BYTES: usize = 48;
pub const WASM_ENGINE: &str = "wasmi-2.0.0-portable";
pub const NATIVE_ENGINE: &str = "bubblewrap-seccomp-x86_64-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Wasm,
    NativeProcess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Error {
    InvalidManifest,
    DigestMismatch,
    IsolationUnavailable,
    DeniedCapability,
    ResourceLimit,
    ProtocolMismatch,
    Trap,
    Crash,
    Timeout,
    Cancelled,
    Stopped,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    pub artifact_bytes: usize,
    pub memory_bytes: usize,
    pub fuel: u64,
    pub deadline_ms: u32,
    pub requests: usize,
    pub payload_bytes: usize,
    pub queued_bytes: usize,
    pub resources: usize,
    pub resource_bytes: usize,
}
impl Default for Budgets {
    fn default() -> Self {
        Self {
            artifact_bytes: 4 << 20,
            memory_bytes: 64 << 20,
            fuel: 1_000_000,
            deadline_ms: 1000,
            requests: 16,
            payload_bytes: 65536,
            queued_bytes: 2 << 20,
            resources: 16,
            resource_bytes: 1 << 20,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub digest: String,
    pub mode: Mode,
    pub engine: String,
    /// WASM uses `wasm32`; native plugins must match the exact supported target.
    pub target: String,
    pub methods: Vec<u32>,
    /// Names of immutable byte resources. Other capabilities are unavailable.
    pub read_resources: Vec<String>,
    pub budgets: Budgets,
}
impl Manifest {
    /// Parse a bounded manifest supplied by an untrusted distribution.
    pub fn from_json(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > 65536 {
            return Err(Error::ResourceLimit);
        }
        let manifest: Self = serde_json::from_slice(bytes).map_err(|_| Error::InvalidManifest)?;
        manifest.validate()?;
        Ok(manifest)
    }
    pub fn validate(&self) -> Result<(), Error> {
        let b = &self.budgets;
        if self.version != VERSION
            || self.digest.len() != 64
            || !self
                .digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.methods.is_empty()
            || self.methods.len() > 4096
            || self
                .methods
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != self.methods.len()
            || self.read_resources.len() > b.resources
            || self
                .read_resources
                .iter()
                .any(|n| !super::native::identifier(n))
            || self
                .read_resources
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != self.read_resources.len()
            || !(1..=16 << 20).contains(&b.artifact_bytes)
            || !(65536..=256 << 20).contains(&b.memory_bytes)
            || !(1..=1_000_000_000).contains(&b.fuel)
            || !(1..=60000).contains(&b.deadline_ms)
            || !(1..=4096).contains(&b.requests)
            || !(1..=1 << 20).contains(&b.payload_bytes)
            || b.queued_bytes < b.payload_bytes
            || b.queued_bytes > 64 << 20
            || b.resources > 1024
            || b.resource_bytes > 16 << 20
        {
            return Err(Error::InvalidManifest);
        }
        match self.mode {
            Mode::Wasm if self.engine == WASM_ENGINE && self.target == "wasm32" => Ok(()),
            Mode::NativeProcess
                if self.engine == NATIVE_ENGINE && self.target == "x86_64-unknown-linux-gnu" =>
            {
                Ok(())
            }
            _ => Err(Error::IsolationUnavailable),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Tag {
    Request = 1,
    Response = 2,
    Error = 3,
    Cancel = 4,
    Callback = 5,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub tag: Tag,
    pub method: u32,
    pub generation: u64,
    pub request: u64,
    pub resource: u64,
    pub length: usize,
}
impl Header {
    pub fn encode(self) -> [u8; HEADER_BYTES] {
        let mut bytes = [0; HEADER_BYTES];
        bytes[..4].copy_from_slice(b"WPLG");
        bytes[4..8].copy_from_slice(&VERSION.to_le_bytes());
        bytes[8..12].copy_from_slice(&(self.tag as u32).to_le_bytes());
        bytes[12..16].copy_from_slice(&self.method.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.generation.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.request.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.resource.to_le_bytes());
        bytes[40..48].copy_from_slice(&(self.length as u64).to_le_bytes());
        bytes
    }
    /// Validate the fixed-size header before allocating a body.
    pub fn decode(bytes: &[u8; HEADER_BYTES], limit: usize) -> Result<Self, Error> {
        let u32_at = |i| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
        let u64_at = |i| u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap());
        if &bytes[..4] != b"WPLG" || u32_at(4) != VERSION {
            return Err(Error::ProtocolMismatch);
        }
        let tag = match u32_at(8) {
            1 => Tag::Request,
            2 => Tag::Response,
            3 => Tag::Error,
            4 => Tag::Cancel,
            5 => Tag::Callback,
            _ => return Err(Error::ProtocolMismatch),
        };
        let length = usize::try_from(u64_at(40)).map_err(|_| Error::ResourceLimit)?;
        if length > limit {
            return Err(Error::ResourceLimit);
        }
        Ok(Self {
            tag,
            method: u32_at(12),
            generation: u64_at(16),
            request: u64_at(24),
            resource: u64_at(32),
            length,
        })
    }
}
