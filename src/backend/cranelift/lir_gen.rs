//! LIR-walking code generation — willow-0g8j.
//!
//! First stage of migrating the emit layer off the raw AST: a function whose
//! lowered IR uses only the supported scalar subset is compiled by walking its
//! [`LirFunction`] basic blocks directly (typed [`HirExpr`] trees inside), so
//! the backend never touches the AST body for it. Everything else falls back to
//! the existing AST-walking path, chosen per function in
//! `compile_function_named`. `WILLOW_LIR_BACKEND=0` disables the LIR path;
//! `WILLOW_LIR_REQUIRE=1` turns the fallback into a hard compile error, so a
//! test can pin a function to this path instead of passing vacuously when a
//! lowering or eligibility regression sends it back to the AST walker.
//!
//! Supported subset (v4): `i64`/`f64`/`bool`/`String`/`Array<T>` values and
//! SIMPLE class objects; literals, variables, arithmetic/comparison, unary ops,
//! string concatenation and content comparison; array literals, indexing,
//! index-assignment and the builtin `len`/`push`/`pop`/`toString` methods;
//! `new`, field reads, field assignment, instance and static method calls;
//! direct calls to known non-async functions; `print`/`println` of a scalar or
//! a string; `let`/assign; the full block control flow (jump/branch/return).
//!
//! A class is SIMPLE when it has no base class, is not itself a base, is
//! neither an interface nor an enum, has a known field layout, and every field
//! type is itself supported. Inheritance dispatches virtually and an
//! interface-typed slot needs a boxing coercion, so both stay on the AST path —
//! and because [`LirTypeCtx::supported_type`] rejects every interface `Named`
//! type outright, no coercion site (let-init, assign, argument, return, field
//! store, array element) can reach the walker. Nullable types, enums, maps,
//! async, and lambdas also stay on the AST path for now.
//!
//! GC rooting (willow-0g8j.1): the LIR has no block scopes — it is a flat
//! basic-block graph — so a per-`let` push/pop pairing like the AST path's
//! would grow the shadow root stack once per loop iteration. Instead every
//! GC-managed local gets ONE stack slot allocated and rooted at function
//! entry, null-initialized so a collection before the `let` runs sees an empty
//! slot; the slot is both the variable's storage and its root (the AST
//! invariant that keeps a reassignment from leaving a stale root), and all
//! roots are popped at each `return`. Expression temporaries that must survive
//! an allocating call are rooted exactly as the AST path roots them.
//!
//! Arrays (willow-0g8j.4) ride on the same discipline: an `Array<T>` value is a
//! GC handle whose pointer is stable across growth (a `push` reallocates only
//! the buffer), so the entry slot of an array local stays valid for the whole
//! function. Temporaries are rooted around any sub-expression that can run a
//! collection, decided by [`may_allocate`] so a literal index or a constant
//! element list costs no root traffic.

use std::collections::{HashMap, HashSet};

use cranelift_codegen::ir::{
    InstBuilder, MemFlagsData, StackSlotData, StackSlotKind, condcodes::FloatCC, condcodes::IntCC,
    types,
};
use cranelift_module::Module;

use crate::ir::lowered::{LirBlock, LirFunction, LirInst, Terminator};
use crate::ir::typed_ast::{HirExpr, HirExprKind};
use crate::parser::ast::{BinOp, ParamMode, Type, UnaryOp};
use crate::semantic::ids::FunctionMap;

use super::emit_interface::collection_elem_kind;
use super::gc_codegen::{GcLayoutMetadata, GcStoreDestination};
use super::symbols::{class_method_symbol_name, class_name_for_object_type};
use super::type_helpers::{clif_type, is_gc_managed};
use super::{BuildMode, FuncGen, VarStorage, array_element_type};

/// True when the environment does not disable the LIR backend.
pub(super) fn lir_backend_enabled() -> bool {
    std::env::var("WILLOW_LIR_BACKEND")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// True when the environment demands that every function compile through the
/// LIR path. A fallback to the AST emitter is then a compile error naming the
/// function (willow-0g8j.4 review): the differential tests compare "LIR on"
/// against "LIR off", and without this a function that stopped being eligible
/// would just compare the AST path with itself and still pass.
///
/// Opt-in, so ordinary builds keep falling back silently for everything the
/// walker does not support yet.
pub(super) fn lir_required() -> bool {
    std::env::var("WILLOW_LIR_REQUIRE")
        .map(|v| v != "0")
        .unwrap_or(false)
}

fn scalar(ty: &Type) -> bool {
    matches!(ty, Type::I64 | Type::F64 | Type::Bool)
}

/// Whether a *supported* type is a GC-managed heap reference. The supported set
/// contains no enums, so — unlike [`is_gc_managed`] — this needs no enum table
/// and can be answered during eligibility, before a `FuncGen` exists.
fn gc_managed_supported(ty: &Type) -> bool {
    matches!(ty, Type::String | Type::Array(_) | Type::Named(_))
}

/// Whether a value of type `value` can be stored into a slot declared `target`
/// with no coercion. The walker emits none (willow-0g8j.5), so a class value
/// flowing into an interface-typed slot — the one place the AST path inserts a
/// boxing conversion — must be rejected rather than stored raw. Interfaces are
/// already outside [`LirTypeCtx::supported_type`], so the class arms below are
/// the belt to that suspenders: two *different* named types never share a
/// representation the walker may assume.
fn assignable_repr(target: &Type, value: &Type) -> bool {
    match (target, value) {
        (Type::Named(a), Type::Named(b)) => a == b,
        (Type::Named(_), _) | (_, Type::Named(_)) => false,
        // An empty literal `[]` infers `Array<Void>`; the handle it produces is
        // the same shape as any other array's, so only the handle matters.
        (Type::Array(_), Type::Array(_)) => true,
        _ => {
            clif_type(target) == clif_type(value)
                && gc_managed_supported(target) == gc_managed_supported(value)
        }
    }
}

/// The program facts eligibility needs beyond the lowered IR itself: which
/// named types are classes the walker can lay out, which symbols exist, and
/// what those symbols' signatures are. Built from the compiler's registration
/// tables at the dispatch site in `compile_function_named`.
pub(super) struct LirTypeCtx<'x> {
    /// Whether a symbol name is a declared/linkable function.
    pub known_fn: &'x dyn Fn(&str) -> bool,
    pub class_layouts: &'x HashMap<String, Vec<(String, Type)>>,
    pub class_base: &'x HashMap<String, String>,
    /// Runtime `type_id` per class NAME. A direct type import (`import
    /// zoo::Animal;`) registers the imported class a second time under its
    /// unqualified name, sharing the canonical class's id — so this is what
    /// makes class IDENTITY comparable across those two names.
    pub class_type_ids: &'x HashMap<String, i64>,
    /// Whether a name is registered as an interface (never a class here).
    pub is_interface: &'x dyn Fn(&str) -> bool,
    /// Whether a name is registered as an enum (never a class here).
    pub is_enum: &'x dyn Fn(&str) -> bool,
    pub fn_types: &'x FunctionMap<Type>,
    pub func_param_modes: &'x FunctionMap<Vec<ParamMode>>,
    pub known_modules: &'x HashMap<String, String>,
}

impl LirTypeCtx<'_> {
    /// Types the LIR walker can hold in a value position: the scalars, `Void`,
    /// `String`, `Array<T>` over a supported `T`, and — since willow-0g8j.5 —
    /// a *simple class* (see [`Self::supported_class`]). Enums, interfaces,
    /// maps, nullable types, generics and function types still fall back to the
    /// AST path.
    ///
    /// An array's element type must itself be supported: an
    /// `Array<SomeInterface>` would need the class → interface boxing coercion
    /// that only the AST path applies, and storing an unboxed class value
    /// through it would miscompile.
    pub(super) fn supported_type(&self, ty: &Type) -> bool {
        let mut open = HashSet::new();
        self.supported_type_inner(ty, &mut open)
    }

    fn supported_type_inner(&self, ty: &Type, open: &mut HashSet<String>) -> bool {
        match ty {
            Type::Array(elem) => {
                !matches!(**elem, Type::Void) && self.supported_type_inner(elem, open)
            }
            Type::Named(name) => self.supported_class_inner(name, open),
            _ => scalar(ty) || matches!(ty, Type::Void | Type::String),
        }
    }

    /// A class the walker can emit: it has a registered field layout, is not an
    /// interface or an enum, takes no part in inheritance, and every field type
    /// is itself supported.
    ///
    /// Inheritance is the hard exclusion. With no base and no subclass, a
    /// receiver's static type is *exact*, so a direct call to `Class__method`
    /// is the whole of dispatch; the moment a class is extended, the AST path's
    /// runtime `type_id` dispatch chain becomes load-bearing and the walker
    /// would silently call the wrong implementation.
    pub(super) fn supported_class(&self, name: &str) -> bool {
        let mut open = HashSet::new();
        self.supported_class_inner(name, &mut open)
    }

    fn supported_class_inner(&self, name: &str, open: &mut HashSet<String>) -> bool {
        if (self.is_interface)(name) || (self.is_enum)(name) {
            return false;
        }
        let Some(layout) = self.class_layouts.get(name) else {
            return false;
        };
        if self.participates_in_inheritance(name) {
            return false;
        }
        // A self- or mutually-referential field (`class Node { next: Node; }`)
        // is fine — it is the same layout — but must not recurse forever.
        if !open.insert(name.to_string()) {
            return true;
        }
        layout
            .iter()
            .all(|(_, ty)| self.supported_type_inner(ty, open))
    }

    /// Whether `name` is the base or the subclass of some `extends` edge, under
    /// ANY of its names.
    ///
    /// A direct type import (`import zoo::Animal;`) copies the imported class's
    /// tables under the unqualified `Animal`, but `class_base` keeps CANONICAL
    /// names on both sides of every edge (`zoo::Dog` → `zoo::Animal`). Comparing
    /// `"Animal"` to those strings finds nothing, so a plain name comparison
    /// would call an imported base class a leaf and let the walker emit a direct
    /// `Animal__speak` for a receiver that is really a `Dog` — the exact
    /// miscompile the inheritance exclusion exists to prevent. Aliased and
    /// canonical names share one runtime `type_id`, so compare that instead and
    /// fall back to the name when a class has no id registered.
    fn participates_in_inheritance(&self, name: &str) -> bool {
        let id = self.class_type_ids.get(name).copied();
        let is_same = |other: &str| {
            other == name || (id.is_some() && self.class_type_ids.get(other).copied() == id)
        };
        self.class_base
            .iter()
            .any(|(sub, base)| is_same(sub) || is_same(base))
    }

    /// The declared field layout of a supported class named by `ty`.
    fn class_layout_of(&self, ty: &Type) -> Option<&Vec<(String, Type)>> {
        let Type::Named(name) = ty else { return None };
        if !self.supported_class(name) {
            return None;
        }
        self.class_layouts.get(name)
    }

    /// Whether a direct call to the symbol `mangled` is emittable with `args`:
    /// the symbol exists, no parameter is by-reference (the walker passes
    /// values, never addresses), and every declared parameter type is supported
    /// and matches its argument without coercion. `skip_self` drops the hidden
    /// receiver parameter that class methods and static calls carry.
    fn callable(&self, mangled: &str, args: &[HirExpr], skip_self: bool) -> bool {
        if !(self.known_fn)(mangled) {
            return false;
        }
        if self
            .func_param_modes
            .get(mangled)
            .is_some_and(|modes| modes.iter().any(|m| !matches!(m, ParamMode::Value)))
        {
            return false;
        }
        let Some(Type::Fn(params, _)) = self.fn_types.get(mangled) else {
            // No recorded signature (a runtime symbol, or a shape the front end
            // did not register): only argument types whose representation the
            // walker cannot get wrong may reach it.
            return args.iter().all(|a| !matches!(a.ty, Type::Named(_)));
        };
        let params: &[Type] = if skip_self {
            match params.split_first() {
                Some((_, rest)) => rest,
                None => return false,
            }
        } else {
            params
        };
        params.len() == args.len()
            && params
                .iter()
                .zip(args)
                .all(|(p, a)| self.supported_type(p) && assignable_repr(p, &a.ty))
    }
}

/// Conservative "can evaluating this run a collection?" test, used to decide
/// whether a live GC temporary has to be rooted across a sub-expression. Only
/// the forms known to be allocation-free answer `false`; everything else —
/// including any expression form added later — answers `true`, so a new node
/// kind cannot silently drop a root.
fn may_allocate(e: &HirExpr) -> bool {
    match &e.kind {
        HirExprKind::Int(_) | HirExprKind::Float(_) | HirExprKind::Bool(_) => false,
        HirExprKind::Var(_) => false,
        HirExprKind::Unary { operand, .. } => may_allocate(operand),
        // String concatenation allocates; equality/inequality only compare
        // bytes and collect only if evaluating an operand can allocate.
        HirExprKind::Binary { op, lhs, rhs } => {
            (lhs.ty == Type::String && matches!(op, BinOp::Add))
                || may_allocate(lhs)
                || may_allocate(rhs)
        }
        HirExprKind::Ternary {
            condition,
            then_expr,
            else_expr,
        } => may_allocate(condition) || may_allocate(then_expr) || may_allocate(else_expr),
        // `willow_array_get` only reads through the handle.
        HirExprKind::Index { array, index } => may_allocate(array) || may_allocate(index),
        // A field read is a plain load at a fixed offset; the debug nil check
        // only branches and traps (willow-0g8j.5).
        HirExprKind::FieldAccess { object, .. } => may_allocate(object),
        _ => true,
    }
}

/// Conservative eligibility: every type, instruction, and expression must be in
/// the supported subset, every callee must be a known symbol, every variable
/// must be a parameter or a `let` of this function, and binding names must be
/// unique across it (LIR flattens block scopes, so shadowing across sibling
/// scopes — or over a parameter — would alias one variable).
pub(super) fn lir_supported_function(f: &LirFunction, ctx: &LirTypeCtx<'_>) -> bool {
    if !ctx.supported_type(&f.return_type) {
        return false;
    }
    // Reference parameters (`&`/`&mut`) are pointers at the ABI level.
    if !f
        .params
        .iter()
        .all(|p| ctx.supported_type(&p.ty) && !p.by_reference)
    {
        return false;
    }
    // Names the walker can resolve, mapped to the type they are BOUND with (not
    // the initialiser's type — see `LirInst::Let::ty`). Any other `Var` is
    // something the HIR spells like a variable but codegen must special-case —
    // a bare enum variant, a function used as a value — so the function falls
    // back (willow-0g8j.1).
    let mut names: HashMap<&str, &Type> = HashMap::new();
    for p in &f.params {
        if names.insert(p.name.as_str(), &p.ty).is_some() {
            return false;
        }
    }
    for block in &f.blocks {
        for inst in &block.instrs {
            if let LirInst::Let { name, ty, .. } = inst
                && names.insert(name.as_str(), ty).is_some()
            {
                return false; // shadows a parameter or another `let`
            }
        }
    }

    for block in &f.blocks {
        for inst in &block.instrs {
            match inst {
                LirInst::Let { ty, value, .. } => {
                    // The binding type is the slot's type; an annotation that
                    // widens the initialiser (`let a: Animal = new Dog();`)
                    // would need the AST path's boxing coercion.
                    if !ctx.supported_type(ty)
                        || !assignable_repr(ty, &value.ty)
                        || !supported_expr(value, ctx, &names)
                    {
                        return false;
                    }
                }
                LirInst::Assign { name, value } => {
                    let Some(declared) = names.get(name.as_str()) else {
                        return false;
                    };
                    if !assignable_repr(declared, &value.ty) || !supported_expr(value, ctx, &names)
                    {
                        return false;
                    }
                }
                LirInst::Expr(e) => {
                    if !supported_expr(e, ctx, &names) {
                        return false;
                    }
                }
                LirInst::IndexAssign {
                    array,
                    index,
                    value,
                } => {
                    let Type::Array(elem) = &array.ty else {
                        return false;
                    };
                    if !assignable_repr(elem, &value.ty)
                        || !supported_expr(array, ctx, &names)
                        || !supported_expr(index, ctx, &names)
                        || !supported_expr(value, ctx, &names)
                    {
                        return false;
                    }
                }
                // `obj.field = value;` on a simple class (willow-0g8j.5).
                LirInst::FieldAssign {
                    object,
                    field,
                    value,
                } => {
                    let Some(field_ty) = ctx
                        .class_layout_of(&object.ty)
                        .and_then(|l| l.iter().find(|(n, _)| n == field))
                        .map(|(_, ty)| ty.clone())
                    else {
                        return false;
                    };
                    if !assignable_repr(&field_ty, &value.ty)
                        || !supported_expr(object, ctx, &names)
                        || !supported_expr(value, ctx, &names)
                    {
                        return false;
                    }
                }
                _ => return false,
            }
        }
        match &block.terminator {
            Terminator::Branch { cond, .. } => {
                if !supported_expr(cond, ctx, &names) {
                    return false;
                }
            }
            Terminator::Return(Some(v)) => {
                // A `void`-typed return value has no slot in the Cranelift
                // signature; the walker would emit `return_(&[v])` on a
                // zero-result function.
                if v.ty == Type::Void
                    || !assignable_repr(&f.return_type, &v.ty)
                    || !supported_expr(v, ctx, &names)
                {
                    return false;
                }
            }
            Terminator::Jump(_) | Terminator::Return(None) => {}
        }
    }
    true
}

fn supported_expr(e: &HirExpr, ctx: &LirTypeCtx<'_>, names: &HashMap<&str, &Type>) -> bool {
    if !ctx.supported_type(&e.ty) {
        return false;
    }
    match &e.kind {
        HirExprKind::Int(_) | HirExprKind::Float(_) | HirExprKind::Bool(_) => true,
        HirExprKind::Str(_) => true,
        HirExprKind::Var(name) => names.contains_key(name.as_str()),
        HirExprKind::Binary { op, lhs, rhs } => {
            // On strings only `+` (concat) and content comparison are emitted.
            if lhs.ty == Type::String && !matches!(op, BinOp::Add | BinOp::Eq | BinOp::Ne) {
                return false;
            }
            // Class values have no operators: `==` on two objects would be an
            // identity comparison the walker does not emit.
            if matches!(lhs.ty, Type::Named(_)) || matches!(rhs.ty, Type::Named(_)) {
                return false;
            }
            supported_expr(lhs, ctx, names) && supported_expr(rhs, ctx, names)
        }
        HirExprKind::Ternary {
            condition,
            then_expr,
            else_expr,
        } => {
            supported_expr(condition, ctx, names)
                && supported_expr(then_expr, ctx, names)
                && supported_expr(else_expr, ctx, names)
        }
        HirExprKind::Unary { operand, .. } => supported_expr(operand, ctx, names),
        HirExprKind::Call { callee, args } => {
            ctx.callable(callee.as_str(), args, false)
                && args.iter().all(|a| supported_expr(a, ctx, names))
        }
        HirExprKind::Print { value, newline: _ } => {
            (scalar(&value.ty) || value.ty == Type::String) && supported_expr(value, ctx, names)
        }
        // The element type is already vetted by `supported_type(&e.ty)` above.
        HirExprKind::Array { elements } => {
            let elem = array_element_type(&e.ty);
            elements
                .iter()
                .all(|el| assignable_repr(&elem, &el.ty) && supported_expr(el, ctx, names))
        }
        // Only real arrays: `FrozenArray<T>`, `Map<K, V>` and `Range<i64>` also
        // spell their reads as `Index` but need different runtime calls.
        HirExprKind::Index { array, index } => {
            matches!(array.ty, Type::Array(_))
                && supported_expr(array, ctx, names)
                && supported_expr(index, ctx, names)
        }
        // `new Class(args)` — explicit `Class__init` or the implicit memberwise
        // constructor (willow-0g8j.5).
        HirExprKind::New { class, args } => {
            let Some(layout) = ctx.class_layout_of(&e.ty) else {
                return false;
            };
            if !matches!(&e.ty, Type::Named(n) if n == class) {
                return false;
            }
            let mangled = class_method_symbol_name(ctx.known_modules, class, "init");
            let shape_ok = if (ctx.known_fn)(&mangled) {
                ctx.callable(&mangled, args, true)
            } else {
                // Memberwise: positional args fill the declared fields in order.
                args.len() == layout.len()
                    && layout
                        .iter()
                        .zip(args)
                        .all(|((_, fty), a)| assignable_repr(fty, &a.ty))
            };
            shape_ok && args.iter().all(|a| supported_expr(a, ctx, names))
        }
        // `Class { field: value, ... }` — the given names must be exactly the
        // declared fields, each once. Matching only the COUNT would accept
        // `Point { x: 1, x: 2 }`, which the emitter would store twice into `x`
        // and leave `y` at its zero value.
        //
        // The type checker rejects this syntax in source today (E0847), so this
        // arm only guards the node's internal use; the check stays exact so the
        // predicate and the emitter cannot drift apart if it comes back.
        HirExprKind::ObjectLiteral { class, fields } => {
            let Some(layout) = ctx.class_layout_of(&e.ty) else {
                return false;
            };
            if !matches!(&e.ty, Type::Named(n) if n == class) || fields.len() != layout.len() {
                return false;
            }
            let mut seen: HashSet<&str> = HashSet::new();
            fields.iter().all(|(name, value)| {
                seen.insert(name.as_str())
                    && layout
                        .iter()
                        .find(|(n, _)| n == name)
                        .is_some_and(|(_, fty)| assignable_repr(fty, &value.ty))
                    && supported_expr(value, ctx, names)
            })
        }
        // `object.field` on a simple class. `Range<i64>.start/.end` and every
        // other receiver stay on the AST path.
        HirExprKind::FieldAccess { object, field } => {
            ctx.class_layout_of(&object.ty)
                .and_then(|l| l.iter().find(|(n, _)| n == field))
                .is_some_and(|(_, fty)| assignable_repr(fty, &e.ty))
                && supported_expr(object, ctx, names)
        }
        // The builtin array methods the walker emits, plus a direct call to a
        // method of a simple class. Anything else on an array (`freeze`,
        // `map`, …) and every other receiver falls back.
        HirExprKind::MethodCall {
            object,
            method,
            args,
        } => match &object.ty {
            Type::Array(elem) => {
                let shape_ok = match method.as_str() {
                    "len" | "pop" => args.is_empty(),
                    "push" => args.len() == 1 && assignable_repr(elem, &args[0].ty),
                    // `toString` renders elements in the runtime, which only
                    // knows the four scalar/string element kinds.
                    "toString" => args.is_empty() && collection_elem_kind(elem).is_some(),
                    _ => false,
                };
                shape_ok
                    && supported_expr(object, ctx, names)
                    && args.iter().all(|a| supported_expr(a, ctx, names))
            }
            Type::Named(class) if ctx.supported_class(class) => {
                let mangled = class_method_symbol_name(ctx.known_modules, class, method);
                ctx.callable(&mangled, args, true)
                    && ctx.fn_types.get(&mangled).is_some_and(
                        |t| matches!(t, Type::Fn(_, ret) if assignable_repr(ret, &e.ty)),
                    )
                    && supported_expr(object, ctx, names)
                    && args.iter().all(|a| supported_expr(a, ctx, names))
            }
            _ => false,
        },
        // `Class::method(args)` — a static method of a simple class only. A
        // module call (`math::add`), a builtin namespace (`fs`, `env`) and an
        // enum variant constructor all spell themselves the same way and need
        // the AST path's special cases.
        HirExprKind::StaticCall {
            class,
            method,
            args,
        } => {
            if ctx.known_modules.contains_key(class) || !ctx.supported_class(class) {
                return false;
            }
            let mangled = class_method_symbol_name(ctx.known_modules, class, method);
            ctx.callable(&mangled, args, true)
                && ctx
                    .fn_types
                    .get(&mangled)
                    .is_some_and(|t| matches!(t, Type::Fn(_, ret) if assignable_repr(ret, &e.ty)))
                && args.iter().all(|a| supported_expr(a, ctx, names))
        }
        _ => false,
    }
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Emit a whole function body by walking its LIR block graph. The entry
    /// block's instructions land in the already-created Cranelift entry block
    /// (parameters are bound there); every other LIR block gets its own.
    /// All paths are terminated by the LIR, so the caller must skip its
    /// implicit-return epilogue.
    pub(super) fn emit_lir_function(&mut self, f: &LirFunction) {
        let entry = self.builder.current_block().expect("entry block active");
        self.bind_lir_gc_locals(f);
        let mut blocks = vec![entry];
        for _ in 1..f.blocks.len() {
            blocks.push(self.builder.create_block());
        }

        for (i, block) in f.blocks.iter().enumerate() {
            if i > 0 {
                self.builder.switch_to_block(blocks[i]);
            }
            self.emit_lir_block(block, &blocks, &f.return_type);
        }
        self.builder.seal_all_blocks();
        self.terminated = true;
    }

    /// Give every GC-managed `let` of this function one entry-allocated, rooted
    /// stack slot (see the module docs). The slot is null-initialized so a
    /// collection that happens before the `let` executes reads an empty root
    /// rather than uninitialized stack memory. GC-managed *parameters* already
    /// got the same treatment from `bind_param`, so they are skipped here.
    fn bind_lir_gc_locals(&mut self, f: &LirFunction) {
        let ptr_ty = self.module.target_config().pointer_type();
        let mut null = None;
        for block in &f.blocks {
            for inst in &block.instrs {
                let LirInst::Let { name, ty, .. } = inst else {
                    continue;
                };
                if !is_gc_managed(ty, self.enum_infos) {
                    continue;
                }
                let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    0,
                ));
                let zero = *null.get_or_insert_with(|| self.builder.ins().iconst(ptr_ty, 0));
                self.builder.ins().stack_store(zero, slot, 0);
                self.emit_push_root_slot(slot);
                self.vars.insert(
                    name.clone(),
                    VarStorage::Stack {
                        slot,
                        ty: ty.clone(),
                    },
                );
            }
        }
    }

    fn emit_lir_block(
        &mut self,
        block: &LirBlock,
        blocks: &[cranelift_codegen::ir::Block],
        return_type: &Type,
    ) {
        for inst in &block.instrs {
            match inst {
                LirInst::Let {
                    name, ty, value, ..
                } => {
                    let val = self.emit_lir_expr(value);
                    // A GC-managed local already has its rooted slot from
                    // `bind_lir_gc_locals`; storing into it is the whole binding.
                    if let Some(storage @ VarStorage::Stack { .. }) =
                        self.vars.get(name.as_str()).cloned()
                    {
                        self.store_var(&storage, val);
                        continue;
                    }
                    let var = self.builder.declare_var(clif_type(ty));
                    self.builder.def_var(var, val);
                    self.vars.insert(
                        name.clone(),
                        VarStorage::Value {
                            var,
                            ty: ty.clone(),
                        },
                    );
                }
                LirInst::Assign { name, value } => {
                    let val = self.emit_lir_expr(value);
                    if let Some(storage) = self.vars.get(name.as_str()).cloned() {
                        self.store_var(&storage, val);
                    }
                }
                LirInst::Expr(e) => {
                    self.emit_lir_expr(e);
                }
                LirInst::IndexAssign {
                    array,
                    index,
                    value,
                } => {
                    // Null and out-of-bounds are checked inside `willow_array_set`.
                    let elem_ty = array_element_type(&array.ty);
                    let arr = self.emit_lir_expr(array);
                    // The index and the value are evaluated after the array
                    // handle is in hand, so the handle needs a root if either
                    // of them can collect.
                    let rooted = may_allocate(index) || may_allocate(value);
                    if rooted {
                        self.emit_push_root(arr);
                    }
                    let idx = self.emit_lir_expr(index);
                    let val = self.emit_lir_expr(value);
                    let word = self.coerce_to_i64(val, &elem_ty);
                    let set_id = self.func_id("willow_array_set");
                    let set_ref = self.module.declare_func_in_func(set_id, self.builder.func);
                    self.builder.ins().call(set_ref, &[arr, idx, word]);
                    if rooted {
                        self.emit_pop_roots_n(1);
                        self.gc_root_count -= 1;
                    }
                }
                LirInst::FieldAssign {
                    object,
                    field,
                    value,
                } => self.emit_lir_field_assign(object, field, value),
                // Filtered out by eligibility.
                _ => unreachable!("unsupported LIR instruction reached emission"),
            }
        }
        match &block.terminator {
            Terminator::Jump(b) => {
                self.builder.ins().jump(blocks[b.0], &[]);
            }
            Terminator::Branch {
                cond,
                then_block,
                else_block,
            } => {
                let c = self.emit_lir_expr(cond);
                self.builder
                    .ins()
                    .brif(c, blocks[then_block.0], &[], blocks[else_block.0], &[]);
            }
            Terminator::Return(Some(v)) => {
                // Evaluate first: the value may read through a rooted local.
                let val = self.emit_lir_expr(v);
                self.emit_pop_roots_n(self.gc_root_count);
                self.builder.ins().return_(&[val]);
            }
            Terminator::Return(None) => {
                self.emit_pop_roots_n(self.gc_root_count);
                if *return_type == Type::Void {
                    self.builder.ins().return_(&[]);
                } else {
                    // Unreachable fall-through in a value function (the checker
                    // guarantees returns); satisfy the signature with a zero.
                    let zero = match clif_type(return_type) {
                        types::F64 => self.builder.ins().f64const(0.0),
                        ty => self.builder.ins().iconst(ty, 0),
                    };
                    self.builder.ins().return_(&[zero]);
                }
            }
        }
    }

    fn emit_lir_expr(&mut self, e: &HirExpr) -> cranelift_codegen::ir::Value {
        match &e.kind {
            HirExprKind::Int(n) => self.builder.ins().iconst(types::I64, *n),
            HirExprKind::Float(x) => self.builder.ins().f64const(*x),
            HirExprKind::Bool(b) => self.builder.ins().iconst(types::I8, i64::from(*b)),
            HirExprKind::Str(s) => self.emit_string_literal(s),
            HirExprKind::Var(name) => match self.vars.get(name.as_str()).cloned() {
                Some(storage) => self.load_var(&storage),
                // Same loud failure as the AST path (willow-thqe class): a
                // variable the eligibility check admitted must be bound.
                None => {
                    panic!("internal compiler error: variable `{name}` reached LIR codegen unbound")
                }
            },
            HirExprKind::Binary { op, lhs, rhs } if lhs.ty == Type::String => {
                self.emit_lir_string_binop(op, lhs, rhs)
            }
            HirExprKind::Binary { op, lhs, rhs } => match op {
                // Short-circuit: the rhs must not evaluate when the lhs decides.
                BinOp::And | BinOp::Or => {
                    let l = self.emit_lir_expr(lhs);
                    let result_var = self.builder.declare_var(types::I8);
                    let rhs_block = self.builder.create_block();
                    let short_block = self.builder.create_block();
                    let merge_block = self.builder.create_block();
                    if matches!(op, BinOp::And) {
                        self.builder.ins().brif(l, rhs_block, &[], short_block, &[]);
                    } else {
                        self.builder.ins().brif(l, short_block, &[], rhs_block, &[]);
                    }

                    self.builder.switch_to_block(rhs_block);
                    self.builder.seal_block(rhs_block);
                    let r = self.emit_lir_expr(rhs);
                    self.builder.def_var(result_var, r);
                    self.builder.ins().jump(merge_block, &[]);

                    self.builder.switch_to_block(short_block);
                    self.builder.seal_block(short_block);
                    let short_val = self
                        .builder
                        .ins()
                        .iconst(types::I8, i64::from(matches!(op, BinOp::Or)));
                    self.builder.def_var(result_var, short_val);
                    self.builder.ins().jump(merge_block, &[]);

                    self.builder.switch_to_block(merge_block);
                    self.builder.seal_block(merge_block);
                    self.builder.use_var(result_var)
                }
                _ => {
                    let float = lhs.ty == Type::F64;
                    let l = self.emit_lir_expr(lhs);
                    let r = self.emit_lir_expr(rhs);
                    if !float && matches!(op, BinOp::Div | BinOp::Rem) {
                        self.emit_int_div_guard(l, r, matches!(op, BinOp::Rem), e.span);
                    }
                    self.emit_lir_binop(op, l, r, float)
                }
            },
            HirExprKind::Ternary {
                condition,
                then_expr,
                else_expr,
            } => {
                let result_var = self.builder.declare_var(clif_type(&e.ty));
                let then_block = self.builder.create_block();
                let else_block = self.builder.create_block();
                let merge_block = self.builder.create_block();

                let cond = self.emit_lir_expr(condition);
                self.builder
                    .ins()
                    .brif(cond, then_block, &[], else_block, &[]);

                self.builder.switch_to_block(then_block);
                self.builder.seal_block(then_block);
                let t = self.emit_lir_expr(then_expr);
                self.builder.def_var(result_var, t);
                self.builder.ins().jump(merge_block, &[]);

                self.builder.switch_to_block(else_block);
                self.builder.seal_block(else_block);
                let f = self.emit_lir_expr(else_expr);
                self.builder.def_var(result_var, f);
                self.builder.ins().jump(merge_block, &[]);

                self.builder.switch_to_block(merge_block);
                self.builder.seal_block(merge_block);
                self.builder.use_var(result_var)
            }
            HirExprKind::Unary { op, operand } => {
                let val = self.emit_lir_expr(operand);
                match op {
                    UnaryOp::Neg if operand.ty == Type::F64 => self.builder.ins().fneg(val),
                    UnaryOp::Neg => self.builder.ins().ineg(val),
                    UnaryOp::Not => {
                        let one = self.builder.ins().iconst(types::I8, 1);
                        self.builder.ins().bxor(val, one)
                    }
                }
            }
            HirExprKind::Call { callee, args } => {
                let (vals, temp_roots) = self.emit_lir_args_rooted(args);
                let fid = self.func_ids[callee.as_str()];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                // Debug builds record the call on the panic call-chain stack,
                // exactly like the AST path (willow-992h).
                let pushed = self.emit_callstack_push(callee, e.span);
                let call = self.builder.ins().call(fref, &vals);
                let results = self.builder.inst_results(call);
                let result = results
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0));
                if pushed {
                    self.emit_callstack_pop();
                }
                self.emit_pop_roots_n(temp_roots);
                self.gc_root_count -= temp_roots;
                result
            }
            HirExprKind::Print { value, newline } => {
                let val = self.emit_lir_expr(value);
                let fn_name = match (&value.ty, newline) {
                    (Type::I64, false) => "willow_print_i64",
                    (Type::I64, true) => "willow_println_i64",
                    (Type::F64, false) => "willow_print_f64",
                    (Type::F64, true) => "willow_println_f64",
                    (Type::Bool, false) => "willow_print_bool",
                    (Type::Bool, true) => "willow_println_bool",
                    (Type::String, false) => "willow_print_string",
                    (Type::String, true) => "willow_println_string",
                    _ => unreachable!("unsupported print type passed eligibility"),
                };
                let fid = self.func_ids[fn_name];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder.ins().call(fref, &[val]);
                self.builder.ins().iconst(types::I8, 0)
            }
            HirExprKind::Array { elements } => {
                self.emit_lir_array_literal(elements, &array_element_type(&e.ty))
            }
            HirExprKind::Index { array, index } => self.emit_lir_index(array, index),
            HirExprKind::MethodCall {
                object,
                method,
                args,
            } => match &object.ty {
                Type::Array(_) => self.emit_lir_array_method(object, method, args),
                _ => self.emit_lir_class_method(object, method, args, &e.ty, e.span),
            },
            HirExprKind::New { class, args } => self.emit_lir_new(class, args),
            HirExprKind::ObjectLiteral { class, fields } => {
                self.emit_lir_object_literal(class, fields)
            }
            HirExprKind::FieldAccess { object, field } => self.emit_lir_field_access(object, field),
            HirExprKind::StaticCall {
                class,
                method,
                args,
            } => self.emit_lir_static_call(class, method, args, &e.ty),
            _ => unreachable!("unsupported LIR expression reached emission"),
        }
    }

    /// Evaluate call arguments left to right, rooting each GC-managed one as it
    /// is produced: a later argument (or the callee itself) can allocate and
    /// collect, and an already-evaluated argument is otherwise only held in an
    /// SSA register the GC cannot see (same rule as the AST path). Returns the
    /// values and the number of roots the caller must pop after the call.
    fn emit_lir_args_rooted(
        &mut self,
        args: &[HirExpr],
    ) -> (Vec<cranelift_codegen::ir::Value>, usize) {
        let mut vals = Vec::with_capacity(args.len());
        let mut temp_roots = 0usize;
        for a in args {
            let val = self.emit_lir_expr(a);
            if is_gc_managed(&a.ty, self.enum_infos) {
                self.emit_push_root(val);
                temp_roots += 1;
            }
            vals.push(val);
        }
        (vals, temp_roots)
    }

    /// The class layout for a receiver/field-owner type. Eligibility already
    /// proved the type is a simple class with a registered layout.
    fn lir_class_layout(&self, ty: &Type) -> Vec<(String, Type)> {
        let class =
            class_name_for_object_type(ty).expect("class receiver type vetted by LIR eligibility");
        self.class_layouts
            .get(&class)
            .cloned()
            .expect("class layout vetted by LIR eligibility")
    }

    /// `new Class(args)`: allocate the object with its GC ref mask, stamp the
    /// runtime `type_id` into word 0, then run the explicit `Class__init` or —
    /// when the class has none — store the positional arguments memberwise.
    ///
    /// The fresh object is rooted for the whole construction: argument
    /// evaluation and the constructor body can both allocate, and until this
    /// returns nothing else refers to it.
    fn emit_lir_new(&mut self, class: &str, args: &[HirExpr]) -> cranelift_codegen::ir::Value {
        let layout = self
            .class_layouts
            .get(class)
            .cloned()
            .expect("class layout vetted by LIR eligibility");
        let type_id = self.class_type_ids.get(class).copied().unwrap_or(0);
        let gc_layout = GcLayoutMetadata::class(class, type_id, &layout, self.enum_infos);
        let ptr = self.emit_gc_alloc(gc_layout);
        let type_id_val = self.builder.ins().iconst(types::I64, type_id);
        self.builder
            .ins()
            .store(MemFlagsData::new(), type_id_val, ptr, 0i32);
        self.emit_push_root(ptr);

        let mangled = class_method_symbol_name(self.known_modules, class, "init");
        if let Some(&init_fid) = self.func_ids.get(&mangled) {
            let (arg_vals, arg_roots) = self.emit_lir_args_rooted(args);
            let init_ref = self
                .module
                .declare_func_in_func(init_fid, self.builder.func);
            let mut call_args = vec![ptr];
            call_args.extend(arg_vals);
            self.builder.ins().call(init_ref, &call_args);
            if arg_roots > 0 {
                self.emit_pop_roots_n(arg_roots);
                self.gc_root_count -= arg_roots;
            }
        } else {
            // Implicit memberwise constructor. Each value is stored the instant
            // it exists, so it is unrooted only across an allocation-free window.
            for (i, arg) in args.iter().enumerate() {
                let field_ty = layout[i].1.clone();
                let val = self.emit_lir_expr(arg);
                self.emit_gc_heap_store(
                    ptr,
                    (i as i32 + 1) * 8,
                    val,
                    &field_ty,
                    GcStoreDestination::ObjectField,
                );
            }
        }

        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        ptr
    }

    /// `Class { field: value, ... }`: the same allocation as `new`, with the
    /// fields addressed by name instead of position.
    fn emit_lir_object_literal(
        &mut self,
        class: &str,
        fields: &[(String, HirExpr)],
    ) -> cranelift_codegen::ir::Value {
        let layout = self
            .class_layouts
            .get(class)
            .cloned()
            .expect("class layout vetted by LIR eligibility");
        let type_id = self.class_type_ids.get(class).copied().unwrap_or(0);
        let gc_layout = GcLayoutMetadata::class(class, type_id, &layout, self.enum_infos);
        let ptr = self.emit_gc_alloc(gc_layout);
        let type_id_val = self.builder.ins().iconst(types::I64, type_id);
        self.builder
            .ins()
            .store(MemFlagsData::new(), type_id_val, ptr, 0i32);
        self.emit_push_root(ptr);

        for (name, value) in fields {
            let idx = layout
                .iter()
                .position(|(n, _)| n == name)
                .expect("object-literal field vetted by LIR eligibility");
            let field_ty = layout[idx].1.clone();
            let val = self.emit_lir_expr(value);
            self.emit_gc_heap_store(
                ptr,
                (idx as i32 + 1) * 8,
                val,
                &field_ty,
                GcStoreDestination::ObjectField,
            );
        }

        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        ptr
    }

    /// `object.field`: a plain load at `(index + 1) * 8` — word 0 holds the
    /// runtime type_id. Debug builds check the receiver for nil first, exactly
    /// as the AST path does.
    fn emit_lir_field_access(
        &mut self,
        object: &HirExpr,
        field: &str,
    ) -> cranelift_codegen::ir::Value {
        let layout = self.lir_class_layout(&object.ty);
        let ptr = self.emit_lir_expr(object);
        if self.build_mode == BuildMode::Debug {
            self.emit_nil_check(ptr, object.span, field);
        }
        let idx = layout
            .iter()
            .position(|(n, _)| n == field)
            .expect("field vetted by LIR eligibility");
        let load_ty = clif_type(&layout[idx].1);
        self.builder
            .ins()
            .load(load_ty, MemFlagsData::new(), ptr, (idx as i32 + 1) * 8)
    }

    /// `object.field = value;` through the GC write barrier, so an old-space
    /// object that gains a young reference lands in the remembered set.
    fn emit_lir_field_assign(&mut self, object: &HirExpr, field: &str, value: &HirExpr) {
        let layout = self.lir_class_layout(&object.ty);
        let idx = layout
            .iter()
            .position(|(n, _)| n == field)
            .expect("field vetted by LIR eligibility");
        let field_ty = layout[idx].1.clone();
        let ptr = self.emit_lir_expr(object);
        if self.build_mode == BuildMode::Debug {
            self.emit_nil_check(ptr, object.span, field);
        }
        // The owner is evaluated first, so it needs a root whenever producing
        // the value can collect.
        let rooted = may_allocate(value);
        if rooted {
            self.emit_push_root(ptr);
        }
        let val = self.emit_lir_expr(value);
        self.emit_gc_heap_store(
            ptr,
            (idx as i32 + 1) * 8,
            val,
            &field_ty,
            GcStoreDestination::ObjectField,
        );
        if rooted {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
    }

    /// `object.method(args)` on a simple class. Eligibility guarantees the
    /// receiver's static type is exact (the class neither inherits nor is
    /// inherited from), so this is a direct call to `Class__method` and needs
    /// none of the AST path's runtime type_id dispatch chain.
    fn emit_lir_class_method(
        &mut self,
        object: &HirExpr,
        method: &str,
        args: &[HirExpr],
        ret_ty: &Type,
        span: crate::diagnostics::Span,
    ) -> cranelift_codegen::ir::Value {
        let class = class_name_for_object_type(&object.ty)
            .expect("class receiver type vetted by LIR eligibility");
        let self_ptr = self.emit_lir_expr(object);
        if self.build_mode == BuildMode::Debug {
            self.emit_nil_check(self_ptr, object.span, method);
        }
        let mangled = class_method_symbol_name(self.known_modules, &class, method);
        let fid = self.func_ids[&mangled];

        let pushed = self.emit_callstack_push(method, span);
        // The receiver may be a temporary (`make().m(alloc())`) reachable only
        // through this register; an allocating argument could collect it.
        self.emit_push_root(self_ptr);
        let (arg_vals, arg_roots) = self.emit_lir_args_rooted(args);
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let mut call_args = vec![self_ptr];
        call_args.extend(arg_vals);
        let call = self.builder.ins().call(fref, &call_args);
        let result = self
            .builder
            .inst_results(call)
            .first()
            .copied()
            .unwrap_or_else(|| self.builder.ins().iconst(clif_type(ret_ty), 0));
        if pushed {
            self.emit_callstack_pop();
        }
        self.emit_pop_roots_n(arg_roots + 1);
        self.gc_root_count -= arg_roots + 1;
        result
    }

    /// `Class::method(args)`: class methods always carry a hidden receiver
    /// parameter, so a static call passes a null `self` — the same convention
    /// the AST path uses.
    fn emit_lir_static_call(
        &mut self,
        class: &str,
        method: &str,
        args: &[HirExpr],
        ret_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let mangled = class_method_symbol_name(self.known_modules, class, method);
        let fid = self.func_ids[&mangled];
        let dummy_self = self.builder.ins().iconst(types::I64, 0);
        let (arg_vals, arg_roots) = self.emit_lir_args_rooted(args);
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let mut call_args = vec![dummy_self];
        call_args.extend(arg_vals);
        let call = self.builder.ins().call(fref, &call_args);
        let result = self
            .builder
            .inst_results(call)
            .first()
            .copied()
            .unwrap_or_else(|| self.builder.ins().iconst(clif_type(ret_ty), 0));
        if arg_roots > 0 {
            self.emit_pop_roots_n(arg_roots);
            self.gc_root_count -= arg_roots;
        }
        result
    }

    /// `String` `+` / `==` / `!=`. Concatenation allocates; equality only
    /// compares bytes. The left operand is rooted across evaluation of the
    /// right operand because that expression may allocate. Keeping that root
    /// through the allocation-free equality call is conservative and mirrors
    /// the AST path.
    fn emit_lir_string_binop(
        &mut self,
        op: &BinOp,
        lhs: &HirExpr,
        rhs: &HirExpr,
    ) -> cranelift_codegen::ir::Value {
        let l = self.emit_lir_expr(lhs);
        self.emit_push_root(l);
        let r = self.emit_lir_expr(rhs);
        let (rt, roots) = match op {
            BinOp::Add => {
                // The concat call allocates, so the right operand must be a
                // root too — it may itself be a fresh temporary.
                self.emit_push_root(r);
                ("willow_string_concat", 2)
            }
            BinOp::Eq | BinOp::Ne => ("willow_string_eq", 1),
            _ => unreachable!("non-concat/compare string operator passed eligibility"),
        };
        let fid = self.func_id(rt);
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[l, r]);
        let raw = self.builder.inst_results(call)[0];
        self.emit_pop_roots_n(roots);
        self.gc_root_count -= roots;
        match op {
            BinOp::Add => raw,
            // `willow_string_eq` answers in a word; the language's bool is i8.
            BinOp::Eq => self.builder.ins().ireduce(types::I8, raw),
            _ => {
                let inv = self.builder.ins().bxor_imm(raw, 1);
                self.builder.ins().ireduce(types::I8, inv)
            }
        }
    }

    /// `[e0, e1, ...]`: allocate the handle, then fill it in place. Elements
    /// cross the runtime ABI as raw 64-bit words. The fresh array is rooted
    /// while the elements are evaluated, but only when one of them can actually
    /// collect — a list of constants cannot.
    fn emit_lir_array_literal(
        &mut self,
        elements: &[HirExpr],
        elem_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let len_val = self.builder.ins().iconst(types::I64, elements.len() as i64);
        let is_ref = i64::from(is_gc_managed(elem_ty, self.enum_infos));
        let is_ref_val = self.builder.ins().iconst(types::I64, is_ref);
        let new_id = self.func_id("willow_array_new");
        let new_ref = self.module.declare_func_in_func(new_id, self.builder.func);
        let call = self.builder.ins().call(new_ref, &[len_val, is_ref_val]);
        let arr = self.builder.inst_results(call)[0];

        let rooted = elements.iter().any(may_allocate);
        if rooted {
            self.emit_push_root(arr);
        }
        for (i, el) in elements.iter().enumerate() {
            // Each element is stored immediately, so it is only unrooted for
            // the allocation-free window between its own value and the `set`.
            let val = self.emit_lir_expr(el);
            let word = self.coerce_to_i64(val, elem_ty);
            let idx_val = self.builder.ins().iconst(types::I64, i as i64);
            let set_id = self.func_id("willow_array_set");
            let set_ref = self.module.declare_func_in_func(set_id, self.builder.func);
            self.builder.ins().call(set_ref, &[arr, idx_val, word]);
        }
        if rooted {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
        arr
    }

    /// `arr[index]`: bounds-checked element read, converted back from the
    /// uniform 64-bit word to the element type.
    fn emit_lir_index(&mut self, array: &HirExpr, index: &HirExpr) -> cranelift_codegen::ir::Value {
        let elem_ty = array_element_type(&array.ty);
        let arr = self.emit_lir_expr(array);
        // The array may be a temporary (`build()[i]`) that nothing else roots.
        let rooted = may_allocate(index);
        if rooted {
            self.emit_push_root(arr);
        }
        let idx = self.emit_lir_expr(index);
        let get_id = self.func_id("willow_array_get");
        let get_ref = self.module.declare_func_in_func(get_id, self.builder.func);
        let call = self.builder.ins().call(get_ref, &[arr, idx]);
        let word = self.builder.inst_results(call)[0];
        if rooted {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
        self.coerce_i64_to(word, &elem_ty)
    }

    /// The builtin `Array<T>` methods admitted by eligibility. `push` can grow
    /// the buffer, but the runtime roots both the handle and the pushed word
    /// across that reallocation, and the handle pointer itself never moves.
    fn emit_lir_array_method(
        &mut self,
        object: &HirExpr,
        method: &str,
        args: &[HirExpr],
    ) -> cranelift_codegen::ir::Value {
        let elem_ty = array_element_type(&object.ty);
        let arr = self.emit_lir_expr(object);
        match method {
            "len" => {
                let id = self.func_id("willow_array_len");
                let r = self.module.declare_func_in_func(id, self.builder.func);
                let call = self.builder.ins().call(r, &[arr]);
                self.builder.inst_results(call)[0]
            }
            "push" => {
                let rooted = may_allocate(&args[0]);
                if rooted {
                    self.emit_push_root(arr);
                }
                let v = self.emit_lir_expr(&args[0]);
                let word = self.coerce_to_i64(v, &elem_ty);
                let id = self.func_id("willow_array_push");
                let r = self.module.declare_func_in_func(id, self.builder.func);
                self.builder.ins().call(r, &[arr, word]);
                if rooted {
                    self.emit_pop_roots_n(1);
                    self.gc_root_count -= 1;
                }
                self.builder.ins().iconst(types::I8, 0) // void
            }
            "pop" => {
                let id = self.func_id("willow_array_pop");
                let r = self.module.declare_func_in_func(id, self.builder.func);
                let call = self.builder.ins().call(r, &[arr]);
                let word = self.builder.inst_results(call)[0];
                self.coerce_i64_to(word, &elem_ty)
            }
            "toString" => {
                let kind = collection_elem_kind(&elem_ty)
                    .expect("array toString element kind vetted by eligibility");
                let kind_val = self.builder.ins().iconst(types::I64, kind);
                let id = self.func_id("willow_array_to_string");
                let r = self.module.declare_func_in_func(id, self.builder.func);
                let call = self.builder.ins().call(r, &[arr, kind_val]);
                self.builder.inst_results(call)[0]
            }
            _ => unreachable!("unsupported array method passed eligibility"),
        }
    }

    fn emit_lir_binop(
        &mut self,
        op: &BinOp,
        l: cranelift_codegen::ir::Value,
        r: cranelift_codegen::ir::Value,
        float: bool,
    ) -> cranelift_codegen::ir::Value {
        let ins = self.builder.ins();
        if float {
            return match op {
                BinOp::Add => ins.fadd(l, r),
                BinOp::Sub => ins.fsub(l, r),
                BinOp::Mul => ins.fmul(l, r),
                BinOp::Div => ins.fdiv(l, r),
                BinOp::Rem => unreachable!("f64 % is rejected by the checker"),
                BinOp::Eq => ins.fcmp(FloatCC::Equal, l, r),
                BinOp::Ne => ins.fcmp(FloatCC::NotEqual, l, r),
                BinOp::Lt => ins.fcmp(FloatCC::LessThan, l, r),
                BinOp::Le => ins.fcmp(FloatCC::LessThanOrEqual, l, r),
                BinOp::Gt => ins.fcmp(FloatCC::GreaterThan, l, r),
                BinOp::Ge => ins.fcmp(FloatCC::GreaterThanOrEqual, l, r),
                BinOp::And | BinOp::Or => unreachable!("short-circuit ops rejected"),
            };
        }
        match op {
            BinOp::Add => ins.iadd(l, r),
            BinOp::Sub => ins.isub(l, r),
            BinOp::Mul => ins.imul(l, r),
            BinOp::Div => ins.sdiv(l, r),
            BinOp::Rem => ins.srem(l, r),
            BinOp::Eq => ins.icmp(IntCC::Equal, l, r),
            BinOp::Ne => ins.icmp(IntCC::NotEqual, l, r),
            BinOp::Lt => ins.icmp(IntCC::SignedLessThan, l, r),
            BinOp::Le => ins.icmp(IntCC::SignedLessThanOrEqual, l, r),
            BinOp::Gt => ins.icmp(IntCC::SignedGreaterThan, l, r),
            BinOp::Ge => ins.icmp(IntCC::SignedGreaterThanOrEqual, l, r),
            BinOp::And | BinOp::Or => unreachable!("short-circuit ops rejected"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::typed_ast::HirStmt;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    /// The registration tables [`LirTypeCtx`] borrows, derived from a parsed
    /// single-file program the same way `compile_program` derives them: class
    /// layouts and bases from `class`/`extends`, interface and enum names from
    /// their declarations, and one signature per declared symbol (free
    /// functions, `Class__method`, and `Class__init` for an explicit
    /// constructor). Modules are out of scope for these unit tests, so
    /// `known_modules` is empty and mangling is the plain `Class__method` form.
    struct TestTables {
        known: HashSet<String>,
        class_layouts: HashMap<String, Vec<(String, Type)>>,
        class_base: HashMap<String, String>,
        class_type_ids: HashMap<String, i64>,
        interfaces: HashSet<String>,
        enums: HashSet<String>,
        fn_types: FunctionMap<Type>,
        param_modes: FunctionMap<Vec<ParamMode>>,
        known_modules: HashMap<String, String>,
    }

    impl TestTables {
        fn build(program: &crate::parser::ast::Program, extra_fns: &[&str]) -> Self {
            use crate::parser::ast::Item;
            let mut t = TestTables {
                known: extra_fns.iter().map(|s| s.to_string()).collect(),
                class_layouts: HashMap::new(),
                class_base: HashMap::new(),
                class_type_ids: HashMap::new(),
                interfaces: HashSet::new(),
                enums: HashSet::new(),
                fn_types: FunctionMap::default(),
                param_modes: FunctionMap::default(),
                known_modules: HashMap::new(),
            };
            let sig = |params: &[crate::parser::ast::Param], ret: &Type, with_self: bool| {
                let mut ps: Vec<Type> = Vec::new();
                if with_self {
                    ps.push(Type::I64);
                }
                ps.extend(params.iter().map(|p| p.ty.clone()));
                Type::Fn(ps, Box::new(ret.clone()))
            };
            let modes = |params: &[crate::parser::ast::Param], with_self: bool| {
                let mut ms: Vec<ParamMode> = Vec::new();
                if with_self {
                    ms.push(ParamMode::Value);
                }
                ms.extend(params.iter().map(|p| p.mode.clone()));
                ms
            };
            for item in &program.items {
                match item {
                    // A free function's SIGNATURE is always recorded, but its
                    // name is a known symbol only when the test lists it: that
                    // is how a test models a callee the backend cannot link.
                    Item::Function(f) => {
                        t.fn_types
                            .insert(&f.name, sig(&f.params, &f.return_type, false));
                        t.param_modes.insert(&f.name, modes(&f.params, false));
                    }
                    Item::Interface(i) => {
                        t.interfaces.insert(i.name.clone());
                    }
                    Item::Enum(e) => {
                        t.enums.insert(e.name.clone());
                    }
                    Item::Class(c) => {
                        t.class_layouts.insert(
                            c.name.clone(),
                            c.fields
                                .iter()
                                .filter(|f| !f.is_static)
                                .map(|f| (f.name.clone(), f.ty.clone()))
                                .collect(),
                        );
                        if let Some(base) = &c.base_class {
                            t.class_base.insert(c.name.clone(), base.name().to_string());
                        }
                        // Same rule as `register_class`: one id per class, in
                        // declaration order.
                        let next_id = t.class_type_ids.len() as i64 + 1;
                        t.class_type_ids.entry(c.name.clone()).or_insert(next_id);
                        // Every class method — instance, static, or a
                        // constructor lowered to `init` — carries a hidden
                        // `self` parameter in its signature; a STATIC one is
                        // simply called with a null receiver. `func_param_modes`
                        // records only the declared parameters, matching
                        // `declare_class_methods`.
                        for ctor in &c.constructors {
                            let mangled = format!("{}__init", c.name);
                            t.known.insert(mangled.clone());
                            t.fn_types
                                .insert(&mangled, sig(&ctor.params, &Type::Void, true));
                            t.param_modes.insert(&mangled, modes(&ctor.params, false));
                        }
                        for m in &c.methods {
                            let mangled = format!("{}__{}", c.name, m.name);
                            t.known.insert(mangled.clone());
                            t.fn_types
                                .insert(&mangled, sig(&m.params, &m.return_type, true));
                            t.param_modes.insert(&mangled, modes(&m.params, false));
                        }
                    }
                }
            }
            t
        }

        /// The closures in [`LirTypeCtx`] are borrowed, so the context cannot
        /// outlive this call — hand it to the caller instead of returning it.
        fn with_ctx<R>(&self, body: impl FnOnce(&LirTypeCtx<'_>) -> R) -> R {
            body(&LirTypeCtx {
                known_fn: &|n| self.known.contains(n),
                class_layouts: &self.class_layouts,
                class_base: &self.class_base,
                class_type_ids: &self.class_type_ids,
                is_interface: &|n| self.interfaces.contains(n),
                is_enum: &|n| self.enums.contains(n),
                fn_types: &self.fn_types,
                func_param_modes: &self.param_modes,
                known_modules: &self.known_modules,
            })
        }
    }

    fn eligible(src: &str, name: &str, fns: &[&str]) -> bool {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (hir, diags) = crate::ir::lower::lower_program(&program);
        assert!(diags.is_empty(), "{diags:?}");
        let p = crate::ir::lowered::lower_program(&hir);
        let tables = TestTables::build(&program, fns);
        let f = p.functions.iter().find(|f| f.name == name).unwrap();
        tables.with_ctx(|ctx| lir_supported_function(f, ctx))
    }

    /// Like [`eligible`], but for forms the HIR may refuse to lower at all: a
    /// function with no lowered IR is by definition not claimed by the LIR path.
    fn eligible_lenient(src: &str, name: &str, fns: &[&str]) -> bool {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (hir, diags) = crate::ir::lower::lower_program(&program);
        if !diags.is_empty() {
            return false;
        }
        let p = crate::ir::lowered::lower_program(&hir);
        let tables = TestTables::build(&program, fns);
        match p.functions.iter().find(|f| f.name == name) {
            Some(f) => tables.with_ctx(|ctx| lir_supported_function(f, ctx)),
            None => false,
        }
    }

    fn returned_hir_expr(src: &str) -> HirExpr {
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (hir, diags) = crate::ir::lower::lower_program(&program);
        assert!(diags.is_empty(), "{diags:?}");
        let function = hir.functions.first().expect("one function");
        match function.body.last().expect("return statement") {
            HirStmt::Return {
                value: Some(value), ..
            } => value.clone(),
            other => panic!("expected value return, got {other:?}"),
        }
    }

    #[test]
    fn may_allocate_distinguishes_string_comparison_from_concatenation() {
        let eq = returned_hir_expr("fn f(a: String, b: String) -> bool { return a == b; }");
        let ne = returned_hir_expr("fn f(a: String, b: String) -> bool { return a != b; }");
        let concat = returned_hir_expr("fn f(a: String, b: String) -> String { return a + b; }");

        assert!(
            !may_allocate(&eq) && !may_allocate(&ne),
            "willow_string_eq is an allocation-free byte comparison"
        );
        assert!(
            may_allocate(&concat),
            "willow_string_concat allocates its result"
        );
    }

    // 1. a scalar arithmetic function is eligible
    #[test]
    fn e01_scalar_fn_eligible() {
        assert!(eligible(
            "fn add(a: i64, b: i64) -> i64 { return a + b; }",
            "add",
            &["add"]
        ));
    }

    // 2. recursive control flow (fib) is eligible
    #[test]
    fn e02_fib_eligible() {
        let src = "fn fib(n: i64) -> i64 { if n <= 1 { return n; } return fib(n-1) + fib(n-2); }";
        assert!(eligible(src, "fib", &["fib"]));
    }

    // 3. print of a scalar is eligible
    #[test]
    fn e03_scalar_print_eligible() {
        assert!(eligible(
            "fn show(n: i64) { println(n * 2); }",
            "show",
            &["show"]
        ));
    }

    // 4. (updated by willow-0g8j.1) string values became eligible with GC
    // rooting; kept as a positive check so a regression here is loud.
    #[test]
    fn e04_string_now_eligible() {
        assert!(eligible("fn s() { println(\"hi\"); }", "s", &["s"]));
    }

    // 5. (updated) short-circuit operators became eligible with lazy block
    // emission; kept as a positive check so a regression here is loud.
    #[test]
    fn e05_short_circuit_now_eligible() {
        assert!(eligible(
            "fn f(a: bool, b: bool) -> bool { return a && b; }",
            "f",
            &["f"]
        ));
    }

    // 6. unknown callees are not eligible
    #[test]
    fn e06_unknown_callee_ineligible() {
        assert!(!eligible(
            "fn g() -> i64 { return 1; } fn f() -> i64 { return g(); }",
            "f",
            &[] // g not in the known set
        ));
    }

    // 7. shadowing a let across sibling scopes is not eligible (flattened LIR)
    #[test]
    fn e07_shadowing_ineligible() {
        let src = "fn f(c: bool) -> i64 { let x = 1; if c { let x = 2; print(x); } return x; }";
        assert!(!eligible(src, "f", &["f"]));
    }

    // 8. while/for loops stay eligible (control flow is blocks, not exprs)
    #[test]
    fn e08_loops_eligible() {
        let src =
            "fn sum_to(n: i64) -> i64 { let mut t = 0; for i in 0..n { t = t + i; } return t; }";
        assert!(eligible(src, "sum_to", &["sum_to"]));
    }

    // 9. (updated by willow-0g8j.4) array-typed values are now eligible
    #[test]
    fn e09_arrays_now_eligible() {
        let src = "fn f() -> i64 { let xs = [1, 2]; return xs.len(); }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 10. f64 arithmetic + comparison is eligible
    #[test]
    fn e10_f64_eligible() {
        let src = "fn half(x: f64) -> bool { return x / 2.0 > 1.0; }";
        assert!(eligible(src, "half", &["half"]));
    }

    // 11. reference-mode parameters are rejected by the HIR itself — the
    // eligibility check never consults the AST declaration (willow-0g8j fix).
    #[test]
    fn e11_reference_params_ineligible_via_hir() {
        let src = "fn bump(n: &mut i64) { n = n + 1; }";
        assert!(!eligible(src, "bump", &["bump"]));
        let src2 = "fn read(n: &i64) -> i64 { return n; }";
        assert!(!eligible(src2, "read", &["read"]));
    }

    // 12. short-circuit && / || are now eligible (lazy block emission)
    #[test]
    fn e12_short_circuit_eligible() {
        assert!(eligible(
            "fn f(a: bool, b: bool) -> bool { return a && b || !a; }",
            "f",
            &["f"]
        ));
    }

    // 13. scalar ternaries are eligible
    #[test]
    fn e13_ternary_eligible() {
        assert!(eligible(
            "fn f(c: bool) -> i64 { return c ? 1 : 2; }",
            "f",
            &["f"]
        ));
    }

    // 14. (updated by willow-0g8j.1) a String ternary is now eligible
    #[test]
    fn e14_string_ternary_now_eligible() {
        let src = "fn f(c: bool) -> String { let s = c ? \"a\" : \"b\"; return s; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // ---------------------------------------------------------------------
    // willow-0g8j.1 — GC-managed values and rooting in the LIR walker.
    //
    // Perspectives 1-12 below are the *eligibility* half (which functions the
    // LIR path claims); perspectives 13-32 live in `tests/integration` as
    // differential and GC-stress runs, because they are about emitted code, not
    // about the predicate.
    //
    //  1. a String parameter/return function is eligible
    //  2. String concatenation is eligible
    //  3. String equality/inequality is eligible
    //  4. `println` of a String is eligible
    //  5. a String ternary is eligible
    //  6. mixed scalar + String locals in one function are eligible
    //  7. a String `let` that is reassigned in a loop is eligible
    //  8. calling a String-returning function is eligible
    //  9. a `let` shadowing a PARAMETER is rejected (flattened scopes)
    // 10. a bare enum variant `Var` is rejected (needs the AST special case)
    // 11. an unsupported String operator (`<`) is rejected
    // 12. arrays / class objects / interfaces still fall back
    // ---------------------------------------------------------------------

    // 15. String parameters and returns are eligible
    #[test]
    fn e15_string_param_and_return_eligible() {
        let src = "fn id(s: String) -> String { return s; }";
        assert!(eligible(src, "id", &["id"]));
    }

    // 16. concatenation of strings is eligible
    #[test]
    fn e16_string_concat_eligible() {
        let src = "fn join(a: String, b: String) -> String { return a + b; }";
        assert!(eligible(src, "join", &["join"]));
    }

    // 17. string equality and inequality are eligible
    #[test]
    fn e17_string_compare_eligible() {
        let eq = "fn f(a: String, b: String) -> bool { return a == b; }";
        assert!(eligible(eq, "f", &["f"]));
        let ne = "fn f(a: String, b: String) -> bool { return a != b; }";
        assert!(eligible(ne, "f", &["f"]));
    }

    // 18. a String local reassigned inside a loop is eligible — the case the
    // entry-rooted slot design exists for (a per-`let` root would grow the
    // shadow stack once per iteration).
    #[test]
    fn e18_string_loop_accumulator_eligible() {
        let src = "fn rep(n: i64) -> String { let mut s = \"\"; let mut i = 0; \
                   while i < n { s = s + \"x\"; i = i + 1; } return s; }";
        assert!(eligible(src, "rep", &["rep"]));
    }

    // 19. mixed scalar and String locals in one function are eligible
    #[test]
    fn e19_mixed_scalar_and_gc_eligible() {
        let src = "fn f(n: i64) -> String { let tag = \"n=\"; let doubled = n * 2; \
                   let ok = doubled > 0; return ok ? tag : \"\"; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 20. a call that both takes and returns a String is eligible
    #[test]
    fn e20_string_call_eligible() {
        let src = "fn wrap(s: String) -> String { return \"[\" + s + \"]\"; } \
                   fn f() -> String { return wrap(\"a\"); }";
        assert!(eligible(src, "f", &["f", "wrap"]));
    }

    // 21. a `let` shadowing a PARAMETER is rejected: LIR has no block scopes,
    // so one name cannot have two storages (willow-0g8j.1).
    #[test]
    fn e21_let_shadowing_param_ineligible() {
        let src = "fn f(s: String) -> String { let s = \"other\"; return s; }";
        assert!(!eligible(src, "f", &["f"]));
    }

    // 22. an enum value never reaches the LIR walker: the bare variant form
    // does not survive HIR lowering at all, and the qualified form is not a
    // supported expression. Both are checked so that a future lowering change
    // cannot quietly hand the walker a `Var` it would resolve to a local (the
    // `names` guard in `lir_supported_function` is the backstop).
    #[test]
    fn e22_enum_variant_never_reaches_walker() {
        let bare = "enum Status { Open, Closed } fn f() -> Status { return Closed; }";
        let tokens = Lexer::new(bare).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (_, diags) = crate::ir::lower::lower_program(&program);
        assert!(!diags.is_empty(), "bare variant unexpectedly lowered");

        let qualified = "enum Status { Open, Closed } fn f() -> Status { return Status::Closed; }";
        assert!(!eligible_lenient(qualified, "f", &["f"]));
    }

    // 23. an ordering operator on strings is not emitted, so it is rejected
    // even though both operand types are supported.
    #[test]
    fn e23_string_ordering_ineligible() {
        let src = "fn f(a: String, b: String) -> bool { return a < b; }";
        // The checker may reject this outright; if it lowers, we must not claim it.
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 24. (updated by willow-0g8j.4) arrays of strings are now eligible
    #[test]
    fn e24_string_array_now_eligible() {
        let src = "fn f() -> i64 { let xs = [\"a\", \"b\"]; return xs.len(); }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 25. (updated by willow-0g8j.5) a static call returning a class object is
    // now claimed by the LIR path
    #[test]
    fn e25_class_object_now_eligible() {
        let src = "class Item { name: String; pub static fn make(n: String) -> Item \
                   { return new Item(n); } } \
                   fn f() -> Item { return Item::make(\"a\"); }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // ---------------------------------------------------------------------
    // willow-0g8j.4 — `Array<T>` in the LIR walker.
    //
    // Perspectives 1-15 below are the *eligibility* half (which functions the
    // LIR path claims); perspectives 16-38 live in `tests/integration` as
    // differential and `WILLOW_GC_STRESS=alloc` runs, because they are about
    // emitted code and rooting, not about the predicate.
    //
    //  1. an array literal + `len()` is eligible
    //  2. array parameters and array returns are eligible
    //  3. indexing an array is eligible
    //  4. index-assignment (`a[i] = v`) is eligible
    //  5. `push` / `pop` are eligible
    //  6. `toString()` is eligible for every renderable element kind
    //  7. an array of `String` is eligible (GC element type)
    //  8. `Array<Array<i64>>` is eligible (element type checked recursively)
    //  9. an empty array literal never reaches the predicate: HIR lowering
    //     rejects it before the walker sees the function
    // 10. `for x in arr` is eligible (desugars to `len`/index)
    // 11. an array of class objects is rejected (no interface boxing here)
    // 12. an unsupported array method (`freeze`) is rejected
    // 13. a `FrozenArray<T>` receiver is rejected (different runtime call)
    // 14. a `Map<K, V>` receiver/index is rejected
    // 15. `toString()` on a non-renderable element type is rejected
    // ---------------------------------------------------------------------

    // 26. array parameters and array returns are eligible
    #[test]
    fn e26_array_param_and_return_eligible() {
        let src = "fn f(xs: Array<i64>) -> Array<i64> { return xs; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 27. reading through an index is eligible
    #[test]
    fn e27_array_index_eligible() {
        let src = "fn f(xs: Array<i64>, i: i64) -> i64 { return xs[i]; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 28. index-assignment is eligible (a LIR instruction, not an expression)
    #[test]
    fn e28_index_assign_eligible() {
        let src = "fn f() -> i64 { let mut xs = [1, 2]; xs[0] = 9; return xs[0]; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 29. push and pop are eligible
    #[test]
    fn e29_push_pop_eligible() {
        let src = "fn f() -> i64 { let mut xs = [1]; xs.push(2); return xs.pop(); }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 30. toString is eligible for each element kind the runtime can render
    #[test]
    fn e30_to_string_eligible_for_scalar_kinds() {
        for (decl, lit) in [
            ("i64", "[1, 2]"),
            ("f64", "[1.5]"),
            ("bool", "[true]"),
            ("String", "[\"a\"]"),
        ] {
            let src = format!(
                "fn f() -> String {{ let xs: Array<{decl}> = {lit}; return xs.toString(); }}"
            );
            assert!(eligible(&src, "f", &["f"]), "{decl} array toString");
        }
    }

    // 31. an `Array<Array<i64>>` is eligible: the element type is itself checked
    #[test]
    fn e31_nested_array_eligible() {
        let src = "fn f() -> i64 { let xs = [[1, 2], [3]]; return xs[0][1]; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 32. an empty literal never reaches the walker: the HIR lowering refuses
    // it (there is no first element to take the type from), so the function has
    // no LIR at all and stays on the AST path.
    #[test]
    fn e32_empty_array_never_reaches_walker() {
        let src = "fn f() -> i64 { let mut xs: Array<i64> = []; xs.push(1); return xs.len(); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 33. `for x in arr` is eligible: the LIR desugars it into `len()` + index
    #[test]
    fn e33_for_over_array_eligible() {
        let src =
            "fn f(xs: Array<i64>) -> i64 { let mut t = 0; for x in xs { t = t + x; } return t; }";
        assert!(eligible(src, "f", &["f"]));
    }

    // 34. arrays of SIMPLE class objects are eligible since willow-0g8j.5 (the
    // element is a plain GC handle); an array of an INTERFACE element type
    // still falls back, because storing into it needs the boxing coercion only
    // the AST path applies.
    #[test]
    fn e34_class_element_array_eligible_interface_element_not() {
        let src = "class Item { pub name: String; } \
                   fn f(xs: Array<Item>) -> i64 { return xs.len(); }";
        assert!(eligible_lenient(src, "f", &["f"]));

        let iface = "interface Named { fn name(self) -> String; } \
                     fn f(xs: Array<Named>) -> i64 { return xs.len(); }";
        assert!(!eligible_lenient(iface, "f", &["f"]));
    }

    // 35. an array method the walker does not emit falls back
    #[test]
    fn e35_unsupported_array_method_ineligible() {
        let src = "fn f() -> i64 { let xs = [1, 2]; let ys = xs.freeze(); return ys.len(); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 36. a `Map<K, V>` receiver is not an array receiver
    #[test]
    fn e36_map_ineligible() {
        let src = "fn f() -> i64 { let m: Map<String, i64> = Map::new(); \
                   m.insert(\"a\", 1); return m.len(); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 37. `toString()` on an element type the runtime cannot render falls back
    #[test]
    fn e37_nested_array_to_string_ineligible() {
        let src = "fn f() -> String { let xs = [[1], [2]]; return xs.toString(); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // ---------------------------------------------------------------------
    // willow-0g8j.5 — class objects and field access in the LIR walker.
    //
    // A "simple" class is the subset the walker claims: no base class, not
    // itself a base, not an interface or enum, a known field layout, and every
    // field type supported. Anything else — inheritance (virtual dispatch),
    // interfaces (boxing), enums (payload layout) — stays on the AST path.
    //
    // Perspectives 1-26 below are the *eligibility* half; perspectives 27-46
    // live in `tests/integration/codegen.rs` as differential and
    // `WILLOW_GC_STRESS=alloc` runs, because they are about the emitted code
    // and its GC rooting, not about the predicate.
    //
    //  1. a class-typed parameter and return is eligible
    //  2. `new C(..)` through the implicit memberwise constructor is eligible
    //  3. `new C(..)` through an explicit `init` is eligible
    //  4. an object literal `C { f: v }` is eligible
    //  5. an object literal missing a declared field is not claimed
    //  6. reading a field is eligible
    //  7. assigning a field is eligible
    //  8. a chained field read (`a.b.c`) is eligible
    //  9. an instance method call is eligible
    // 10. a static method call is eligible
    // 11. a GC-managed (`String`) field is eligible
    // 12. an `Array<T>` field is eligible
    // 13. a class local declared before a `while` keeps its entry root slot
    // 14. a subclass (`extends`) is rejected — virtual dispatch
    // 15. a base class (something extends it) is rejected — callers may be
    //     holding a subclass instance whose layout differs
    // 16. an interface-typed field is rejected — the store needs boxing
    // 17. an enum-typed field is rejected
    // 18. an interface-typed parameter is rejected
    // 19. `let x: Iface = new C();` is rejected: the BINDING type widens, so
    //     the value would need a coercion the walker does not emit
    // 20. a method with a `&mut` parameter is rejected (mode check)
    // 21. a method whose return type is an interface is rejected
    // 22. a self-referential field type is eligible — the support check is
    //     cycle-safe and must not recurse forever
    // 23. an array of simple class objects with a field read is eligible
    // 24. a NULLABLE class type (`Node?`) is rejected everywhere it appears —
    //     as a field, a parameter or a local — because `nil` comparison and
    //     nil-guarded access are not part of the walker yet
    // 25. a base class reached under an IMPORT ALIAS is still rejected: class
    //     identity is the runtime `type_id`, not the name
    // 26. an object literal naming the same field twice is rejected, not just
    //     one with the wrong field COUNT
    // ---------------------------------------------------------------------

    /// A minimal simple class, reused by the perspectives below.
    const POINT: &str = "class Point { pub x: i64; pub y: i64; } ";

    // 38. a class-typed parameter and a class-typed return are eligible
    #[test]
    fn e38_class_param_and_return_eligible() {
        let src = format!("{POINT} fn f(p: Point) -> Point {{ return p; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // 39. `new` through the implicit memberwise constructor is eligible
    #[test]
    fn e39_new_memberwise_eligible() {
        let src = format!("{POINT} fn f() -> i64 {{ let p = new Point(1, 2); return p.x + p.y; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // 40. `new` through an explicit `init` constructor is eligible
    #[test]
    fn e40_new_explicit_init_eligible() {
        let src = "class Counter { pub n: i64; \
                   pub init(self, n: i64) { self.n = n; } } \
                   fn f() -> i64 { let c = new Counter(7); return c.n; }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 41. an object literal is eligible: it lowers to the same field stores.
    //
    // The type checker rejects `C { f: v }` in source today (check_ops.rs:
    // "named field syntax is part of the old construction form"), so this and
    // the next perspective exercise the walker's handling directly from HIR —
    // the eligibility predicate must stay consistent with the emitter for the
    // node it can still be handed.
    #[test]
    fn e41_object_literal_eligible() {
        let src =
            format!("{POINT} fn f() -> i64 {{ let p = Point {{ x: 1, y: 2 }}; return p.y; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // 42. an object literal that omits a declared field is not claimed: the
    // walker only emits a complete memberwise initialisation.
    #[test]
    fn e42_partial_object_literal_ineligible() {
        let src = format!("{POINT} fn f() -> i64 {{ let p = Point {{ x: 1 }}; return p.x; }}");
        assert!(!eligible_lenient(&src, "f", &["f"]));
    }

    // 43. reading a field is eligible
    #[test]
    fn e43_field_read_eligible() {
        let src = format!("{POINT} fn f(p: Point) -> i64 {{ return p.x; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // 44. assigning a field is eligible (a LIR instruction, not an expression)
    #[test]
    fn e44_field_assign_eligible() {
        let src =
            format!("{POINT} fn f() -> i64 {{ let p = new Point(1, 2); p.x = 9; return p.x; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // 45. a chained field read walks two objects, each with its own nil check
    #[test]
    fn e45_nested_field_read_eligible() {
        let src = "class Inner { pub v: i64; } class Outer { pub inner: Inner; } \
                   fn f(o: Outer) -> i64 { return o.inner.v; }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 46. an instance method call is eligible: a direct call to `Class__method`
    #[test]
    fn e46_instance_method_call_eligible() {
        let src = "class Counter { pub n: i64; \
                   pub fn get(self) -> i64 { return self.n; } } \
                   fn f(c: Counter) -> i64 { return c.get(); }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 47. a static method call is eligible (a null receiver is passed)
    #[test]
    fn e47_static_method_call_eligible() {
        let src = "class Counter { pub n: i64; \
                   pub static fn zero() -> i64 { return 0; } } \
                   fn f() -> i64 { return Counter::zero(); }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 48. a GC-managed field type is eligible: the store goes through the
    // object-field write path, not a plain store
    #[test]
    fn e48_string_field_eligible() {
        let src = "class Item { pub name: String; } \
                   fn f() -> String { let i = new Item(\"a\"); i.name = \"b\"; return i.name; }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 49. an `Array<T>` field is eligible: the element type is checked too
    #[test]
    fn e49_array_field_eligible() {
        let src = "class Bag { pub xs: Array<i64>; } \
                   fn f(b: Bag) -> i64 { return b.xs.len(); }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 50. a class local that lives across a loop is eligible: its root slot is
    // allocated once at entry, so the shadow stack does not grow per iteration
    #[test]
    fn e50_class_local_across_loop_eligible() {
        let src = format!(
            "{POINT} fn f() -> i64 {{ let p = new Point(0, 0); let mut i = 0; \
             while i < 3 {{ p.x = p.x + i; i = i + 1; }} return p.x; }}"
        );
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // 51. a subclass is rejected: its methods dispatch virtually
    #[test]
    fn e51_subclass_ineligible() {
        let src = "pub open class Animal { pub age: i64; } \
                   pub class Dog extends Animal { } \
                   fn f(d: Dog) -> i64 { return d.age; }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 52. a base class is rejected too: a caller may hand it a subclass
    // instance, whose field layout the walker never sees
    #[test]
    fn e52_base_class_ineligible() {
        let src = "pub open class Animal { pub age: i64; } \
                   pub class Dog extends Animal { } \
                   fn f(a: Animal) -> i64 { return a.age; }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 53. an interface-typed field is rejected: storing into it needs boxing
    #[test]
    fn e53_interface_field_ineligible() {
        let src = "interface Named { fn name(self) -> String; } \
                   class Holder { pub n: Named; } \
                   fn f(h: Holder) -> i64 { return 1; }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 54. an enum-typed field is rejected: enum payload layout is not handled
    #[test]
    fn e54_enum_field_ineligible() {
        let src = "enum Color { Red, Green } \
                   class Holder { pub c: Color; } \
                   fn f(h: Holder) -> i64 { return 1; }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 55. an interface-typed parameter is rejected
    #[test]
    fn e55_interface_param_ineligible() {
        let src = "interface Named { fn name(self) -> String; } \
                   fn f(n: Named) -> String { return n.name(); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 56. a widening `let` annotation is rejected: the BINDING type is the
    // interface, so the class value would need the boxing coercion the walker
    // does not emit. This is exactly the case that makes `HirStmt::Let::ty`
    // rather than `value.ty` the type the walker must trust.
    #[test]
    fn e56_widening_let_annotation_ineligible() {
        let src = "interface Named { fn name(self) -> String; } \
                   class Item implements Named { pub n: String; \
                   pub fn name(self) -> String { return self.n; } } \
                   fn f() -> String { let x: Named = new Item(\"a\"); return x.name(); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 57. a method with a by-reference parameter is rejected: the walker only
    // passes values, so it must never claim a callee expecting an address
    #[test]
    fn e57_reference_param_method_ineligible() {
        let src = "class Counter { pub n: i64; \
                   pub fn bump(self, v: &mut i64) { v = v + 1; } } \
                   fn f(c: Counter) -> i64 { let mut k = 1; c.bump(&k); return k; }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 58. a method returning an interface is rejected
    #[test]
    fn e58_interface_returning_method_ineligible() {
        let src = "interface Named { fn name(self) -> String; } \
                   class Item implements Named { pub n: String; \
                   pub fn name(self) -> String { return self.n; } \
                   pub fn as_named(self) -> Named { return self; } } \
                   fn f(i: Item) -> String { return i.as_named().name(); }";
        assert!(!eligible_lenient(src, "f", &["f"]));
    }

    // 59. a self-referential field type must not send the support check into
    // infinite recursion; a linked node is a perfectly ordinary GC handle
    #[test]
    fn e59_self_referential_class_eligible() {
        let src = "class Node { pub v: i64; pub next: Node; } \
                   fn f(n: Node) -> i64 { return n.v; }";
        assert!(eligible_lenient(src, "f", &["f"]));
    }

    // 60. an array of simple class objects, indexed and field-read, is eligible
    #[test]
    fn e60_class_array_field_read_eligible() {
        let src = format!("{POINT} fn f(ps: Array<Point>) -> i64 {{ return ps[0].x; }}");
        assert!(eligible_lenient(&src, "f", &["f"]));
    }

    // 61. nullable class types are rejected: a `Node?` needs the nil-comparison
    // and nil-guard handling the walker does not have. That also keeps the
    // linked-list shape (`pub next: Node?`) on the AST path, which is why a
    // self-referential NON-nullable field (perspective 22) is the eligible one.
    #[test]
    fn e61_nullable_class_ineligible() {
        let field = "class Node { pub v: i64; pub next: Node?; } \
                     fn f(n: Node) -> i64 { return n.v; }";
        assert!(!eligible_lenient(field, "f", &["f"]));

        let param = format!("{POINT} fn f(p: Point?) -> i64 {{ return 1; }}");
        assert!(!eligible_lenient(&param, "f", &["f"]));
    }
    // 62. a class reached through a DIRECT TYPE IMPORT is the same class as its
    // module-qualified self, so an imported base class must still be rejected.
    // `import zoo::Animal;` registers the class a second time under `Animal`,
    // sharing the canonical `type_id`, while `class_base` keeps canonical names
    // on both sides (`zoo::Dog` -> `zoo::Animal`). Comparing names alone would
    // call `Animal` a leaf and emit a direct `Animal__speak` for a receiver that
    // is really a `Dog`.
    #[test]
    fn e62_imported_base_class_alias_ineligible() {
        let src = "class Animal { pub value: i64; } fn f(a: Animal) -> i64 { return a.value; }";
        let tokens = Lexer::new(src).tokenize().expect("lex");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "{errs:?}");
        let (hir, diags) = crate::ir::lower::lower_program(&program);
        assert!(diags.is_empty(), "{diags:?}");
        let p = crate::ir::lowered::lower_program(&hir);
        let f = p.functions.iter().find(|f| f.name == "f").unwrap();

        let mut tables = TestTables::build(&program, &["f"]);
        // Without the module in the picture, `Animal` is an ordinary leaf.
        assert!(tables.with_ctx(|ctx| lir_supported_function(f, ctx)));

        // Now model the import: `Animal` is an alias of `zoo::Animal`, which
        // `zoo::Dog` extends. Only the canonical names appear in `class_base`.
        let animal_id = tables.class_type_ids["Animal"];
        tables
            .class_type_ids
            .insert("zoo::Animal".to_string(), animal_id);
        tables
            .class_type_ids
            .insert("zoo::Dog".to_string(), animal_id + 100);
        tables
            .class_base
            .insert("zoo::Dog".to_string(), "zoo::Animal".to_string());
        assert!(
            !tables.with_ctx(|ctx| lir_supported_function(f, ctx)),
            "an imported base class must not be treated as a leaf"
        );
    }

    // 63. an object literal that names the same field twice is rejected even
    // though the COUNT matches the layout: the emitter would store into that
    // field twice and leave the other one at its zero value.
    #[test]
    fn e63_object_literal_duplicate_field_ineligible() {
        let src =
            format!("{POINT} fn f() -> i64 {{ let p = Point {{ x: 1, x: 2 }}; return p.x; }}");
        assert!(!eligible_lenient(&src, "f", &["f"]));
    }
}
