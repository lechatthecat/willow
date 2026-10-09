//! Scalar bridge IR shared by declarations, adapters, and metadata.
use crate::parser::ast::Type;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write;

pub type RustBridgeSymbols = Vec<((u32, String), RustBridgeSymbol)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scalar {
    I64,
    F64,
    Bool,
    Void,
}
impl Scalar {
    pub fn abi_type(self) -> Option<willow_abi::AbiTy> {
        Some(match self {
            Self::I64 => willow_abi::AbiTy::I64,
            Self::F64 => willow_abi::AbiTy::F64,
            Self::Bool => willow_abi::AbiTy::I8,
            Self::Void => return None,
        })
    }

    pub fn from_type(ty: &Type) -> Option<Self> {
        Some(match ty {
            Type::I64 => Self::I64,
            Type::F64 => Self::F64,
            Type::Bool => Self::Bool,
            Type::Void => Self::Void,
            _ => return None,
        })
    }
    fn rust(self) -> &'static str {
        match self {
            Self::I64 => "i64",
            Self::F64 => "f64",
            Self::Bool => "bool",
            Self::Void => "()",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RustBridgeSymbol {
    pub rust_crate: String,
    pub willow_function: String,
    pub abi_symbol: String,
    pub input_types: Vec<Scalar>,
    pub output_type: Scalar,
    pub effects: u8,
}
impl RustBridgeSymbol {
    pub fn new(name: String, inputs: Vec<Scalar>, output: Scalar) -> Self {
        let effects = willow_abi::RuntimeEffects::MAY_PANIC
            .union(willow_abi::RuntimeEffects::MAY_BLOCK)
            .union(willow_abi::RuntimeEffects::MAY_ALLOCATE);
        Self {
            rust_crate: String::new(),
            abi_symbol: name.clone(),
            willow_function: name,
            input_types: inputs,
            output_type: output,
            effects: effects.bits() | willow_abi::ffi::FOREIGN_CALL,
        }
    }
    pub fn bind(&self, identity: &str) -> Self {
        let mut bound = self.clone();
        bound.rust_crate = identity.into();
        let bytes =
            serde_json::to_vec(&(&self.willow_function, &self.input_types, self.output_type))
                .unwrap();
        // Package identity is a separate component, shared by this bridge's
        // exports. The symbol component keeps its readable leaf name plus a
        // digest of the full Willow name/signature, avoiding namespace and
        // signature collisions without lossy identifier escaping.
        let package_hash = Sha256::digest(identity.as_bytes());
        let name = self.willow_function.rsplit("::").next().unwrap();
        bound.abi_symbol = format!(
            "__willow_rust_{package_hash:x}_{name}_{:x}",
            Sha256::digest(bytes)
        );
        bound
    }
}
/// Exactly one adapter per declaration; O(declarations + parameters + output).
pub fn wrappers(symbols: &[RustBridgeSymbol], abi_path: &std::path::Path, revision: u32) -> String {
    let mut source = format!(
        "#[allow(dead_code)] #[path = {abi_path:?}] mod willow_bridge_abi;\nconst _: () = assert!(willow_bridge_abi::WILLOW_RUST_BRIDGE_ABI_REVISION == {revision});\nunsafe extern \"C\" {{ fn willow_rust_bridge_enter(revision: u32); fn willow_rust_bridge_panic() -> !; }}\n"
    );
    source.push_str(
        "#[cfg(panic = \"abort\")] compile_error!(\"Rust bridge requires panic=unwind\");\n",
    );
    for symbol in symbols {
        let target = symbol.willow_function.rsplit("::").next().unwrap();
        write!(
            source,
            "#[unsafe(no_mangle)]\npub extern \"C\" fn {}(",
            symbol.abi_symbol
        )
        .unwrap();
        for (i, ty) in symbol.input_types.iter().enumerate() {
            write!(source, "a{i}: {},", ty.rust()).unwrap();
        }
        write!(source, ") -> {} {{\nunsafe {{ willow_rust_bridge_enter({revision}); }}\nmatch std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {{\nlet adapter: fn(", symbol.output_type.rust()).unwrap();
        for ty in &symbol.input_types {
            write!(source, "{},", ty.rust()).unwrap();
        }
        write!(
            source,
            ") -> {} = bridge::r#{target};\nadapter(",
            symbol.output_type.rust()
        )
        .unwrap();
        for i in 0..symbol.input_types.len() {
            write!(source, "a{i},").unwrap();
        }
        source.push_str(")\n})) { Ok(value) => value, Err(payload) => { std::mem::forget(payload); unsafe { willow_rust_bridge_panic() } } }\n}\n");
    }
    source
}
