//! Compiler evidence remains typed from capture through presentation.
use super::Location;
use crate::{diagnostics::Span, parser::ast::Type, semantic::ids::FunctionId};
use serde::{Deserialize, Serialize};

macro_rules! spellings {
    ($name:ident { $($variant:ident => $text:literal),* $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub enum $name { $($variant,)* Other(String) }
        impl $name {
            pub fn as_str(&self) -> &str {
                match self { $(Self::$variant => $text,)* Self::Other(s) => s }
            }
        }
        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                match s { $($text => Self::$variant,)* _ => Self::Other(s.into()) }
            }
        }
        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                String::deserialize(d).map(|s| Self::from(s.as_str()))
            }
        }
    };
}

spellings!(WitnessKind {
    Wait => "wait", Panic => "panic", Allocation => "allocation",
    ExternalBoundary => "external-boundary", Nonpreemptible => "nonpreemptible",
    Unavailable => "unavailable", RuntimeCapability => "runtime-capability",
    BuiltinEffectPaths => "builtin-effect-paths",
});
spellings!(EvidenceStatus {
    MissingWitness => "missing-witness", MissingSource => "missing-source",
    ConservativeBound => "conservative-bound", ExternalBoundary => "external-boundary",
    CompilerFact => "compiler-fact",
});
spellings!(Certainty {
    ConservativeBound => "conservative-bound", CompilerFact => "compiler-fact",
});
spellings!(CapabilityOperation {
    Await => "await", IndirectCall => "indirect-call", ExternRust => "extern rust",
    ExternalCall => "external-call", ArrayAllocation => "array-allocation",
    EnumAllocation => "enum-allocation", ObjectAllocation => "object-allocation",
    ObjectLiteralAllocation => "object-literal-allocation",
    StringLiteralAllocation => "string-literal-allocation",
    StringConcatenation => "string-concatenation", LambdaEnvironment => "lambda-environment",
    ArrayLengthNullPanic => "array-length-null-panic",
    PanicPayloadAllocation => "panic-payload-allocation", PanicGuard => "panic-guard",
    Allocation => "allocation", SynchronousLoop => "synchronous-loop",
    SynchronousRecursion => "synchronous-recursion", LoopSafepoint => "loop-safepoint",
    ChannelSend => "channel-send", ChannelRecv => "channel-recv", ChannelRecvOpt => "channel-recv-opt",
    BuiltinMethodCall => "builtin-method-call", BuiltinCall => "builtin-call",
});
impl CapabilityOperation {
    // First-use literal initialization stays below per-evaluation allocations
    // when choosing one representative witness. It still has concrete evidence.
    pub fn explicit_allocation(&self) -> bool {
        matches!(
            self,
            Self::ArrayAllocation
                | Self::EnumAllocation
                | Self::ObjectAllocation
                | Self::ObjectLiteralAllocation
                | Self::StringConcatenation
                | Self::LambdaEnvironment
        )
    }
    pub fn channel(&self) -> bool {
        matches!(
            self,
            Self::ChannelSend | Self::ChannelRecv | Self::ChannelRecvOpt
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CauseSource {
    Captured { span: Span },
    Located { location: Option<Location> },
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SourcePoint {
    Captured(Span),
    Located(Location),
}
/// Source lock mode, classified once when compiler or persisted data enters AI.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LockMode {
    Plain(String),
    Qualified(String),
}
impl From<&str> for LockMode {
    fn from(value: &str) -> Self {
        match value.strip_prefix("lock ") {
            Some(mode) => Self::Qualified(mode.into()),
            None => Self::Plain(value.into()),
        }
    }
}
impl LockMode {
    pub fn qualifier(&self) -> Option<&str> {
        match self {
            Self::Qualified(mode) => Some(mode),
            Self::Plain(_) => None,
        }
    }
}
impl Serialize for LockMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Plain(value) => serializer.serialize_str(value),
            Self::Qualified(mode) => serializer.collect_str(&format_args!("lock {mode}")),
        }
    }
}
impl<'de> Deserialize<'de> for LockMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|value| Self::from(value.as_str()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LockRecord {
    pub mode: LockMode,
    pub target: Option<SourcePoint>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub ty: Option<Type>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_display: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cause {
    #[serde(flatten)]
    pub source: CauseSource,
    pub operation: CapabilityOperation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<crate::compiler_db::effects::LockEffectKind>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_conservative: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock: Option<Box<LockRecord>>,
}
impl std::hash::Hash for Cause {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        use crate::compiler_db::effects::LockEffectKind;
        self.source.hash(state);
        self.operation.hash(state);
        self.kind
            .map(|kind| match kind {
                LockEffectKind::Io => 0u8,
                LockEffectKind::Suspend => 1,
                LockEffectKind::Block => 2,
                LockEffectKind::SuspendOrBlock => 3,
            })
            .hash(state);
        self.is_conservative.hash(state);
        self.lock.hash(state);
    }
}
impl Cause {
    pub fn captured(span: Span, operation: CapabilityOperation) -> Self {
        Self {
            source: CauseSource::Captured { span },
            operation,
            kind: None,
            is_conservative: false,
            lock: None,
        }
    }
    pub fn location(&self) -> Option<&Location> {
        match &self.source {
            CauseSource::Located { location } => location.as_ref(),
            _ => None,
        }
    }
    pub fn location_mut(&mut self) -> Option<&mut Location> {
        match &mut self.source {
            CauseSource::Located { location } => location.as_mut(),
            _ => None,
        }
    }
}

/// Live capture data, restored with other source types before presentation.
#[derive(Debug, Clone)]
pub(crate) struct AwaitType {
    pub operand: Option<Type>,
    pub parks: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Witness {
    pub kind: WitnessKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip)]
    pub(crate) owner_key: Option<FunctionId>,
    #[serde(skip)]
    pub(crate) target_key: Option<FunctionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<Cause>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub certainty: Option<Certainty>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_nullable"
    )]
    pub awaited_type: Option<Option<String>>,
    #[serde(skip)]
    pub(crate) awaited: Option<AwaitType>,
}
impl Witness {
    pub fn new(kind: WitnessKind) -> Self {
        Self {
            kind,
            owner: None,
            owner_key: None,
            target_key: None,
            target: None,
            cause: None,
            certainty: None,
            reason: None,
            awaited_type: None,
            awaited: None,
        }
    }
    pub fn operation(&self) -> Option<&CapabilityOperation> {
        self.cause.as_ref().map(|c| &c.operation)
    }
    pub fn location(&self) -> Option<&Location> {
        self.cause.as_ref().and_then(Cause::location)
    }
    pub fn status(&self) -> EvidenceStatus {
        if self.kind == WitnessKind::Unavailable {
            EvidenceStatus::MissingWitness
        } else if self.cause.is_some() && self.location().is_none() {
            EvidenceStatus::MissingSource
        } else if self.kind == WitnessKind::RuntimeCapability {
            EvidenceStatus::ConservativeBound
        } else if self.kind == WitnessKind::ExternalBoundary {
            EvidenceStatus::ExternalBoundary
        } else {
            EvidenceStatus::CompilerFact
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AllocationPath {
    May,
    None,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllocationPaths {
    pub may_allocate: AllocationPath,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuiltinPaths {
    pub normal: AllocationPaths,
    pub panic: AllocationPaths,
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<CapabilityOperation>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_nullable"
    )]
    pub location: Option<Option<Location>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskKind {
    Spawn,
    Await,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRelationship {
    pub kind: TaskKind,
    pub function: String,
    pub location: Option<Location>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectFact {
    pub effect: u8,
    pub witness: Witness,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<BuiltinPaths>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExplainedEffect {
    pub effect: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<EvidenceStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub witness: Option<Witness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_function: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<BuiltinPaths>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_relationship: Option<TaskRelationship>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_chain: Option<Vec<String>>,
}
impl ExplainedEffect {
    pub fn new(effect: u8, witness: Witness, via_function: String) -> Self {
        Self {
            effect,
            status: Some(witness.status()),
            witness: Some(witness),
            via_function: Some(via_function),
            reason: None,
            paths: None,
            task_relationship: None,
            call_chain: None,
        }
    }
}

/// A present null differs from an omitted optional field on the wire.
pub(crate) fn present_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

// Stored evidence used JSON objects historically. Canonicalize one record at
// the serialization boundary to retain its lexical key order in snapshot hashes.
// No JSON objects are retained or consulted by semantic consumers.
pub(super) fn serialize_facts<S: serde::Serializer>(
    facts: &std::collections::BTreeMap<String, Vec<EffectFact>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::{SerializeMap, SerializeSeq};
    struct Records<'a>(&'a [EffectFact]);
    impl Serialize for Records<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
            for fact in self.0 {
                let value = serde_json::to_value(fact).map_err(serde::ser::Error::custom)?;
                seq.serialize_element(&value)?;
            }
            seq.end()
        }
    }
    let mut map = serializer.serialize_map(Some(facts.len()))?;
    for (owner, records) in facts {
        map.serialize_entry(owner, &Records(records))?;
    }
    map.end()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stored_evidence_preserves_canonical_bytes_and_nullable_fields() {
        let input = serde_json::json!({"f":[{"effect":1,"witness":{
            "kind":"wait","awaited_type":null,"cause":{"operation":"await",
            "location":{"path":"main.wi","start":4,"end":9}}}},
            {"effect":2,"witness":{"kind":"builtin-effect-paths"},"paths":{
            "normal":{"may_allocate":"none"},"panic":{"may_allocate":"may"},
            "scope":"operation","location":null}}]});
        let facts: std::collections::BTreeMap<String, Vec<EffectFact>> =
            serde_json::from_value(input.clone()).unwrap();
        #[derive(Serialize)]
        struct Stored<'a>(
            #[serde(serialize_with = "serialize_facts")]
            &'a std::collections::BTreeMap<String, Vec<EffectFact>>,
        );
        assert_eq!(
            serde_json::to_string(&Stored(&facts)).unwrap(),
            serde_json::to_string(&input).unwrap()
        );
    }
}
