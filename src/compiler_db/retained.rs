//! Conservative retained-payload accounting, independent of allocator RSS.
//! Shared payloads are charged per record; collection buckets include slack.
use std::collections::{BTreeSet, HashMap, HashSet};
use std::mem::size_of;
pub(crate) trait Retained {
    fn heap_bytes(&self) -> usize;
    fn retained_bytes(&self) -> usize
    where
        Self: Sized,
    {
        size_of::<Self>() + self.heap_bytes()
    }
}
macro_rules! scalar { ($($ty:ty),*) => { $(impl Retained for $ty { fn heap_bytes(&self) -> usize { 0 } })* }; }
scalar!(
    bool,
    u8,
    u64,
    i64,
    usize,
    crate::module::UnitId,
    crate::diagnostics::Span,
    super::tracked::Durability,
    crate::parser::ast::BodyId,
    crate::semantic::ids::FunctionId,
    crate::semantic::ids::TypeId
);
impl Retained for std::path::PathBuf {
    fn heap_bytes(&self) -> usize {
        self.capacity()
    }
}
impl<T: Retained> Retained for std::rc::Rc<T> {
    fn heap_bytes(&self) -> usize {
        2 * size_of::<usize>() + self.as_ref().retained_bytes()
    }
}
impl<T: Retained> Retained for HashSet<T> {
    fn heap_bytes(&self) -> usize {
        self.capacity() * (size_of::<T>() + 16) * 2
            + self.iter().map(Retained::heap_bytes).sum::<usize>()
    }
}
impl Retained for String {
    fn heap_bytes(&self) -> usize {
        self.capacity()
    }
}
impl<T: Retained> Retained for Vec<T> {
    fn heap_bytes(&self) -> usize {
        self.capacity() * size_of::<T>() + self.iter().map(Retained::heap_bytes).sum::<usize>()
    }
}
impl<T: Retained> Retained for Option<T> {
    fn heap_bytes(&self) -> usize {
        self.as_ref().map_or(0, Retained::heap_bytes)
    }
}
impl<A: Retained, B: Retained> Retained for (A, B) {
    fn heap_bytes(&self) -> usize {
        self.0.heap_bytes() + self.1.heap_bytes()
    }
}
impl<K: Retained, V: Retained> Retained for HashMap<K, V> {
    fn heap_bytes(&self) -> usize {
        self.capacity() * (size_of::<(K, V)>() + 16) * 2
            + self
                .iter()
                .map(|(k, v)| k.heap_bytes() + v.heap_bytes())
                .sum::<usize>()
    }
}
impl<T: Retained> Retained for BTreeSet<T> {
    fn heap_bytes(&self) -> usize {
        self.len() * (size_of::<T>() + 64) * 2
            + self.iter().map(Retained::heap_bytes).sum::<usize>()
    }
}
impl Retained for serde_json::Value {
    fn heap_bytes(&self) -> usize {
        match self {
            Self::String(s) => s.heap_bytes(),
            Self::Array(v) => v.heap_bytes(),
            Self::Object(v) => {
                v.len() * (size_of::<(String, Self)>() + 64) * 2
                    + v.iter()
                        .map(|(k, v)| k.heap_bytes() + v.heap_bytes())
                        .sum::<usize>()
            }
            _ => 0,
        }
    }
}
impl Retained for crate::parser::ast::Type<crate::semantic::ids::TypeId> {
    fn heap_bytes(&self) -> usize {
        use crate::parser::ast::Type;
        match self {
            Type::Array(t) => t.retained_bytes(),
            Type::Generic(_, args) => args.heap_bytes(),
            Type::Fn(args, ret) | Type::Closure(args, ret) => {
                args.heap_bytes() + ret.retained_bytes()
            }
            _ => 0,
        }
    }
}
impl Retained for super::references::SymbolId {
    fn heap_bytes(&self) -> usize {
        self.0.heap_bytes()
    }
}
impl Retained for super::references::SymbolUseId {
    fn heap_bytes(&self) -> usize {
        self.owner.heap_bytes()
    }
}
impl Retained for super::tracked::QueryNode {
    fn heap_bytes(&self) -> usize {
        use super::tracked::QueryNode::*;
        match self {
            Parse(p) | SyntaxDeclarations(p) | SyntaxImports(p) | VisibleScope(_, p) => {
                p.capacity()
            }
            SyntaxSignature(p, s) | BodySyntax(p, s) => p.capacity() + s.capacity(),
            SemanticSignature(_, s) | DispatchTargets(_, _, s) => s.capacity(),
            ResolvedReference(id) => id.heap_bytes(),
            SymbolReferences(id) => id.heap_bytes(),
            _ => 0,
        }
    }
}

impl Retained for super::tracked::InputNode {
    fn heap_bytes(&self) -> usize {
        use super::tracked::InputNode::*;
        match self {
            Source(p) | Manifest(p) | Lock(p) => p.capacity(),
            SemanticSymbol(_, s) => s.capacity(),
            ResolvedReference(id) => id.heap_bytes(),
            ReferenceMembers(id) => id.heap_bytes(),
            DerivedDependencies(node) => node.heap_bytes(),
            _ => 0,
        }
    }
}
