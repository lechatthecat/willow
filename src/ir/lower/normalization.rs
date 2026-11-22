//! Exact subtree keys for the checker's whole-type annotation rewrites.
use super::{HashMap, PartialCheckerTables, Type};
use std::borrow::Cow;
use std::mem::{Discriminant, discriminant};
use std::sync::{Arc, OnceLock};

#[cfg(test)]
pub(super) use crate::counted_hashes::{HASHES as HASHED_KEY_STEPS, KeyHasher};
#[cfg(not(test))]
pub(super) type KeyHasher = std::collections::hash_map::RandomState;

pub(super) type BoundNames<'a> = std::collections::HashSet<&'a str, KeyHasher>;

pub(super) fn bound_names(parameters: &[String]) -> BoundNames<'_> {
    parameters.iter().map(String::as_str).collect()
}

/// One immutable annotation table per unit. Its lowering views share the
/// index, which is built only when there are recorded rewrites to consult.
#[derive(Clone, Default)]
pub struct NormalizedTypes<'a>(Option<Arc<LazyRules<'a>>>);

struct LazyRules<'a> {
    table: &'a HashMap<Type, Type>,
    index: OnceLock<RuleIndex<'a>>,
}

impl<'a> From<&'a HashMap<Type, Type>> for NormalizedTypes<'a> {
    fn from(table: &'a HashMap<Type, Type>) -> Self {
        Self((!table.is_empty()).then(|| {
            Arc::new(LazyRules {
                table,
                index: OnceLock::new(),
            })
        }))
    }
}

impl NormalizedTypes<'_> {
    pub(super) fn index(&self) -> Option<&RuleIndex<'_>> {
        let rules = self.0.as_ref()?;
        let index = rules.index.get_or_init(|| RuleIndex::new(rules.table));
        (!index.rules.is_empty()).then_some(index)
    }
}

/// A head followed by child IDs uniquely identifies a type, including callable
/// kind, child order, arity and fixed-array length. Intermediate prefixes share
/// the same index. Hashing any one transition takes no descendant walk.
#[derive(Hash, Eq, PartialEq)]
enum Step<'a> {
    Atom(Discriminant<Type>),
    Named(&'a str),
    Generic(&'a str),
    Array,
    FixedArray(u64),
    Callable(bool),
    Child(usize, usize),
}

impl<'a> Step<'a> {
    fn head(ty: &'a Type) -> Self {
        match ty {
            Type::Named(name) => Self::Named(name),
            Type::Generic(name, _) => Self::Generic(name),
            Type::Array(_) => Self::Array,
            Type::FixedArray(_, n) => Self::FixedArray(*n),
            Type::Fn(..) => Self::Callable(true),
            Type::Closure(..) => Self::Callable(false),
            _ => Self::Atom(discriminant(ty)),
        }
    }
}

// Table-owned types remain immutably borrowed for the lifetime of the index.
// Addresses identify nodes only within that borrow and are never dereferenced.
type Matches<'a> = HashMap<usize, &'a Type>;

pub(crate) struct RuleIndex<'a> {
    steps: HashMap<Step<'a>, usize, KeyHasher>,
    // Maps each exact source key to its replacement and the replacement's key.
    rules: HashMap<usize, (&'a Type, usize)>,
    // Before-rewrite matches inside replacements can be shared by all queries.
    replacement_matches: Matches<'a>,
}

impl<'a> RuleIndex<'a> {
    fn new(table: &'a HashMap<Type, Type>) -> Self {
        let mut index = Self {
            steps: HashMap::default(),
            rules: HashMap::new(),
            replacement_matches: HashMap::new(),
        };
        for (input, output) in table {
            if input != output {
                let from = index.intern(input);
                let to = index.intern(output);
                index.rules.insert(from, (output, to));
            }
        }
        let mut matches = Matches::new();
        for &(output, _) in index.rules.values() {
            index.find_matches(output, &mut matches);
        }
        index.replacement_matches = matches;
        index
    }

    fn intern(&mut self, ty: &'a Type) -> usize {
        fold_keys(ty, |node, children| {
            let mut intern = |step| {
                let next = self.steps.len();
                *self.steps.entry(step).or_insert(next)
            };
            let mut key = intern(Step::head(node));
            for child in children {
                key = intern(Step::Child(key, *child));
            }
            key
        })
    }

    fn key(&self, head: Step<'_>, children: &[Option<usize>]) -> Option<usize> {
        let lookup = |step| self.steps.get(&step).copied();
        let mut key = lookup(head)?;
        for child in children {
            key = lookup(Step::Child(key, (*child)?))?;
        }
        Some(key)
    }

    fn find_matches(&self, ty: &Type, matches: &mut Matches<'a>) {
        if self.rules.is_empty() {
            return;
        }
        fold_keys(ty, |node, children| {
            let key = self.key(Step::head(node), children)?;
            if let Some(&(replacement, _)) = self.rules.get(&key) {
                matches.insert(std::ptr::from_ref(node).addr(), replacement);
            }
            Some(key)
        });
    }
}

/// Compute exact keys in postorder. The scratch stack holds only one ID per
/// pending child, including for deep types.
fn fold_keys<'a, K>(root: &'a Type, mut fold: impl FnMut(&'a Type, &[K]) -> K) -> K {
    if let ([], None) = root.child_parts() {
        return fold(root, &[]);
    }
    let mut work = vec![(root, None)];
    let mut keys = Vec::new();
    while let Some((node, count)) = work.pop() {
        if let Some(count) = count {
            let start = keys.len() - count;
            let key = fold(node, &keys[start..]);
            keys.truncate(start);
            keys.push(key);
        } else {
            let (args, ret) = node.child_parts();
            let count = args.len() + usize::from(ret.is_some());
            if count == 0 {
                keys.push(fold(node, &[]));
            } else {
                work.push((node, Some(count)));
                work.extend(args.iter().chain(ret).rev().map(|child| (child, None)));
            }
        }
    }
    keys.pop().expect("root key")
}

/// Postorder instructions for the surviving output. A post-rewrite can replace
/// its child instructions before any owned type tree is allocated.
enum Output<'a> {
    Copy(&'a Type),
    Named(Cow<'a, str>),
    Array,
    Generic(&'a str, usize),
    Callable { shared: bool, arity: usize },
}

impl<'a> Output<'a> {
    fn node(ty: &'a Type, tables: &'a PartialCheckerTables<'_>) -> Self {
        let enum_name = |name: &str| {
            tables
                .enums
                .and_then(|enums| enums.get(&crate::semantic::ids::TypeId::from_source_name(name)))
                .map(|info| info.name.as_str())
        };
        let interface_name = |name: &str| {
            tables
                .symbols
                .and_then(|s| s.lookup_interface(name))
                .map(|info| info.name.as_str())
        };
        match ty {
            Type::Named(name) => {
                let canonical = enum_name(name)
                    .or_else(|| {
                        tables
                            .symbols
                            .and_then(|s| s.lookup_class(name))
                            .map(|info| info.name.as_str())
                    })
                    .or_else(|| interface_name(name))
                    .unwrap_or(name);
                Self::Named(Cow::Borrowed(canonical))
            }
            Type::Array(_) | Type::FixedArray(..) => Self::Array,
            Type::Generic(name, args) => Self::Generic(
                interface_name(name)
                    .or_else(|| enum_name(name))
                    .unwrap_or(name),
                args.len(),
            ),
            Type::Fn(params, _) | Type::Closure(params, _) => Self::Callable {
                shared: matches!(ty, Type::Fn(..)),
                arity: params.len(),
            },
            _ => Self::Copy(ty),
        }
    }

    fn head(&self) -> Step<'_> {
        match self {
            Self::Copy(ty) => Step::head(ty),
            Self::Named(name) => Step::Named(name),
            Self::Array => Step::Array,
            Self::Generic(name, _) => Step::Generic(name),
            Self::Callable { shared, .. } => Step::Callable(*shared),
        }
    }

    fn build(self, values: &mut Vec<Type>, receiver: Option<&Type>) -> Type {
        match self {
            // Whole-type rewrites can introduce Self anywhere in their output.
            Self::Copy(ty) => match receiver {
                Some(receiver) => {
                    ty.substitute_names(|name| (name == "Self").then(|| receiver.clone()))
                }
                None => ty.clone(),
            },
            Self::Named(name) => match receiver {
                Some(receiver) if name == "Self" => receiver.clone(),
                _ => Type::Named(name.into_owned()),
            },
            Self::Array => Type::Array(Box::new(values.pop().expect("element"))),
            Self::Generic(name, arity) => {
                Type::Generic(name.into(), values.split_off(values.len() - arity))
            }
            Self::Callable { shared, arity } => {
                let ret = Box::new(values.pop().expect("return type"));
                let params = values.split_off(values.len() - arity);
                if shared {
                    Type::Fn(params, ret)
                } else {
                    Type::Closure(params, ret)
                }
            }
        }
    }
}

pub(super) fn normalize(
    tables: &PartialCheckerTables<'_>,
    root: &Type,
    bound: Option<&BoundNames<'_>>,
    receiver: Option<&Type>,
) -> Type {
    enum Work<'a> {
        Enter(&'a Type),
        Finish {
            node: Output<'a>,
            output_start: usize,
            keys_start: usize,
        },
    }
    let rules = tables.normalized_types.index();
    // A scalar with no rule is already canonical in every declaration scope.
    if let head @ Step::Atom(_) = Step::head(root)
        && rules
            .and_then(|r| r.rules.get(&r.key(head, &[])?))
            .is_none()
    {
        return root.clone();
    }
    let mut before = Matches::new();
    if let Some(rules) = rules {
        rules.find_matches(root, &mut before);
    }
    let mut work = vec![Work::Enter(root)];
    let mut keys = Vec::new();
    let mut output = Vec::new();
    while let Some(task) = work.pop() {
        let (node, output_start, keys_start) = match task {
            Work::Enter(original) => {
                if let Type::Named(name) = original {
                    let protected = if bound.is_some_and(|names| names.contains(name.as_str())) {
                        Some(Cow::Borrowed(name.as_str()))
                    } else {
                        tables.receiver_spelling(name).map(Cow::Owned)
                    };
                    if let Some(name) = protected {
                        let node = Output::Named(name);
                        if let Some(rules) = rules {
                            keys.push(rules.key(node.head(), &[]));
                        }
                        output.push(node);
                        continue;
                    }
                }
                let address = std::ptr::from_ref(original).addr();
                let ty = before
                    .get(&address)
                    .or_else(|| rules.and_then(|r| r.replacement_matches.get(&address)))
                    .copied()
                    .unwrap_or(original);
                let node = Output::node(ty, tables);
                let (args, ret) = ty.child_parts();
                if args.is_empty() && ret.is_none() {
                    (node, output.len(), keys.len())
                } else {
                    work.push(Work::Finish {
                        node,
                        output_start: output.len(),
                        keys_start: keys.len(),
                    });
                    work.extend(args.iter().chain(ret).rev().map(Work::Enter));
                    continue;
                }
            }
            Work::Finish {
                node,
                output_start,
                keys_start,
            } => (node, output_start, keys_start),
        };
        if let Some(rules) = rules {
            let key = rules.key(node.head(), &keys[keys_start..]);
            keys.truncate(keys_start);
            if let Some(&(replacement, result_key)) = key.and_then(|id| rules.rules.get(&id)) {
                output.truncate(output_start);
                output.push(Output::Copy(replacement));
                keys.push(Some(result_key));
                continue;
            }
            keys.push(key);
        }
        output.push(node);
    }
    let mut values = Vec::new();
    for node in output {
        let ty = node.build(&mut values, receiver);
        values.push(ty);
    }
    values.pop().expect("normalized root")
}
