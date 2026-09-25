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
    pub declarations: Vec<Declaration>,
    pub members: Vec<(Span, Declaration)>,
    pub member_references: Vec<MemberReference>,
    pub references: Vec<Reference>,
}
impl Facts {
    pub fn extend(&mut self, other: Self) {
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
