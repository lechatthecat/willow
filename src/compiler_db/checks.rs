//! Persisted semantic checks with explicit environmental reads.
use crate::{
    diagnostics::Diagnostic,
    semantic::{
        concurrency::ConcurrencyReport,
        ids::FunctionId,
        symbols::{FuncInfo, SymbolRead},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum CheckKind {
    Signature,
    Concurrency,
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum CheckInput {
    Helper(FunctionId),
    Class(String),
    Target,
    SignatureType(String, crate::diagnostics::FileId),
    SignatureSymbol(SymbolRead),
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum CheckValue {
    Signature(Box<FuncInfo>, Vec<Diagnostic>, SignatureState),
    Concurrency(ConcurrencyReport, Vec<Diagnostic>),
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct CheckRecord {
    pub syntax: std::rc::Rc<Value>,
    pub inputs: Vec<(CheckInput, Value)>,
    pub value: CheckValue,
}

/// Resolution outputs needed by later annotations and lowering. Concrete user
/// instantiations have already been expanded into the program before this cache.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct SignatureState {
    pub normalized: Vec<(crate::parser::ast::Type, crate::parser::ast::Type)>,
    pub qualified_collections: std::collections::HashSet<String>,
    pub missing_collections: std::collections::HashSet<String>,
}
