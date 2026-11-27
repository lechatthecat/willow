//! Query records shared by semantic analysis and source-facing commands.
use super::operation::Operation;
use super::{Function, Location, SymbolIdentity, symbols};
use crate::ai::SymbolKind;
use crate::parser::ast::Type;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Ok,
    Unknown,
    Unanalyzed,
    Ambiguous,
    Incomplete,
    Stale,
    Error,
    InvalidArguments,
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolRecord {
    pub identity: Option<SymbolIdentity>,
    pub id: String,
    pub name: String,
    pub kind: SymbolKind,
    pub location: Option<Location>,
    pub ty: Option<Type>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_display: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub payload: Vec<Type>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub payload_display: Vec<String>,
}

impl From<&symbols::Symbol> for SymbolRecord {
    fn from(symbol: &symbols::Symbol) -> Self {
        Self {
            identity: symbol.identity.clone(),
            id: symbol.id.clone(),
            name: symbol.name.clone(),
            kind: symbol.kind.clone(),
            location: symbol.location.clone(),
            ty: symbol.ty.clone(),
            type_display: None,
            payload: Vec::new(),
            payload_display: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum SymbolInfo {
    Function(FunctionRecord),
    Declaration(SymbolRecord),
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionRecord {
    pub identity: Option<SymbolIdentity>,
    pub id: String,
    pub module: String,
    pub name: String,
    pub locations: Vec<Location>,
    pub synthetic: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callees: Option<Vec<String>>,
    pub runtime_effects: u8,
    pub unknown: bool,
    pub unresolved: Vec<String>,
}
impl From<&Function> for FunctionRecord {
    fn from(f: &Function) -> Self {
        Self {
            identity: f.identity.clone(),
            id: f.id.clone(),
            module: f.module.clone(),
            name: f.name.clone(),
            locations: f.locations.clone(),
            synthetic: f.synthetic,
            fingerprint: Some(f.fingerprint.clone()),
            body_fingerprint: Some(f.body_fingerprint.clone()),
            callees: Some(f.callees.clone()),
            runtime_effects: f.runtime_effects,
            unknown: f.unknown,
            unresolved: f.unresolved.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TypeRecord {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub ty: Option<Option<Type>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_display: Option<String>,
    pub location: Option<Location>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub payload: Vec<Type>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub payload_display: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    pub status: Status,
    #[serde(flatten)]
    pub data: QueryData,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum QueryData {
    Empty {},
    Symbol {
        symbol: SymbolInfo,
        #[serde(skip_serializing_if = "Option::is_none")]
        resolved_symbols: Option<Vec<SymbolRecord>>,
    },
    Symbols {
        symbols: Vec<SymbolRecord>,
        #[serde(skip_serializing_if = "Option::is_none")]
        total: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        truncated: Option<bool>,
    },
    Type(TypeRecord),
    References {
        references: Vec<ReferenceRecord>,
        #[serde(skip_serializing_if = "Option::is_none")]
        coverage: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        unresolved_candidates: Option<Vec<ReferenceRecord>>,
    },
    Effects(EffectResult),
}

#[derive(Debug, Clone, Serialize)]
pub struct Traversal {
    pub function: String,
    pub via: Option<String>,
    pub reason: Option<String>,
    pub unresolved: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_effects: Option<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnknownCause {
    pub function: String,
    pub name: String,
    pub module: String,
    pub locations: Vec<Location>,
    pub unresolved: Vec<String>,
    pub call_sites: Option<Vec<super::semantic::UnresolvedCall>>,
    pub caller_location: Option<Location>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectBounds {
    pub runtime: u8,
    pub compiler: Option<u8>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EffectResult {
    pub schema: &'static str,
    pub effects: Vec<super::effect_states::EffectState>,
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assumptions: Option<super::assumptions::Assumptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_causes: Option<Vec<UnknownCause>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<EffectBounds>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect_evidence: Option<Vec<super::evidence::ExplainedEffect>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_panic: Option<Option<crate::semantic::type_checker::PanicEvidence>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub witness: Option<Vec<Traversal>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compiler_witnesses: Option<Option<Vec<super::evidence::ExplainedEffect>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meaning: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReferenceCertainty {
    Resolved,
    PossibleDispatch,
    SignatureMatched,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReferenceRecord {
    pub identity: Option<SymbolIdentity>,
    pub target: Option<String>,
    pub location: Location,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ty: Option<Option<Type>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub certainty: Option<ReferenceCertainty>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<Option<Operation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurrences: Option<Vec<usize>>,
}

impl From<&symbols::Reference> for ReferenceRecord {
    fn from(reference: &symbols::Reference) -> Self {
        Self {
            identity: reference.identity.clone(),
            target: Some(reference.target.clone()),
            location: reference.location.clone(),
            role: reference.role.clone(),
            ty: reference.ty.clone().map(Some),
            certainty: None,
            reason: None,
            function: None,
            operation: None,
            fingerprint: None,
            flow: None,
            node: None,
            occurrences: None,
        }
    }
}

impl ReferenceRecord {
    pub(crate) fn expression(
        e: &super::semantic::Expression,
        f: &Function,
        resolved: bool,
    ) -> Self {
        Self {
            identity: f.identity.clone(),
            target: e.target.clone(),
            location: e.location.clone(),
            role: "call".into(),
            ty: Some(e.ty.clone()),
            certainty: Some(if resolved {
                ReferenceCertainty::Resolved
            } else {
                ReferenceCertainty::PossibleDispatch
            }),
            reason: None,
            function: Some(e.function.clone()),
            operation: Some(e.operation.clone()),
            fingerprint: Some(e.fingerprint.clone()),
            flow: Some(e.flow),
            node: Some(e.node),
            occurrences: Some(e.occurrences.clone()),
        }
    }

    /// Merge one occurrence, retaining absent versus explicit-null fields.
    pub(crate) fn merge(&mut self, other: Self) {
        let resolved = other.certainty == Some(ReferenceCertainty::Resolved)
            && self.certainty != Some(ReferenceCertainty::Resolved);
        fn field<T>(old: &mut Option<T>, new: Option<T>, replace: bool) {
            if old.is_none() || replace && new.is_some() {
                *old = new;
            }
        }
        if resolved {
            if other.identity.is_some() {
                self.identity = other.identity;
            }
            if other.target.is_some() {
                self.target = other.target;
            }
            self.role = other.role;
        }
        let typed = other.ty.as_ref().is_some_and(Option::is_some);
        field(&mut self.ty, other.ty, resolved && typed);
        let operation = other.operation.as_ref().is_some_and(Option::is_some);
        field(&mut self.operation, other.operation, resolved && operation);
        field(&mut self.certainty, other.certainty, resolved);
        field(&mut self.reason, other.reason, resolved);
        field(&mut self.function, other.function, resolved);
        field(&mut self.fingerprint, other.fingerprint, resolved);
        field(&mut self.flow, other.flow, resolved);
        field(&mut self.node, other.node, resolved);
        field(&mut self.occurrences, other.occurrences, resolved);
    }
}

impl QueryResult {
    pub fn empty(status: Status) -> Self {
        Self {
            status,
            data: QueryData::Empty {},
        }
    }

    pub fn symbol(status: Status, symbol: SymbolInfo) -> Self {
        Self {
            status,
            data: QueryData::Symbol {
                symbol,
                resolved_symbols: None,
            },
        }
    }
}

impl Status {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unknown => "unknown",
            Self::Unanalyzed => "unanalyzed",
            Self::Ambiguous => "ambiguous",
            Self::Incomplete => "incomplete",
            Self::Stale => "stale",
            Self::Error => "error",
            Self::InvalidArguments => "invalid-arguments",
        }
    }
}
pub use super::assumptions::{Assumption, Assumptions};
pub use super::effect_states::{
    EffectState, EvidenceKind, State, UnknownReason, UnknownReasonCode, WitnessEvidence,
};
