//! Bridge type IR shared by declarations, adapters, and metadata.
//! `Scalar` retains its R2 public name for API compatibility.
use crate::qualified_name::QualifiedName;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write;

pub(crate) const WRAPPER_SCHEMA: &str = "11";

pub type RustBridgeSymbols = Vec<((u32, String), RustBridgeSymbol)>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scalar {
    I64,
    I8,
    I16,
    I32,
    U16,
    U32,
    U64,
    Char,
    U8,
    F64,
    Bool,
    Void,
    String,
    Bytes,
    Array(Box<Scalar>),
    Record(String),
    Opaque(String),
    Option(Box<Scalar>),
    Result(Box<Scalar>, Box<Scalar>),
}
impl Scalar {
    pub fn abi_type(&self) -> Option<willow_abi::AbiTy> {
        Some(match self {
            Self::I64 | Self::U64 => willow_abi::AbiTy::I64,
            Self::I32 | Self::U32 | Self::Char => willow_abi::AbiTy::I32,
            Self::I16 | Self::U16 => willow_abi::AbiTy::I16,
            Self::F64 => willow_abi::AbiTy::F64,
            Self::I8 | Self::U8 | Self::Bool => willow_abi::AbiTy::I8,
            Self::Void => return None,
            _ => willow_abi::AbiTy::Ptr,
        })
    }

    fn rust(&self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::Char => "char",
            Self::U8 => "u8",
            Self::I64 => "i64",
            Self::F64 => "f64",
            Self::Bool => "bool",
            Self::Void => "()",
            _ => unreachable!("aggregate Rust type"),
        }
    }

    /// Rust `char` has a validity invariant and is not a C ABI type. Carry its
    /// code point as u32, validating inside the wrapper's unwind boundary.
    fn wire_rust(&self) -> &'static str {
        if *self == Self::Char {
            "u32"
        } else {
            self.rust()
        }
    }

    fn array_storage_flags(&self) -> i64 {
        let width = match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::Char => 4,
            _ => 8,
        };
        willow_abi::array_layout::storage_flags(width, self.reference())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub name: String,
    pub fields: Vec<(String, Scalar)>,
}

/// Copy-on-write declarations: repeated native signatures share their record
/// closure, while existing callers can still mutate a symbol's own records.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecordDefinitions(std::sync::Arc<Vec<Record>>);
impl std::ops::Deref for RecordDefinitions {
    type Target = Vec<Record>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for RecordDefinitions {
    fn deref_mut(&mut self) -> &mut Self::Target {
        std::sync::Arc::make_mut(&mut self.0)
    }
}
impl From<Vec<Record>> for RecordDefinitions {
    fn from(records: Vec<Record>) -> Self {
        Self(std::sync::Arc::new(records))
    }
}
#[cfg(test)]
impl RecordDefinitions {
    pub(crate) fn shares_storage(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustBridgeSymbol {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<crate::rust_module::NativeBridge>,
    #[serde(default)]
    pub is_async: bool,
    pub rust_crate: String,
    pub willow_function: String,
    pub abi_symbol: String,
    pub input_types: Vec<Scalar>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub records: RecordDefinitions,
    pub output_type: Scalar,
    pub effects: u8,
    /// Developer assertion; effective only while the shared review is current.
    #[serde(default)]
    pub no_io: bool,
    #[serde(default)]
    pub review_current: bool,
    #[serde(default)]
    pub assumption_span: Option<crate::diagnostics::Span>,
    #[serde(default)]
    pub close_handle: bool,
    #[serde(default)]
    pub auto_close: bool,
    #[serde(default)]
    pub native_owner: Option<String>,
    #[serde(default)]
    pub native_method: Option<String>,
    #[serde(default)]
    pub native_receiver: bool,
    #[serde(default)]
    pub consume_inputs: Vec<usize>,
    /// Developer assertion; effective only while rust-review.toml is current.
    #[serde(default)]
    pub no_block: bool,
}
impl RustBridgeSymbol {
    /// Native wire type names are unique across Rust modules; Willow names
    /// remain local to the owning source module.
    pub(crate) fn willow_record_name<'a>(&self, wire: &'a str) -> &'a str {
        self.native
            .as_ref()
            .and_then(|native| wire.strip_prefix(&native.module))
            .and_then(|name| name.strip_prefix('_'))
            .unwrap_or(wire)
    }

    pub fn resolve_records(&mut self, definitions: &std::collections::HashMap<String, Record>) {
        let mut seen = std::collections::HashSet::new();
        let mut pending: Vec<_> = self.input_types.iter().chain([&self.output_type]).collect();
        let mut records = Vec::new();
        while let Some(ty) = pending.pop() {
            match ty {
                Scalar::Record(name) if seen.insert(name) => {
                    let record = &definitions[name];
                    records.push(record.clone());
                    pending.extend(record.fields.iter().map(|(_, ty)| ty));
                }
                Scalar::Option(t) | Scalar::Array(t) => pending.push(t),
                Scalar::Result(a, b) => {
                    pending.push(a);
                    pending.push(b);
                }
                _ => {}
            }
        }
        self.records = records.into();
    }
    pub fn aggregate(&self) -> bool {
        self.is_async
            || self
                .input_types
                .iter()
                .chain([&self.output_type])
                .any(Scalar::aggregate)
    }

    pub fn new(name: String, inputs: Vec<Scalar>, output: Scalar) -> Self {
        let effects = willow_abi::RuntimeEffects::MAY_PANIC
            .union(willow_abi::RuntimeEffects::MAY_SUSPEND)
            .union(willow_abi::RuntimeEffects::MAY_ALLOCATE)
            .union(willow_abi::RuntimeEffects::MAY_IO);
        Self {
            native: None,
            is_async: false,
            no_io: false,
            review_current: false,
            assumption_span: None,
            rust_crate: String::new(),
            close_handle: false,
            auto_close: false,
            native_owner: None,
            native_method: None,
            native_receiver: false,
            consume_inputs: vec![],
            no_block: false,
            abi_symbol: name.clone(),
            willow_function: name,
            input_types: inputs,
            records: RecordDefinitions::default(),
            output_type: output,
            effects: effects.bits() | willow_abi::ffi::FOREIGN_CALL,
        }
    }
    pub fn declare_no_block(&mut self) {
        self.no_block = true;
    }

    pub fn assumes_no_block(&self) -> bool {
        self.review_current && self.no_block
    }

    pub fn assumes_no_io(&self) -> bool {
        self.review_current && self.no_io
    }

    pub fn has_assumptions(&self) -> bool {
        self.no_block || self.no_io
    }

    /// Declaration metadata survives failed review; only effective capabilities change.
    pub fn apply_review(&mut self, current: bool) {
        use willow_abi::RuntimeEffects as E;
        self.review_current = current;
        self.effects &= !E::MAY_BLOCK.union(E::MAY_SUSPEND).bits();
        self.effects |= E::MAY_IO.union(E::MAY_SUSPEND).bits();
        if self.assumes_no_io() {
            self.effects &= !E::MAY_IO.bits();
        }
        if self.assumes_no_block() {
            self.effects &= !E::MAY_SUSPEND.bits();
        }
    }

    pub fn uses_blocking_pool(&self) -> bool {
        !self.is_async && !self.assumes_no_block()
    }

    pub fn entry_guard(&self) -> &'static str {
        if self.is_async || self.assumes_no_block() {
            "willow_rust_bridge_enter_nonblocking"
        } else {
            "willow_rust_bridge_enter"
        }
    }

    pub fn bind(&self, identity: &str) -> Self {
        let mut bound = self.clone();
        bound.rust_crate = identity.into();
        let bytes = if self.records.is_empty() {
            serde_json::to_vec(&(&self.willow_function, &self.input_types, &self.output_type))
        } else {
            serde_json::to_vec(&(
                &self.willow_function,
                &self.input_types,
                &self.output_type,
                &self.records,
            ))
        }
        .unwrap();
        // Package identity is a separate component, shared by this bridge's
        // exports. The symbol component keeps its readable leaf name plus a
        // digest of the full Willow name/signature, avoiding namespace and
        // signature collisions without lossy identifier escaping.
        let bytes = if self.is_async {
            serde_json::to_vec(&("async", bytes)).unwrap()
        } else {
            bytes
        };
        let package_hash = Sha256::digest(identity.as_bytes());
        let name = QualifiedName::new(&self.willow_function)
            .last_segment()
            .replace('$', "_");
        bound.abi_symbol = format!(
            "__willow_rust_{:x}_{name}_{:x}",
            crate::hex::Hex(&package_hash),
            crate::hex::Hex(&Sha256::digest(bytes))
        );
        bound
    }
}
/// Exactly one adapter per declaration; O(declarations + parameters + output).
pub fn wrappers(symbols: &[RustBridgeSymbol], abi_path: &std::path::Path, revision: u32) -> String {
    let mut source = format!(
        "#[allow(dead_code, reason = \"Shared bridge support may be unused by a particular adapter\")] #[path = {abi_path:?}] mod willow_bridge_abi;\nconst _: () = assert!(willow_bridge_abi::WILLOW_RUST_BRIDGE_ABI_REVISION == {revision});\nunsafe extern \"C\" {{ fn willow_rust_bridge_enter(revision: u32); fn willow_rust_bridge_enter_nonblocking(revision: u32); fn willow_rust_bridge_panic() -> !; }}\n"
    );
    source.push_str(
        "#[cfg(panic = \"abort\")] compile_error!(\"Rust bridge requires panic=unwind\");\n",
    );
    source.push_str(include_str!("rust_bridge_ir/panic.rs"));
    source.push_str(include_str!("rust_bridge_ir/region.rs"));
    source.push_str("#[allow(dead_code, reason = \"Shared bridge support may be unused by a particular adapter\")] mod handles {\n");
    source.push_str(include_str!(
        "../crates/willow_runtime/src/rust_bridge_abi/handles.rs"
    ));
    source.push_str("\n}\nmod opaque_tags {\n");
    let mut names = std::collections::HashSet::new();
    let mut pending: Vec<_> = symbols
        .iter()
        .flat_map(|symbol| {
            symbol
                .input_types
                .iter()
                .chain([&symbol.output_type])
                .chain(
                    symbol
                        .records
                        .iter()
                        .flat_map(|r| r.fields.iter().map(|(_, ty)| ty)),
                )
        })
        .collect();
    while let Some(ty) = pending.pop() {
        match ty {
            Scalar::Opaque(name) if names.insert(name) => {
                writeln!(source, "pub struct r#{name};").unwrap();
            }
            Scalar::Option(child) | Scalar::Array(child) => pending.push(child),
            Scalar::Result(a, b) => {
                pending.push(a);
                pending.push(b);
            }
            _ => {}
        }
    }
    source.push_str("}\n");
    if symbols
        .iter()
        .any(|s| s.aggregate() || s.native.as_ref().is_some_and(|n| !n.definitions.is_empty()))
    {
        source.push_str(include_str!("rust_bridge_ir/support.rs"));
    }
    if symbols.iter().any(|s| s.is_async || s.uses_blocking_pool()) {
        source.push_str(include_str!("rust_bridge_ir/owned.rs"));
    }
    if symbols.iter().any(RustBridgeSymbol::uses_blocking_pool) {
        source.push_str(include_str!("rust_bridge_ir/blocking.rs"));
    }
    if symbols.iter().any(|s| s.is_async) {
        source.push_str(include_str!("rust_bridge_ir/futures.rs"));
    }
    aggregate::record_structs(&mut source, symbols);
    for symbol in symbols {
        if symbol.aggregate() {
            aggregate_wrapper(&mut source, symbol, revision);
            continue;
        }
        let target = symbol.native.as_ref().map_or_else(
            || QualifiedName::new(&symbol.willow_function).last_segment(),
            |native| native.target.as_str(),
        );
        write!(
            source,
            "#[unsafe(no_mangle)]\npub extern \"C\" fn {}(",
            symbol.abi_symbol
        )
        .unwrap();
        for (i, ty) in symbol.input_types.iter().enumerate() {
            write!(source, "a{i}: {},", ty.wire_rust()).unwrap();
        }
        let guard = symbol.entry_guard();
        write!(source, ") -> {} {{\nunsafe {{ {guard}({revision}); }}\nmatch std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {{\nlet adapter: fn(", symbol.output_type.wire_rust()).unwrap();
        for ty in &symbol.input_types {
            write!(source, "{},", ty.rust()).unwrap();
        }
        let invocation = if symbol.uses_blocking_pool() {
            "blocking_call(move || adapter("
        } else {
            "let _region = BridgeRegion::enter();\nadapter("
        };
        write!(
            source,
            ") -> {} = bridge::r#{target};\n{invocation}",
            symbol.output_type.rust()
        )
        .unwrap();
        for (i, ty) in symbol.input_types.iter().enumerate() {
            if *ty == Scalar::Char {
                write!(
                    source,
                    "char::from_u32(a{i}).expect(\"invalid Unicode scalar at Rust boundary\"),"
                )
                .unwrap();
            } else {
                write!(source, "a{i},").unwrap();
            }
        }
        source.push(')');
        if symbol.uses_blocking_pool() {
            source.push_str(").unwrap_or_default()");
        }
        if symbol.output_type == Scalar::Char {
            source.push_str(" as u32");
        }
        source.push_str("\n})) { Ok(value) => value, Err(payload) => bridge_panic(payload) }\n}\n");
    }
    source
}

mod aggregate;
use aggregate::aggregate_wrapper;

#[cfg(test)]
#[allow(
    dead_code,
    reason = "Shared bridge support may be unused by a particular adapter"
)]
#[path = "rust_bridge_ir/async_executor_tests.rs"]
mod async_executor_tests;

mod handle_debug;
