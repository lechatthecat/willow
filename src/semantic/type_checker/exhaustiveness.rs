//! Reachability and exhaustiveness of `match` arms over nested patterns.
//!
//! One pass of usefulness checking in the style of Maranget, "Warnings for
//! pattern matching" (JFP 2007), in the single-pass form that marks useful
//! rows while it looks for a missing value. The pattern matrix is split on
//! its first column: every constructor present in that column gets the rows
//! that start with it (payloads spliced in as new columns) plus every
//! wildcard row; when the column does not mention all of its type's
//! constructors a "missing" branch keeps only the wildcard rows. With no
//! columns left, rows run in arm order: the first ones up to and including
//! the first unguarded row are reachable there, and a branch without an
//! unguarded row is a value no arm takes.
//!
//! Exhaustiveness of nested patterns is NP-complete in general, so no
//! algorithm is polynomial in every case. Here each row is moved into the
//! one branch of its head constructor and only wildcard rows are copied, so a
//! flat `match` costs O(arms + constructors x wildcard arms), and nesting adds
//! work only where several rows share constructors at several depths. The
//! walk keeps an explicit stack: deep or wide patterns use no native stack.

use std::collections::{HashMap, HashSet};

use super::*;
use crate::parser::ast::*;

#[cfg(test)]
thread_local! {
    /// Rows placed into a branch or visited at a leaf, for scaling tests.
    pub(super) static USEFULNESS_OPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[inline]
fn count_op() {
    #[cfg(test)]
    USEFULNESS_OPS.with(|ops| ops.set(ops.get() + 1));
}

/// The constructors a column's type has.
enum Shape {
    Bool,
    /// Variants in declaration order with their payload types (interned).
    Enum {
        name: String,
        variants: Vec<(String, Vec<usize>)>,
        index: HashMap<String, usize>,
    },
    /// No finite constructor set: integers, strings, classes, interfaces.
    Open,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Ctor<'p> {
    Variant(usize),
    Bool(bool),
    Int(i64),
    /// A class downcast, or anything else no finite type lists.
    Other(&'p str),
}

struct Row<'p> {
    arm: usize,
    guarded: bool,
    /// Pattern columns, first column last.
    cols: Vec<&'p Pattern>,
}

struct Problem<'p> {
    rows: Vec<Row<'p>>,
    /// Column types (interned), first column last.
    tys: Vec<usize>,
}

/// One branch of a split, and how its missing value becomes the parent's.
enum Wrap<'p> {
    /// Fold the first `arity` columns of the witness into `ctor` of `ty`.
    Rebuild {
        ty: usize,
        ctor: Ctor<'p>,
        arity: usize,
    },
    /// Put this value in front of the witness.
    Push(Pattern),
}

struct Split<'p> {
    branches: Vec<(Wrap<'p>, Problem<'p>)>,
}

enum Frame<'p> {
    Eval(Problem<'p>),
    Wrap(Wrap<'p>),
    /// Keep the first missing value among this many branch results.
    Gather(usize),
}

struct Usefulness<'c> {
    checker: &'c TypeChecker,
    span: Span,
    wildcard: &'c Pattern,
    ids: HashMap<Type, usize>,
    types: Vec<Type>,
    shapes: Vec<Option<Shape>>,
}

impl<'c> Usefulness<'c> {
    fn new(checker: &'c TypeChecker, span: Span, wildcard: &'c Pattern) -> Self {
        Self {
            checker,
            span,
            wildcard,
            ids: HashMap::new(),
            types: Vec::new(),
            shapes: Vec::new(),
        }
    }

    fn intern(&mut self, ty: &Type) -> usize {
        if let Some(&id) = self.ids.get(ty) {
            return id;
        }
        let id = self.types.len();
        self.ids.insert(ty.clone(), id);
        self.types.push(ty.clone());
        self.shapes.push(None);
        id
    }

    /// The type's constructors, computed once per type and only for columns
    /// that are actually split, so recursive enums cost nothing extra.
    fn shape(&mut self, ty: usize) -> &Shape {
        if self.shapes[ty].is_none() {
            let shape = self.compute_shape(ty);
            self.shapes[ty] = Some(shape);
        }
        self.shapes[ty].as_ref().expect("shape computed above")
    }

    fn compute_shape(&mut self, ty: usize) -> Shape {
        let (name, args) = match &self.types[ty] {
            Type::Bool => return Shape::Bool,
            Type::Named(name) => (name.clone(), Vec::new()),
            Type::Generic(name, args) => (name.clone(), args.clone()),
            _ => return Shape::Open,
        };
        let Some(info) = self.checker.symbols.lookup_enum(&name) else {
            return Shape::Open;
        };
        let info = info.instantiate(&args);
        let mut variants = Vec::with_capacity(info.variants.len());
        let mut index = HashMap::with_capacity(info.variants.len());
        for variant in &info.variants {
            let payload = variant
                .payload_types
                .iter()
                .map(|t| self.intern(t))
                .collect();
            index.insert(variant.name.clone(), variants.len());
            variants.push((variant.name.clone(), payload));
        }
        Shape::Enum {
            name,
            variants,
            index,
        }
    }

    /// The head constructor and payloads of a pattern in a column of
    /// `shape`, or `None` for a wildcard.
    fn head<'p>(shape: &Shape, pattern: &'p Pattern) -> Option<(Ctor<'p>, &'p [Pattern])> {
        let variant = |name: &'p str| match shape {
            Shape::Enum { index, .. } => index
                .get(name)
                .map_or(Ctor::Other(name), |&i| Ctor::Variant(i)),
            _ => Ctor::Other(name),
        };
        match pattern {
            Pattern::Wildcard(..) | Pattern::Binding { .. } => None,
            Pattern::LiteralBool(value, ..) => Some((Ctor::Bool(*value), &[])),
            Pattern::LiteralInt(value, ..) => Some((Ctor::Int(*value), &[])),
            Pattern::EnumVariant { variant: name, .. } => Some((variant(name), &[])),
            Pattern::EnumVariantTuple {
                variant: name,
                fields,
                ..
            } => Some((variant(name), fields)),
            Pattern::ClassDowncast { class_name, .. } => Some((Ctor::Other(class_name), &[])),
        }
    }

    fn arity(shape: &Shape, ctor: Ctor<'_>) -> usize {
        match (shape, ctor) {
            (Shape::Enum { variants, .. }, Ctor::Variant(i)) => variants[i].1.len(),
            _ => 0,
        }
    }

    /// The value `ctor` of type `ty` with these payloads.
    fn build(&mut self, ty: usize, ctor: Ctor<'_>, fields: Vec<Pattern>) -> Pattern {
        let span = self.span;
        let id = PatternId::fresh();
        match (self.shape(ty), ctor) {
            (_, Ctor::Bool(value)) => Pattern::LiteralBool(value, span, id),
            (_, Ctor::Int(value)) => Pattern::LiteralInt(value, span, id),
            (Shape::Enum { name, variants, .. }, Ctor::Variant(i)) if fields.is_empty() => {
                Pattern::EnumVariant {
                    enum_name: name.clone(),
                    variant: variants[i].0.clone(),
                    variant_span: span,
                    span,
                    id,
                }
            }
            (Shape::Enum { name, variants, .. }, Ctor::Variant(i)) => Pattern::EnumVariantTuple {
                enum_name: name.clone(),
                variant: variants[i].0.clone(),
                variant_span: span,
                fields,
                span,
                id,
            },
            (_, Ctor::Other(name)) => Pattern::ClassDowncast {
                class_name: name.to_owned(),
                binding: "_".into(),
                span,
                id,
            },
            (_, Ctor::Variant(_)) => Pattern::Wildcard(span, id),
        }
    }

    /// Every constructor of `ty` absent from `present`, each with wildcard
    /// payloads; a single `_` when the type has no finite list or nothing
    /// in the column named a constructor.
    fn missing(
        &mut self,
        ty: usize,
        present: &HashMap<Ctor<'_>, usize>,
        all: bool,
    ) -> Vec<Pattern> {
        let ctors: Vec<(Ctor<'static>, usize)> = match self.shape(ty) {
            _ if present.is_empty() && !all => Vec::new(),
            Shape::Bool => [true, false]
                .into_iter()
                .map(Ctor::Bool)
                .filter(|c| !present.contains_key(c))
                .map(|c| (c, 0))
                .collect(),
            Shape::Enum { variants, .. } => {
                let mut out = Vec::new();
                for (i, (_, payload)) in variants.iter().enumerate() {
                    if !present.contains_key(&Ctor::Variant(i)) {
                        out.push((Ctor::Variant(i), payload.len()));
                        if !all {
                            break;
                        }
                    }
                }
                out
            }
            Shape::Open => Vec::new(),
        };
        if ctors.is_empty() {
            return vec![self.wildcard.clone()];
        }
        ctors
            .into_iter()
            .map(|(ctor, arity)| {
                let fields = vec![self.wildcard.clone(); arity];
                self.build(ty, ctor, fields)
            })
            .collect()
    }

    /// Split a problem with at least one column on that column. Returns the
    /// branches in constructor order (declaration order for enums), then the
    /// missing branch, and the constructors present in the column.
    fn split<'p>(&mut self, problem: Problem<'p>) -> (Split<'p>, HashMap<Ctor<'p>, usize>)
    where
        'c: 'p,
    {
        let Problem { rows, mut tys } = problem;
        let ty = tys.pop().expect("split needs a column");
        self.shape(ty);
        let shape = self.shapes[ty].as_ref().expect("shape computed above");
        let mut present: Vec<Ctor<'p>> = Vec::new();
        let mut seen: HashMap<Ctor<'p>, usize> = HashMap::new();
        for row in &rows {
            let pattern = *row.cols.last().expect("row has a column per type");
            if let Some((ctor, _)) = Self::head(shape, pattern)
                && !seen.contains_key(&ctor)
            {
                seen.insert(ctor, 0);
                present.push(ctor);
            }
        }
        // Report in declaration order; literal constructors keep first use.
        if let Shape::Enum { .. } = shape {
            present.sort_by_key(|ctor| match ctor {
                Ctor::Variant(i) => *i,
                _ => usize::MAX,
            });
        }
        let complete = match shape {
            Shape::Bool => present.len() == 2,
            Shape::Enum { variants, .. } => {
                present
                    .iter()
                    .filter(|c| matches!(c, Ctor::Variant(_)))
                    .count()
                    == variants.len()
            }
            Shape::Open => false,
        };
        let mut branches: Vec<(Wrap<'p>, Problem<'p>)> = Vec::with_capacity(present.len() + 1);
        for (slot, ctor) in present.iter().enumerate() {
            seen.insert(*ctor, slot);
            let arity = Self::arity(shape, *ctor);
            let mut branch_tys = tys.clone();
            if let (Shape::Enum { variants, .. }, Ctor::Variant(i)) = (shape, ctor) {
                branch_tys.extend(variants[*i].1.iter().rev());
            }
            branches.push((
                Wrap::Rebuild {
                    ty,
                    ctor: *ctor,
                    arity,
                },
                Problem {
                    rows: Vec::new(),
                    tys: branch_tys,
                },
            ));
        }
        let arities: Vec<usize> = present.iter().map(|c| Self::arity(shape, *c)).collect();
        let mut default = (!complete).then(|| Problem {
            rows: Vec::new(),
            tys: tys.clone(),
        });
        for mut row in rows {
            let pattern = row.cols.pop().expect("row has a column per type");
            match Self::head(shape, pattern) {
                Some((ctor, fields)) => {
                    count_op();
                    row.cols.extend(fields.iter().rev());
                    branches[seen[&ctor]].1.rows.push(row);
                }
                None => {
                    for ((_, branch), &arity) in branches.iter_mut().zip(&arities) {
                        count_op();
                        let mut cols = Vec::with_capacity(row.cols.len() + arity);
                        cols.extend_from_slice(&row.cols);
                        cols.extend(std::iter::repeat_n(self.wildcard, arity));
                        branch.rows.push(Row {
                            arm: row.arm,
                            guarded: row.guarded,
                            cols,
                        });
                    }
                    if let Some(default) = &mut default {
                        count_op();
                        default.rows.push(row);
                    }
                }
            }
        }
        if let Some(default) = default {
            let missing = self
                .missing(ty, &seen, false)
                .pop()
                .expect("at least one missing value");
            branches.push((Wrap::Push(missing), default));
        }
        (Split { branches }, seen)
    }

    /// The first value (as a column stack, first column last) that no row
    /// takes, marking every row that takes some value as useful.
    fn solve<'p>(&mut self, problem: Problem<'p>, useful: &mut [bool]) -> Option<Vec<Pattern>>
    where
        'c: 'p,
    {
        let mut frames = vec![Frame::Eval(problem)];
        let mut results: Vec<Option<Vec<Pattern>>> = Vec::new();
        while let Some(frame) = frames.pop() {
            match frame {
                Frame::Eval(problem) if problem.tys.is_empty() => {
                    let mut covered = false;
                    for row in &problem.rows {
                        count_op();
                        useful[row.arm] = true;
                        if !row.guarded {
                            covered = true;
                            break;
                        }
                    }
                    results.push((!covered).then(Vec::new));
                }
                Frame::Eval(problem) if problem.rows.is_empty() => {
                    results.push(Some(vec![self.wildcard.clone(); problem.tys.len()]));
                }
                Frame::Eval(problem) => {
                    let (split, _) = self.split(problem);
                    frames.push(Frame::Gather(split.branches.len()));
                    for (wrap, branch) in split.branches.into_iter().rev() {
                        frames.push(Frame::Wrap(wrap));
                        frames.push(Frame::Eval(branch));
                    }
                }
                Frame::Wrap(wrap) => {
                    let result = results.pop().expect("branch result");
                    let wrapped = result.map(|witness| self.wrap(wrap, witness));
                    results.push(wrapped);
                }
                Frame::Gather(count) => {
                    let first = results.len() - count;
                    let found = results.drain(first..).flatten().next();
                    results.push(found);
                }
            }
        }
        results.pop().expect("one result")
    }

    fn wrap(&mut self, wrap: Wrap<'_>, mut witness: Vec<Pattern>) -> Vec<Pattern> {
        match wrap {
            Wrap::Push(pattern) => witness.push(pattern),
            Wrap::Rebuild { ty, ctor, arity } => {
                let fields: Vec<Pattern> = witness.drain(witness.len() - arity..).rev().collect();
                let value = self.build(ty, ctor, fields);
                witness.push(value);
            }
        }
        witness
    }
}

/// What a `match` leaves uncovered and which arms are reachable.
struct Coverage {
    /// Missing values in top-level constructor order, each with whether no
    /// unguarded arm names its top-level variant (the whole variant is
    /// uncovered, not only some value inside it).
    missing: Vec<(Pattern, bool)>,
    first_unreachable: Option<usize>,
    open: bool,
}

impl TypeChecker {
    fn match_coverage(&self, m: &MatchExpr, scrutinee_ty: &Type) -> Coverage {
        let wildcard = Pattern::Wildcard(m.span, PatternId::fresh());
        let mut usefulness = Usefulness::new(self, m.span, &wildcard);
        let ty = usefulness.intern(scrutinee_ty);
        let rows = m
            .arms
            .iter()
            .enumerate()
            .map(|(arm, source)| Row {
                arm,
                guarded: !source.guard.is_empty(),
                cols: vec![
                    self.pattern_resolutions
                        .get(&source.pattern.id())
                        .unwrap_or(&source.pattern),
                ],
            })
            .collect::<Vec<Row<'_>>>();
        // Top-level constructors some unguarded arm names: a value missing
        // under one of them is a missing value, not a missing variant.
        usefulness.shape(ty);
        let shape = usefulness.shapes[ty]
            .as_ref()
            .expect("shape computed above");
        let unguarded: HashSet<Ctor<'_>> = rows
            .iter()
            .filter(|row| !row.guarded)
            .filter_map(|row| Usefulness::head(shape, row.cols[0]).map(|(ctor, _)| ctor))
            .collect();
        let mut useful = vec![false; m.arms.len()];
        let mut missing = Vec::new();
        // The top-level split is unrolled so that every missing variant is
        // reported, as the flat check always did, not only the first one.
        let (split, present) = usefulness.split(Problem {
            rows,
            tys: vec![ty],
        });
        for (wrap, branch) in split.branches {
            let Some(witness) = usefulness.solve(branch, &mut useful) else {
                continue;
            };
            match wrap {
                Wrap::Push(_) => missing.extend(
                    usefulness
                        .missing(ty, &present, true)
                        .into_iter()
                        .map(|value| (value, true)),
                ),
                rebuild => {
                    let named = match &rebuild {
                        Wrap::Rebuild { ctor, .. } => unguarded.contains(ctor),
                        Wrap::Push(_) => false,
                    };
                    missing.extend(
                        usefulness
                            .wrap(rebuild, witness)
                            .into_iter()
                            .map(|value| (value, !named)),
                    );
                }
            }
        }
        let open = matches!(usefulness.shape(ty), Shape::Open);
        Coverage {
            missing,
            first_unreachable: useful.iter().position(|reached| !reached),
            open,
        }
    }

    /// Warn on the first unreachable arm and reject a `match` some value
    /// escapes. Returns whether every value reaches an arm.
    pub(super) fn check_match_coverage(&mut self, m: &MatchExpr, scrutinee_ty: &Type) -> bool {
        let coverage = self.match_coverage(m, scrutinee_ty);
        if let Some(arm) = coverage.first_unreachable {
            self.push(Self::unreachable_arm_diagnostic(m, &m.arms[arm]));
        }
        let Some((first, _)) = coverage.missing.first() else {
            return true;
        };
        if m.source == MatchSource::TupleLet {
            self.push(
                Self::error_at(
                    ErrorCode::E1213,
                    format!("refutable pattern in `let`: `{first}` not covered"),
                    m.arms[0].pattern.span(),
                    "this pattern does not match every value",
                )
                .with_help("use `let ... else { ... }` or `match` to handle the other values"),
            );
            return false;
        }
        match scrutinee_ty {
            Type::Bool => self.push(
                Self::error_at(
                    ErrorCode::E1207,
                    "non-exhaustive match: missing bool patterns",
                    m.span,
                    "match is not exhaustive",
                )
                .with_help("add `true` and `false` patterns, or use a wildcard `_`"),
            ),
            Type::U8 | Type::I64 => self.push_error(
                ErrorCode::E1206,
                format!(
                    "non-exhaustive match on `{}`: add a wildcard arm `_ => ...`",
                    self.diagnostic_type_name(scrutinee_ty)
                ),
                m.span,
                "match is not exhaustive",
            ),
            // Interfaces, strings and other open types keep their old rule:
            // a downcast-only `match` simply takes no arm for other values.
            _ if coverage.open => {}
            _ => {
                // Only the generic form has always carried the help line.
                let generic = matches!(scrutinee_ty, Type::Generic(..));
                for (value, absent) in &coverage.missing {
                    // A whole variant left uncovered (no arm names it, or
                    // only guarded arms do) keeps the variant wording; a
                    // value missing inside a named variant shows the value.
                    let flat = *absent;
                    let diagnostic = if let Pattern::EnumVariant {
                        enum_name, variant, ..
                    }
                    | Pattern::EnumVariantTuple {
                        enum_name, variant, ..
                    } = value
                        && flat
                    {
                        let diagnostic = Self::error_at(
                            ErrorCode::E1202,
                            format!(
                                "non-exhaustive match: variant `{enum_name}::{variant}` not covered"
                            ),
                            m.span,
                            "match is not exhaustive",
                        );
                        if generic {
                            diagnostic
                                .with_help("add the missing variant or use a wildcard `_` arm")
                        } else {
                            diagnostic
                        }
                    } else {
                        Self::error_at(
                            ErrorCode::E1202,
                            format!("non-exhaustive match: pattern `{value}` not covered"),
                            m.span,
                            "match is not exhaustive",
                        )
                        .with_help("add an arm for this pattern, or a wildcard `_` arm")
                    };
                    self.push(diagnostic);
                }
            }
        }
        false
    }
}
