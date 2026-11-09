//! Bridge type IR shared by declarations, adapters, and metadata.
//! `Scalar` retains its R2 public name for API compatibility.
use crate::parser::ast::Type;
use crate::qualified_name::QualifiedName;
use crate::semantic::builtin_types::BuiltinTypeId as Builtin;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write;

pub(crate) const WRAPPER_SCHEMA: &str = "6";

pub type RustBridgeSymbols = Vec<((u32, String), RustBridgeSymbol)>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scalar {
    I64,
    F64,
    Bool,
    Void,
    String,
    Bytes,
    Opaque(String),
    Option(Box<Scalar>),
    Result(Box<Scalar>, Box<Scalar>),
}
impl Scalar {
    pub fn abi_type(&self) -> Option<willow_abi::AbiTy> {
        Some(match self {
            Self::I64 => willow_abi::AbiTy::I64,
            Self::F64 => willow_abi::AbiTy::F64,
            Self::Bool => willow_abi::AbiTy::I8,
            Self::Void => return None,
            _ => willow_abi::AbiTy::Ptr,
        })
    }

    pub fn from_type(ty: &Type) -> Option<Self> {
        Self::from_type_with_opaque(ty, &Default::default())
    }
    pub fn from_type_with_opaque(
        ty: &Type,
        names: &std::collections::HashSet<String>,
    ) -> Option<Self> {
        Some(match ty {
            Type::Named(name) if names.contains(name) => Self::Opaque(name.clone()),
            Type::I64 => Self::I64,
            Type::F64 => Self::F64,
            Type::Bool => Self::Bool,
            Type::Void => Self::Void,
            Type::String => Self::String,
            Type::Array(inner) if **inner == Type::I64 => Self::Bytes,
            // `Array` is a source-defined class, so the parser keeps
            // `Array<i64>` as a plain generic application.
            Type::Generic(name, args)
                if Builtin::from_name(name) == Some(Builtin::Array)
                    && args.as_slice() == [Type::I64] =>
            {
                Self::Bytes
            }
            Type::Generic(name, args)
                if Builtin::from_name(name) == Some(Builtin::Option) && args.len() == 1 =>
            {
                Self::Option(Box::new(Self::from_type_with_opaque(&args[0], names)?))
            }
            Type::Generic(name, args)
                if Builtin::from_name(name) == Some(Builtin::Result) && args.len() == 2 =>
            {
                Self::Result(
                    Box::new(Self::from_type_with_opaque(&args[0], names)?),
                    Box::new(Self::from_type_with_opaque(&args[1], names)?),
                )
            }
            _ => return None,
        })
    }
    fn rust(&self) -> &'static str {
        match self {
            Self::I64 => "i64",
            Self::F64 => "f64",
            Self::Bool => "bool",
            Self::Void => "()",
            _ => unreachable!("aggregate Rust type"),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NonblockingReview {
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustBridgeSymbol {
    pub rust_crate: String,
    pub willow_function: String,
    pub abi_symbol: String,
    pub input_types: Vec<Scalar>,
    pub output_type: Scalar,
    pub effects: u8,
    #[serde(default)]
    pub close_handle: bool,
    /// Developer assertion, bound to the resolved lock and bridge source.
    #[serde(default)]
    pub nonblocking: Option<Box<NonblockingReview>>,
}
impl RustBridgeSymbol {
    pub fn aggregate(&self) -> bool {
        self.input_types
            .iter()
            .chain([&self.output_type])
            .any(Scalar::aggregate)
    }

    pub fn new(name: String, inputs: Vec<Scalar>, output: Scalar) -> Self {
        let effects = willow_abi::RuntimeEffects::MAY_PANIC
            .union(willow_abi::RuntimeEffects::MAY_BLOCK)
            .union(willow_abi::RuntimeEffects::MAY_ALLOCATE);
        Self {
            rust_crate: String::new(),
            close_handle: false,
            nonblocking: None,
            abi_symbol: name.clone(),
            willow_function: name,
            input_types: inputs,
            output_type: output,
            effects: effects.bits() | willow_abi::ffi::FOREIGN_CALL,
        }
    }
    pub fn declare_nonblocking(&mut self, fingerprint: String) {
        self.nonblocking = Some(Box::new(NonblockingReview { fingerprint }));
        self.effects &= !willow_abi::RuntimeEffects::MAY_BLOCK.bits();
    }

    pub fn entry_guard(&self) -> &'static str {
        if self.nonblocking.is_some() {
            "willow_rust_bridge_enter_nonblocking"
        } else {
            "willow_rust_bridge_enter"
        }
    }

    pub fn bind(&self, identity: &str) -> Self {
        let mut bound = self.clone();
        bound.rust_crate = identity.into();
        let bytes =
            serde_json::to_vec(&(&self.willow_function, &self.input_types, &self.output_type))
                .unwrap();
        // Package identity is a separate component, shared by this bridge's
        // exports. The symbol component keeps its readable leaf name plus a
        // digest of the full Willow name/signature, avoiding namespace and
        // signature collisions without lossy identifier escaping.
        let package_hash = Sha256::digest(identity.as_bytes());
        let name = QualifiedName::new(&self.willow_function).last_segment();
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
        "#[allow(dead_code)] #[path = {abi_path:?}] mod willow_bridge_abi;\nconst _: () = assert!(willow_bridge_abi::WILLOW_RUST_BRIDGE_ABI_REVISION == {revision});\nunsafe extern \"C\" {{ fn willow_rust_bridge_enter(revision: u32); fn willow_rust_bridge_enter_nonblocking(revision: u32); fn willow_rust_bridge_panic() -> !; }}\n"
    );
    source.push_str(
        "#[cfg(panic = \"abort\")] compile_error!(\"Rust bridge requires panic=unwind\");\n",
    );
    source.push_str(include_str!("rust_bridge_ir/panic.rs"));
    source.push_str(include_str!("rust_bridge_ir/region.rs"));
    source.push_str("#[allow(dead_code)] mod handles {\n");
    source.push_str(include_str!(
        "../crates/willow_runtime/src/rust_bridge_abi/handles.rs"
    ));
    source.push_str("\n}\nmod opaque_tags {\n");
    let mut names = std::collections::HashSet::new();
    let mut pending: Vec<_> = symbols
        .iter()
        .flat_map(|symbol| symbol.input_types.iter().chain([&symbol.output_type]))
        .collect();
    while let Some(ty) = pending.pop() {
        match ty {
            Scalar::Opaque(name) if names.insert(name) => {
                writeln!(source, "pub struct r#{name};").unwrap();
            }
            Scalar::Option(child) => pending.push(child),
            Scalar::Result(a, b) => {
                pending.push(a);
                pending.push(b);
            }
            _ => {}
        }
    }
    source.push_str("}\n");
    if symbols.iter().any(RustBridgeSymbol::aggregate) {
        source.push_str(include_str!("rust_bridge_ir/support.rs"));
    }
    for symbol in symbols {
        if symbol.aggregate() {
            aggregate_wrapper(&mut source, symbol, revision);
            continue;
        }
        let target = QualifiedName::new(&symbol.willow_function).last_segment();
        write!(
            source,
            "#[unsafe(no_mangle)]\npub extern \"C\" fn {}(",
            symbol.abi_symbol
        )
        .unwrap();
        for (i, ty) in symbol.input_types.iter().enumerate() {
            write!(source, "a{i}: {},", ty.rust()).unwrap();
        }
        let guard = symbol.entry_guard();
        write!(source, ") -> {} {{\nunsafe {{ {guard}({revision}); }}\nmatch std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {{\nlet adapter: fn(", symbol.output_type.rust()).unwrap();
        for ty in &symbol.input_types {
            write!(source, "{},", ty.rust()).unwrap();
        }
        write!(
            source,
            ") -> {} = bridge::r#{target};\nlet _region = BridgeRegion::enter();\nadapter(",
            symbol.output_type.rust()
        )
        .unwrap();
        for i in 0..symbol.input_types.len() {
            write!(source, "a{i},").unwrap();
        }
        source
            .push_str(")\n})) { Ok(value) => value, Err(payload) => bridge_panic(payload) }\n}\n");
    }
    source
}

mod aggregate;
use aggregate::aggregate_wrapper;
