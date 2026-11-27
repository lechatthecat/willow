//! Typed semantic operations. Only the snapshot boundary uses legacy spellings.
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Operation {
    Call(String),
    Method(String),
    Static(String),
    IndirectCall,
    Io(String),
    Write(String),
    Wait(String),
    Timeout,
    Channel(String),
    Allocate(String),
    Select(String),
    /// Preserve operations from snapshots produced by a newer compiler.
    Other(String),
}

impl Operation {
    pub(crate) fn is_call(&self) -> bool {
        matches!(self, Self::Call(_) | Self::Method(_) | Self::Static(_))
    }

    pub(crate) fn is_direct_call(&self) -> bool {
        matches!(self, Self::Call(_) | Self::Static(_))
    }

    pub(crate) fn map_call_name(&mut self, map: impl FnOnce(&str) -> String) {
        if let Self::Call(name) | Self::Method(name) | Self::Static(name) = self {
            *name = map(name);
        }
    }

    fn wire_parts(&self) -> (&str, &str) {
        match self {
            Self::Call(s) => ("call:", s),
            Self::Method(s) => ("method:", s),
            Self::Static(s) => ("static:", s),
            Self::IndirectCall => ("indirect-call", ""),
            Self::Io(s) => ("io:", s),
            Self::Write(s) => ("write:", s),
            Self::Wait(s) => ("wait:", s),
            Self::Timeout => ("wait:timeout", ""),
            Self::Channel(s) => ("channel:", s),
            Self::Allocate(s) => ("allocate:", s),
            Self::Select(s) => ("select:", s),
            Self::Other(s) => (s, ""),
        }
    }

    /// Preserve legacy lexical ordering without allocating the wire spelling.
    pub(crate) fn wire_cmp(&self, other: &Self) -> std::cmp::Ordering {
        let (prefix, detail) = self.wire_parts();
        let (other_prefix, other_detail) = other.wire_parts();
        prefix
            .bytes()
            .chain(detail.bytes())
            .cmp(other_prefix.bytes().chain(other_detail.bytes()))
    }
}

impl From<String> for Operation {
    fn from(text: String) -> Self {
        if text == "indirect-call" {
            return Self::IndirectCall;
        }
        let Some((kind, detail)) = text.split_once(':') else {
            return Self::Other(text);
        };
        if text == "wait:timeout" {
            return Self::Timeout;
        }
        match kind {
            "call" => Self::Call(detail.into()),
            "method" => Self::Method(detail.into()),
            "static" => Self::Static(detail.into()),
            "io" => Self::Io(detail.into()),
            "write" => Self::Write(detail.into()),
            "wait" => Self::Wait(detail.into()),
            "channel" => Self::Channel(detail.into()),
            "allocate" => Self::Allocate(detail.into()),
            "select" => Self::Select(detail.into()),
            _ => Self::Other(text),
        }
    }
}

impl From<&str> for Operation {
    fn from(text: &str) -> Self {
        Self::from(text.to_owned())
    }
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (prefix, detail) = self.wire_parts();
        f.write_str(prefix)?;
        f.write_str(detail)
    }
}

impl Serialize for Operation {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Operation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_preserve_wire_spellings_and_lexical_order() {
        let spellings = [
            "call:f",
            "method:m",
            "static:s",
            "indirect-call",
            "io:read",
            "write:x",
            "wait:timeout",
            "wait:other",
            "channel:send",
            "allocate:array",
            "select:ready",
            "bare",
            "",
            "future:猫",
            "call:",
            "call:猫",
            "call",
            "call;",
            "call:f:detail",
        ];
        let mut operations = Vec::new();
        for text in spellings {
            let wire = serde_json::to_string(text).unwrap();
            let operation: Operation = serde_json::from_str(&wire).unwrap();
            assert_eq!(serde_json::to_string(&operation).unwrap(), wire);
            operations.push(operation);
        }
        // Unknown compatibility payloads retain the same ordering and equality
        // as their wire spelling, even if that spelling has a known variant.
        operations.push(Operation::Other("call:f".into()));
        for left in &operations {
            for right in &operations {
                assert_eq!(
                    left.wire_cmp(right),
                    left.to_string().cmp(&right.to_string())
                );
            }
        }
        assert!(matches!(
            Operation::from("indirect-call"),
            Operation::IndirectCall
        ));
        let mut bare = Operation::Other("callee$mono$1".into());
        bare.map_call_name(|_| panic!("bare callee names are not operation prefixes"));
        assert_eq!(bare.to_string(), "callee$mono$1");
    }
}
