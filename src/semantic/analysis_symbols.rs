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
    /// A member call's callee type with the receiver's type arguments
    /// substituted; the declaration keeps its generic signature.
    #[serde(default)]
    pub ty: Option<Type>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemberReference {
    pub owner: Span,
    pub name: String,
    /// The member token itself, excluding its qualifier and any payload.
    pub span: Span,
    pub kind: String,
    /// Matched by a pattern rather than constructed or read.
    #[serde(default)]
    pub pattern: bool,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Facts {
    /// Method calls whose receiver resolved to the lexical base class.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub super_calls: Vec<Span>,
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
        self.super_calls.extend(other.super_calls);
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
            ty: None,
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

/// Top-level generic binders have disjoint lexical extents. Index owners once,
/// then resolve each explicit type head without rescanning declarations/bodies.
pub(crate) struct TypeParameterScopes<'a> {
    owners: Vec<(Span, std::collections::HashSet<&'a str>)>,
    #[cfg(test)]
    probes: std::cell::Cell<usize>,
}
impl<'a> TypeParameterScopes<'a> {
    pub fn new(program: &'a crate::parser::ast::Program) -> Self {
        use crate::parser::ast::Item;
        let mut owners: Vec<_> = program
            .items
            .iter()
            .filter_map(|item| {
                let (span, params) = match item {
                    Item::Function(f) => (f.span, &f.type_params),
                    Item::Class(c) => (c.span, &c.type_params),
                    Item::Enum(e) => (e.span, &e.type_params),
                    Item::Interface(i) => (i.span, &i.type_params),
                };
                (!params.is_empty()).then(|| (span, params.iter().map(String::as_str).collect()))
            })
            .collect();
        owners.sort_by_key(|(span, _)| (span.file_id.0, span.start));
        Self {
            owners,
            #[cfg(test)]
            probes: std::cell::Cell::new(0),
        }
    }
    pub fn owner(&self, usage: &crate::parser::ast::TypeUse) -> Option<Span> {
        let p = self.owners.partition_point(|(span, _)| {
            #[cfg(test)]
            self.probes.set(self.probes.get() + 1);
            (span.file_id.0, span.start) <= (usage.span.file_id.0, usage.span.start)
        });
        let (owner, params) = self.owners.get(p.checked_sub(1)?)?;
        (owner.contains(usage.span) && params.contains(usage.name.as_str())).then_some(*owner)
    }
}

#[cfg(test)]
mod type_parameter_tests {
    use super::*;

    #[test]
    fn type_parameter_owner_index_scales_with_owners_parameters_and_uses() {
        for owners in [16usize, 64, 256] {
            for parameters in [1, 8] {
                for uses in [4, 32] {
                    let params = (0..parameters)
                        .map(|p| format!("T{p}"))
                        .collect::<Vec<_>>()
                        .join(",");
                    let args = (0..uses)
                        .map(|u| format!("x{u}: T{}", u % parameters))
                        .collect::<Vec<_>>()
                        .join(",");
                    let source = (0..owners)
                        .map(|i| format!("fn generic{i}<{params}>({args}) {{}}\n"))
                        .collect::<String>();
                    let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
                    let (program, errors) = crate::parser::Parser::new(tokens).parse();
                    assert!(errors.is_empty(), "{errors:?}");
                    let index = TypeParameterScopes::new(&program);
                    assert_eq!(index.owners.len(), owners);
                    assert_eq!(
                        index.owners.iter().map(|(_, p)| p.len()).sum::<usize>(),
                        owners * parameters
                    );
                    assert_eq!(program.type_uses.len(), owners * uses);
                    for usage in &program.type_uses {
                        assert!(index.owner(usage).unwrap().contains(usage.span));
                    }
                    let bound = owners * uses * (owners.ilog2() as usize + 2);
                    assert!(index.probes.get() <= bound);
                    println!(
                        "owners={owners} parameters={parameters} uses_per_owner={uses} references={} probes={} bound={bound}",
                        owners * uses,
                        index.probes.get()
                    );
                }
            }
        }
    }
}
