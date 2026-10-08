//! Opt-in name-resolution evidence emitted by the type checker, not a second resolver.
use crate::{diagnostics::Span, parser::ast::Type};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Declaration {
    pub name: String,
    pub kind: String,
    pub span: Span,
    pub ty: Option<Type>,
}
impl Declaration {
    pub fn new(name: &str, kind: &str, span: Span, ty: Option<Type>) -> Self {
        Self {
            name: name.into(),
            kind: kind.into(),
            span,
            ty,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Reference {
    pub span: Span,
    pub written: String,
    pub target: Declaration,
    pub role: String,
    pub last: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemberReference {
    pub owner: Span,
    pub name: String,
    /// The member token itself, excluding its qualifier and any payload.
    pub span: Span,
    pub kind: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Facts {
    /// Small, body-local import usage set, retained even outside AI capture.
    #[serde(default, serialize_with = "serialize_used_imports")]
    pub used_imports: std::collections::HashSet<String>,
    pub declarations: Vec<Declaration>,
    pub members: Vec<(Span, Declaration)>,
    pub member_references: Vec<MemberReference>,
    pub references: Vec<Reference>,
}

// Facts are serialized into tracked query values. Set iteration order must not
// turn a presentation-only remap into a semantic change.
fn serialize_used_imports<S: serde::Serializer>(
    imports: &std::collections::HashSet<String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut imports: Vec<_> = imports.iter().collect();
    imports.sort_unstable();
    imports.serialize(serializer)
}

impl Facts {
    pub fn extend(&mut self, other: Self) {
        self.used_imports.extend(other.used_imports);
        self.declarations.extend(other.declarations);
        self.members.extend(other.members);
        self.member_references.extend(other.member_references);
        self.references.extend(other.references);
    }
    pub fn reference(
        &mut self,
        span: Span,
        written: &str,
        target: Declaration,
        role: &str,
        last: bool,
    ) {
        self.references.push(Reference {
            span,
            written: written.into(),
            target,
            role: role.into(),
            last,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_facts_have_stable_wire_order_across_insertion_and_roundtrip() {
        for n in [0, 1, 2, 16, 64, 256] {
            let names: Vec<_> = (0..n).map(|i| format!("module_{i:03}")).collect();
            let expected = serde_json::to_value(&names).unwrap();
            for shift in 0..20 {
                let mut facts = Facts::default();
                facts
                    .used_imports
                    .extend((0..n).rev().map(|i| names[(i + shift) % n].clone()));
                let wire = serde_json::to_value(&facts).unwrap();
                assert_eq!(wire["used_imports"], expected);
                let restored: Facts = serde_json::from_value(wire.clone()).unwrap();
                assert_eq!(serde_json::to_value(restored).unwrap(), wire);
            }
        }
    }
}
