//! Stack-independent ownership and wire representation for source and resolved types.
use super::Type;
use std::hash::{Hash, Hasher};

impl<N> Type<N> {
    pub fn map_names<M>(&self, mut map: impl FnMut(&N) -> M) -> Type<M> {
        enum Work<'a, N> {
            Enter(&'a Type<N>),
            Build(&'a Type<N>, usize),
        }
        let mut work = vec![Work::Enter(self)];
        let mut values = Vec::new();
        while let Some(task) = work.pop() {
            match task {
                Work::Enter(ty) => {
                    let children = ty.children();
                    work.push(Work::Build(ty, children.len()));
                    work.extend(children.into_iter().rev().map(Work::Enter));
                }
                Work::Build(ty, count) => {
                    let mut children = values.split_off(values.len() - count);
                    let value = match ty {
                        Type::I64 => Type::I64,
                        Type::F64 => Type::F64,
                        Type::Bool => Type::Bool,
                        Type::String => Type::String,
                        Type::Void => Type::Void,
                        Type::Never => Type::Never,
                        Type::Named(name) => Type::Named(map(name)),
                        Type::Array(_) => Type::Array(Box::new(children.pop().unwrap())),
                        Type::Generic(name, _) => Type::Generic(map(name), children),
                        Type::Fn(..) => {
                            let result = children.pop().unwrap();
                            Type::Fn(children, Box::new(result))
                        }
                        Type::Closure(..) => {
                            let result = children.pop().unwrap();
                            Type::Closure(children, Box::new(result))
                        }
                    };
                    values.push(value);
                }
            }
        }
        values.pop().unwrap()
    }
    pub fn substitute_names(&self, mut replacement: impl FnMut(&N) -> Option<Self>) -> Self
    where
        N: Clone,
    {
        enum Work<'a, N> {
            Enter(&'a Type<N>),
            Build(&'a Type<N>, usize),
        }
        let mut work = vec![Work::Enter(self)];
        let mut values = Vec::new();
        while let Some(task) = work.pop() {
            match task {
                Work::Enter(ty) => {
                    if let Type::Named(name) = ty
                        && let Some(value) = replacement(name)
                    {
                        values.push(value);
                        continue;
                    }
                    let children = ty.children();
                    work.push(Work::Build(ty, children.len()));
                    work.extend(children.into_iter().rev().map(Work::Enter));
                }
                Work::Build(ty, count) => {
                    let mut children = values.split_off(values.len() - count);
                    let value = match ty {
                        Type::I64 => Type::I64,
                        Type::F64 => Type::F64,
                        Type::Bool => Type::Bool,
                        Type::String => Type::String,
                        Type::Void => Type::Void,
                        Type::Never => Type::Never,
                        Type::Named(name) => Type::Named(name.clone()),
                        Type::Array(_) => Type::Array(Box::new(children.pop().unwrap())),
                        Type::Generic(name, _) => Type::Generic(name.clone(), children),
                        Type::Fn(..) => {
                            let result = children.pop().unwrap();
                            Type::Fn(children, Box::new(result))
                        }
                        Type::Closure(..) => {
                            let result = children.pop().unwrap();
                            Type::Closure(children, Box::new(result))
                        }
                    };
                    values.push(value);
                }
            }
        }
        values.pop().unwrap()
    }
    fn children(&self) -> Vec<&Self> {
        match self {
            Self::Array(element) => vec![element],
            Self::Generic(_, args) => args.iter().collect(),
            Self::Fn(args, result) | Self::Closure(args, result) => args
                .iter()
                .chain(std::iter::once(result.as_ref()))
                .collect(),
            _ => Vec::new(),
        }
    }
    fn take_children(&mut self, pending: &mut Vec<Self>) {
        match self {
            Self::Array(element) => pending.push(std::mem::replace(element.as_mut(), Self::Void)),
            Self::Generic(_, args) => pending.append(args),
            Self::Fn(args, result) | Self::Closure(args, result) => {
                pending.append(args);
                pending.push(std::mem::replace(result.as_mut(), Self::Void));
            }
            _ => {}
        }
    }
}
impl<N: Clone> Clone for Type<N> {
    fn clone(&self) -> Self {
        self.map_names(Clone::clone)
    }
}
impl<N> Drop for Type<N> {
    fn drop(&mut self) {
        let mut pending = Vec::new();
        self.take_children(&mut pending);
        while let Some(mut child) = pending.pop() {
            child.take_children(&mut pending);
        }
    }
}
impl<N: PartialEq> PartialEq for Type<N> {
    fn eq(&self, other: &Self) -> bool {
        let mut pending = vec![(self, other)];
        while let Some((left, right)) = pending.pop() {
            if std::mem::discriminant(left) != std::mem::discriminant(right) {
                return false;
            }
            match (left, right) {
                (Self::Named(a), Self::Named(b)) | (Self::Generic(a, _), Self::Generic(b, _))
                    if a != b =>
                {
                    return false;
                }
                _ => {}
            }
            let a = left.children();
            let b = right.children();
            if a.len() != b.len() {
                return false;
            }
            pending.extend(a.into_iter().zip(b));
        }
        true
    }
}
impl<N: Eq> Eq for Type<N> {}
impl<N: Hash> Hash for Type<N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let mut pending = vec![self];
        while let Some(ty) = pending.pop() {
            std::mem::discriminant(ty).hash(state);
            match ty {
                Self::Named(name) | Self::Generic(name, _) => name.hash(state),
                _ => {}
            }
            let children = ty.children();
            children.len().hash(state);
            pending.extend(children.into_iter().rev());
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
enum FlatType<N> {
    I64,
    F64,
    Bool,
    String,
    Void,
    Never,
    Named(N),
    Array(usize),
    Generic(N, Vec<usize>),
    Fn(Vec<usize>, usize),
    Closure(Vec<usize>, usize),
}
impl<N: serde::Serialize> serde::Serialize for Type<N> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut nodes = Vec::new();
        let mut pending = vec![self];
        while let Some(ty) = pending.pop() {
            nodes.push(ty);
            pending.extend(ty.children());
        }
        // Pointer IDs refer only to this serialization traversal, never persisted addresses.
        let indexes: std::collections::HashMap<*const Type<N>, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, ty)| (*ty as *const _, i))
            .collect();
        let index = |ty: &Type<N>| indexes[&(ty as *const _)];
        let flat: Vec<_> = nodes
            .into_iter()
            .map(|ty| match ty {
                Type::I64 => FlatType::I64,
                Type::F64 => FlatType::F64,
                Type::Bool => FlatType::Bool,
                Type::String => FlatType::String,
                Type::Void => FlatType::Void,
                Type::Never => FlatType::Never,
                Type::Named(name) => FlatType::Named(name),
                Type::Array(element) => FlatType::Array(index(element)),
                Type::Generic(name, args) => {
                    FlatType::Generic(name, args.iter().map(index).collect())
                }
                Type::Fn(args, result) => {
                    FlatType::Fn(args.iter().map(index).collect(), index(result))
                }
                Type::Closure(args, result) => {
                    FlatType::Closure(args.iter().map(index).collect(), index(result))
                }
            })
            .collect();
        flat.serialize(serializer)
    }
}
impl<'de, N: serde::Deserialize<'de>> serde::Deserialize<'de> for Type<N> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let nodes = Vec::<FlatType<N>>::deserialize(deserializer)?;
        if nodes.is_empty() {
            return Err(D::Error::custom("empty type tree"));
        }
        let mut built: Vec<Option<Self>> = (0..nodes.len()).map(|_| None).collect();
        for (index, node) in nodes.into_iter().enumerate().rev() {
            let mut take = |child: usize| -> Result<Self, D::Error> {
                if child <= index {
                    return Err(D::Error::custom("non-forward type edge"));
                }
                built
                    .get_mut(child)
                    .and_then(Option::take)
                    .ok_or_else(|| D::Error::custom("invalid or repeated type edge"))
            };
            let value = match node {
                FlatType::I64 => Self::I64,
                FlatType::F64 => Self::F64,
                FlatType::Bool => Self::Bool,
                FlatType::String => Self::String,
                FlatType::Void => Self::Void,
                FlatType::Never => Self::Never,
                FlatType::Named(name) => Self::Named(name),
                FlatType::Array(child) => Self::Array(Box::new(take(child)?)),
                FlatType::Generic(name, args) => Self::Generic(
                    name,
                    args.into_iter().map(&mut take).collect::<Result<_, _>>()?,
                ),
                FlatType::Fn(args, result) => Self::Fn(
                    args.into_iter().map(&mut take).collect::<Result<_, _>>()?,
                    Box::new(take(result)?),
                ),
                FlatType::Closure(args, result) => Self::Closure(
                    args.into_iter().map(&mut take).collect::<Result<_, _>>()?,
                    Box::new(take(result)?),
                ),
            };
            built[index] = Some(value);
        }
        let root = built[0].take().unwrap();
        if built.iter().any(Option::is_some) {
            return Err(D::Error::custom("unreachable type node"));
        }
        Ok(root)
    }
}

impl<N: std::fmt::Debug> std::fmt::Debug for Type<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        enum Work<'a, N> {
            Type(&'a Type<N>),
            Text(&'static str),
            Name(&'a N),
        }
        let mut work = vec![Work::Type(self)];
        while let Some(task) = work.pop() {
            match task {
                Work::Text(text) => f.write_str(text)?,
                Work::Name(name) => std::fmt::Debug::fmt(name, f)?,
                Work::Type(ty) => match ty {
                    Type::I64 => f.write_str("I64")?,
                    Type::F64 => f.write_str("F64")?,
                    Type::Bool => f.write_str("Bool")?,
                    Type::String => f.write_str("String")?,
                    Type::Void => f.write_str("Void")?,
                    Type::Never => f.write_str("Never")?,
                    Type::Named(name) => {
                        f.write_str("Named(")?;
                        work.extend([Work::Text(")"), Work::Name(name)]);
                    }
                    Type::Array(element) => {
                        f.write_str("Array(")?;
                        work.extend([Work::Text(")"), Work::Type(element)]);
                    }
                    Type::Generic(name, args) => {
                        f.write_str("Generic(")?;
                        std::fmt::Debug::fmt(name, f)?;
                        f.write_str(", [")?;
                        work.push(Work::Text("])"));
                        for (i, arg) in args.iter().enumerate().rev() {
                            work.push(Work::Type(arg));
                            if i != 0 {
                                work.push(Work::Text(", "));
                            }
                        }
                    }
                    Type::Fn(args, result) | Type::Closure(args, result) => {
                        f.write_str(if matches!(ty, Type::Fn(..)) {
                            "Fn(["
                        } else {
                            "Closure(["
                        })?;
                        work.extend([Work::Text(")"), Work::Type(result), Work::Text("], ")]);
                        for (i, arg) in args.iter().enumerate().rev() {
                            work.push(Work::Type(arg));
                            if i != 0 {
                                work.push(Work::Text(", "));
                            }
                        }
                    }
                },
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::ids::{SemanticType, TypeId};
    #[test]
    fn source_and_resolved_type_ownership_twenty_perspectives() {
        // Five recursive shapes x four semantic identities: namespace aliases,
        // same short name in different modules, and Unicode nominal names.
        for name in ["Local", "left::Item", "right::Item", "型::値"] {
            for shape in 0..5 {
                std::thread::Builder::new()
                    .stack_size(128 * 1024)
                    .spawn(move || {
                        let mut ty: Type = Type::Named(name.to_string());
                        for _ in 0..4000 {
                            ty = match shape {
                                0 => Type::Array(Box::new(ty)),
                                1 => Type::Generic("Box".to_string(), vec![ty]),
                                2 => Type::Fn(vec![ty], Box::new(Type::Void)),
                                3 => Type::Closure(Vec::new(), Box::new(ty)),
                                _ => Type::Closure(
                                    vec![Type::I64],
                                    Box::new(Type::Array(Box::new(ty))),
                                ),
                            };
                        }
                        let copy = ty.clone();
                        assert_eq!(copy, ty);
                        let resolved: SemanticType = (&ty).into();
                        assert_eq!(resolved.to_source(), ty);
                        let wire = serde_json::to_vec(&resolved).unwrap();
                        let decoded: SemanticType = serde_json::from_slice(&wire).unwrap();
                        assert_eq!(decoded, resolved);
                        use std::collections::hash_map::DefaultHasher;
                        let mut a = DefaultHasher::new();
                        let mut b = DefaultHasher::new();
                        decoded.hash(&mut a);
                        resolved.hash(&mut b);
                        assert_eq!(a.finish(), b.finish());
                        let substituted = resolved.substitute_names(|id| {
                            (id == &TypeId::from_source_name(name)).then_some(Type::Bool)
                        });
                        assert_ne!(substituted, resolved);
                        assert!(!format!("{resolved:?}").is_empty());
                    })
                    .unwrap()
                    .join()
                    .unwrap();
            }
        }
    }
    #[test]
    fn flat_type_wire_rejects_cycles_duplicates_missing_and_unreachable_nodes() {
        for wire in [
            "[]",
            "[{\"Array\":0}]",
            "[{\"Array\":2},\"I64\"]",
            "[{\"Fn\":[[1],1]},\"I64\"]",
            "[\"Void\",\"I64\"]",
        ] {
            assert!(serde_json::from_str::<Type>(wire).is_err(), "{wire}");
        }
    }
}
