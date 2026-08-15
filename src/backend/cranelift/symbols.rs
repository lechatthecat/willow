//! Symbol-naming and module-qualification helpers for the Cranelift backend
//! (extracted from `mod.rs`). Map Willow names to mangled backend symbols and
//! qualify module-local types/classes; constructor->method desugaring lives here
//! too as a naming/shape transform.
//!
//! # The mangling scheme (willow-uqzx, catalog item 8 phase 2)
//!
//! Every backend symbol is built here and nowhere else. The scheme is
//! **injective**: two different declarations can never produce one symbol.
//!
//! It gets that from two separator characters that a Willow identifier cannot
//! contain (identifiers are `[A-Za-z_][A-Za-z0-9_]*`):
//!
//! ```text
//! .   joins user-written name components   shapes::Square::area -> shapes.Square.area
//! $   introduces a compiler-generated role  Config::size (static) -> Config.size$static
//! ```
//!
//! The scheme it replaced was `name.replace("::", "__")`, which is not
//! injective, because nothing stops a Willow name from containing `__`:
//! `shapes::Square::area` and a plain function named `shapes__Square__area`
//! both mangled to `shapes__Square__area`. Phase 1 turned that from a silent
//! miscompile into an `E0706` diagnostic; separators the source language cannot
//! spell remove the collision instead of reporting it, so both declarations now
//! compile and keep their own code.
//!
//! Separator choice is not exotic. Every Willow binary already links the Rust
//! runtime staticlib, whose legacy-mangled symbols carry both `$` and `.` on
//! Linux, macOS and Windows alike, and Go emits `pkg.Func` on the same three
//! object formats. Willow's own symbols are `Linkage::Local` apart from
//! `willow_user_main`, so they never even reach cross-object resolution.
//!
//! Decoding is the inverse: split off a trailing `$role`, then split on `.`.
//! No component can contain either separator, so nothing is ambiguous.

use std::collections::HashMap;

use crate::parser::ast::*;

use super::USER_MAIN_SYMBOL;

/// Joins user-written name components inside a symbol (module segments, class
/// names, method and field names). Cannot appear in a Willow identifier.
pub(crate) const PATH_SEP: char = '.';

/// Introduces a compiler-generated role suffix (`$static`, `$vtable`, `$poll`).
/// Cannot appear in a Willow identifier, so a role can never be mistaken for a
/// user name component.
pub(crate) const ROLE_SEP: char = '$';

/// True if `symbol` was built by joining components, i.e. it carries at least
/// one separator the source language cannot spell.
///
/// A symbol without one is a bare identifier: an entry-file free function, and
/// the only shape that can still land on a runtime name.
pub(crate) fn is_mangled_symbol(symbol: &str) -> bool {
    symbol.contains(PATH_SEP) || symbol.contains(ROLE_SEP)
}

/// Join already-mangled path components with [`PATH_SEP`].
///
/// This is the one join in the backend, and injectivity rests on it: no
/// component can contain the separator, so distinct component lists cannot
/// meet at one string.
pub(crate) fn symbol_path(components: &[&str]) -> String {
    components.join(&PATH_SEP.to_string())
}

/// Append a compiler-generated role to a symbol.
fn with_role(base: &str, role: &str) -> String {
    format!("{base}{ROLE_SEP}{role}")
}

/// The symbol path for a `::`-qualified module path (`a::b` -> `a.b`).
pub(crate) fn module_symbol_prefix(module_path: &str) -> String {
    symbol_path(&module_path.split("::").collect::<Vec<_>>())
}

/// Make a `::`-qualified name safe for use inside a linker symbol.
pub(crate) fn backend_symbol_component(name: &str) -> String {
    module_symbol_prefix(name)
}

/// `{module_prefix}.{item}` — a module-level free function or an item aliased
/// under a module prefix. `module_prefix` is already a symbol path.
pub(crate) fn module_item_symbol(module_prefix: &str, item: &str) -> String {
    symbol_path(&[module_prefix, item])
}

/// `{class_symbol}.{method}` — a class method whose class is already a symbol
/// path. Use [`class_method_symbol_name`] when the class is a `::`-qualified
/// Willow name that may need module-prefix resolution.
///
/// Deliberately the same join as [`module_item_symbol`]: a module prefix and a
/// class name share one path namespace, exactly as `pkg.Func` and
/// `Type.Method` do in Go. Name resolution already refuses to let an import and
/// a local declaration share a name (`E2003`), so the two cannot meet.
pub(crate) fn class_member_symbol(class_symbol: &str, member: &str) -> String {
    symbol_path(&[class_symbol, member])
}

/// The prefix every method symbol of `class_symbol` starts with, used to
/// re-alias a module's methods under an imported local class name.
pub(crate) fn class_member_prefix(class_symbol: &str) -> String {
    format!("{class_symbol}{PATH_SEP}")
}

/// `{class}.{field}$static` — global storage for a static property.
///
/// The `$static` role is what keeps it apart from a method of the same name;
/// the field name is always a single identifier, so it is always the last path
/// component.
pub(crate) fn static_property_symbol(class_name: &str, field: &str) -> String {
    with_role(
        &class_member_symbol(&backend_symbol_component(class_name), field),
        "static",
    )
}

/// `{class}$as${iface}$vtable` — the (class, interface) dispatch table.
///
/// `$as$` rather than `.` between the two names: both sides are `::`-qualified
/// paths, so joining them with `.` would let (`a::b`, `c`) and (`a`, `b::c`)
/// meet at one symbol.
pub(crate) fn vtable_symbol(class_name: &str, iface_name: &str) -> String {
    let pair = with_role(
        &with_role(&backend_symbol_component(class_name), "as"),
        &backend_symbol_component(iface_name),
    );
    with_role(&pair, "vtable")
}

/// `{fn}$poll` — the state-machine body of `async fn main`.
pub(crate) fn poll_symbol(function_symbol: &str) -> String {
    with_role(function_symbol, "poll")
}

/// `{fn}$coop_poll` — the state-machine body of a cooperative async function
/// or method.
pub(crate) fn coop_poll_symbol(function_symbol: &str) -> String {
    with_role(function_symbol, "coop_poll")
}

/// `{fn}$coop_cancel` — the cancellation entry point paired with a
/// `$coop_poll` body.
pub(crate) fn coop_cancel_symbol(function_symbol: &str) -> String {
    with_role(function_symbol, "coop_cancel")
}

/// `$lambda.{n}` — the lifted top-level function for the `n`th lambda.
///
/// The `$` role marker is what makes it unspellable in source, so a lambda can
/// never collide with a user declaration however it is named.
pub(crate) fn lambda_symbol(index: usize) -> String {
    symbol_path(&[&format!("{ROLE_SEP}lambda"), &index.to_string()])
}

/// Synthesize the `init` method that a constructor lowers to (willow-scq2): a
/// non-static instance method with a hidden `self` receiver and void return.
pub(crate) fn constructor_to_method(ctor: &ConstructorDecl) -> MethodDecl {
    MethodDecl {
        name: "init".to_string(),
        public: ctor.public,
        protected: ctor.protected,
        is_async: false,
        is_open: false,
        is_override: false,
        is_static: false,
        params: ctor.params.clone(),
        has_self: true,
        return_type: Type::Void,
        body: ctor.body.clone(),
        span: ctor.span,
        is_default_injected: false,
    }
}

pub(crate) fn class_method_symbol_name(
    known_modules: &HashMap<String, String>,
    class_name: &str,
    method_name: &str,
) -> String {
    let module_match = known_modules
        .iter()
        .filter_map(|(access_name, symbol_prefix)| {
            class_name
                .strip_prefix(access_name)
                .and_then(|rest| rest.strip_prefix("::"))
                .map(|suffix| (access_name.len(), symbol_prefix, suffix))
        })
        .max_by_key(|(len, _, _)| *len);

    if let Some((_, symbol_prefix, class_suffix)) = module_match {
        let class_suffix = module_symbol_prefix(class_suffix);
        class_member_symbol(
            &module_item_symbol(symbol_prefix, &class_suffix),
            method_name,
        )
    } else {
        class_member_symbol(&backend_symbol_component(class_name), method_name)
    }
}

pub(crate) fn qualify_module_class_decl(class: &ClassDecl, module_name: &str) -> ClassDecl {
    let mut qualified = class.clone();
    qualified.name = format!("{module_name}::{}", class.name);
    qualified.implements = class
        .implements
        .iter()
        .map(|iface| qualify_module_type(iface, module_name))
        .collect();
    qualified.fields = class
        .fields
        .iter()
        .map(|field| {
            let mut field = field.clone();
            field.ty = qualify_module_type(&field.ty, module_name);
            field
        })
        .collect();
    qualified.methods = class
        .methods
        .iter()
        .map(|method| {
            let mut method = method.clone();
            method.params = method
                .params
                .iter()
                .map(|param| {
                    let mut param = param.clone();
                    param.ty = qualify_module_type(&param.ty, module_name);
                    param
                })
                .collect();
            method.return_type = qualify_module_type(&method.return_type, module_name);
            method
        })
        .collect();
    qualified.constructors = class
        .constructors
        .iter()
        .map(|ctor| {
            let mut ctor = ctor.clone();
            ctor.params = ctor
                .params
                .iter()
                .map(|param| {
                    let mut param = param.clone();
                    param.ty = qualify_module_type(&param.ty, module_name);
                    param
                })
                .collect();
            ctor
        })
        .collect();
    qualified
}

pub(crate) fn qualify_module_type(ty: &Type, module_name: &str) -> Type {
    match ty {
        Type::Named(name) if !name.contains("::") => Type::Named(format!("{module_name}::{name}")),
        Type::Array(element) => Type::Array(Box::new(qualify_module_type(element, module_name))),
        Type::Generic(name, args) => Type::Generic(
            name.clone(),
            args.iter()
                .map(|arg| qualify_module_type(arg, module_name))
                .collect(),
        ),
        Type::Fn(params, ret) => Type::Fn(
            params
                .iter()
                .map(|param| qualify_module_type(param, module_name))
                .collect(),
            Box::new(qualify_module_type(ret, module_name)),
        ),
        _ => ty.clone(),
    }
}

/// Qualify a type's module-LOCAL declared type names (in `local`) to
/// `module::Type`, including a GENERIC head name (`Box<i64>` ->
/// `module::Box<i64>`), while leaving builtin generics (Array/Map/Result/...)
/// untouched. Used to qualify a module function's signature so the importing
/// file boxes interface arguments against the right (class, interface) vtable
/// (willow-1js.5).
pub(crate) fn qualify_module_local_type(
    ty: &Type,
    module_name: &str,
    local: &std::collections::HashSet<String>,
) -> Type {
    let qual = |n: &str| -> String {
        if !n.contains("::") && local.contains(n) {
            format!("{module_name}::{n}")
        } else {
            n.to_string()
        }
    };
    match ty {
        Type::Named(name) => Type::Named(qual(name)),
        Type::Generic(name, args) => Type::Generic(
            qual(name),
            args.iter()
                .map(|a| qualify_module_local_type(a, module_name, local))
                .collect(),
        ),
        Type::Array(e) => Type::Array(Box::new(qualify_module_local_type(e, module_name, local))),
        Type::Fn(ps, r) => Type::Fn(
            ps.iter()
                .map(|p| qualify_module_local_type(p, module_name, local))
                .collect(),
            Box::new(qualify_module_local_type(r, module_name, local)),
        ),
        _ => ty.clone(),
    }
}

/// Clone a module function declaration with its SIGNATURE (parameter and return
/// types) qualified to module-local names. The body is left untouched (it is
/// compiled under the module's local-name aliases) (willow-1js.5).
pub(crate) fn qualify_module_fn_signature(
    f: &FunctionDecl,
    module_name: &str,
    local: &std::collections::HashSet<String>,
) -> FunctionDecl {
    let mut out = f.clone();
    for p in &mut out.params {
        p.ty = qualify_module_local_type(&p.ty, module_name, local);
    }
    out.return_type = qualify_module_local_type(&out.return_type, module_name, local);
    out
}

pub(crate) fn class_name_for_object_type(ty: &Type) -> Option<String> {
    match ty {
        Type::Named(name) => Some(name.clone()),
        _ => None,
    }
}

pub(crate) fn user_function_symbol(name: &str) -> String {
    if name == "main" {
        USER_MAIN_SYMBOL.to_string()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod mangling_tests {
    //! Perspectives 33-50 for the injective mangling scheme (willow-uqzx,
    //! catalog item 8 phase 2).
    //!
    //! Phase 1 could only *report* symbol collisions, because
    //! `name.replace("::", "__")` genuinely mapped two declarations onto one
    //! name. These tests pin down the property that replaced the report: a
    //! symbol is a list of components joined by characters no Willow
    //! identifier can contain, so distinct declarations cannot meet.
    //!
    //! Perspective 46 is the load-bearing one — the rest describe the shapes,
    //! it describes the guarantee.

    use super::*;
    use std::collections::{HashMap, HashSet};

    /// Component names a real program can produce, chosen so that every one of
    /// them would have been ambiguous under the old `__` scheme.
    const NASTY: &[&str] = &["a", "b", "a_b", "a__b", "__", "a__b__c", "_", "b__c"];

    fn modules(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(access, prefix)| (access.to_string(), prefix.to_string()))
            .collect()
    }

    /// Perspective 33: a `::` module path becomes a `.` symbol path.
    #[test]
    fn unit_mangle_33_module_path_segments_join_with_a_dot() {
        assert_eq!(module_symbol_prefix("math"), "math");
        assert_eq!(module_symbol_prefix("a::b"), "a.b");
        assert_eq!(module_symbol_prefix("a::b::c"), "a.b.c");
    }

    /// Perspective 34: a single-segment path is unchanged — nothing is
    /// appended to a name that has only one component.
    #[test]
    fn unit_mangle_34_single_segment_path_is_untouched() {
        assert_eq!(module_symbol_prefix("string_tools"), "string_tools");
        assert!(!is_mangled_symbol(&module_symbol_prefix("string_tools")));
    }

    /// Perspective 35: the conversion is total. Every `::` in a qualified name
    /// becomes a separator, including in deeply nested paths.
    #[test]
    fn unit_mangle_35_every_qualifier_is_converted() {
        assert_eq!(backend_symbol_component("a::b::c::D"), "a.b.c.D");
        assert!(!backend_symbol_component("a::b::c::D").contains("::"));
    }

    /// Perspective 36: a plain class method is `Class.method`.
    #[test]
    fn unit_mangle_36_plain_class_method_symbol() {
        let known = modules(&[]);
        assert_eq!(
            class_method_symbol_name(&known, "Point", "area"),
            "Point.area"
        );
    }

    /// Perspective 37: a class inside a known module carries the module prefix
    /// as leading path components.
    #[test]
    fn unit_mangle_37_module_class_method_symbol() {
        let known = modules(&[("shapes", "shapes")]);
        assert_eq!(
            class_method_symbol_name(&known, "shapes::Square", "area"),
            "shapes.Square.area"
        );
    }

    /// Perspective 38: when two module prefixes both match, the longest wins,
    /// so a nested module does not get truncated to its parent.
    #[test]
    fn unit_mangle_38_longest_module_prefix_wins() {
        let known = modules(&[("a", "a"), ("a::b", "a.b")]);
        assert_eq!(
            class_method_symbol_name(&known, "a::b::C", "m"),
            "a.b.C.m",
            "the `a::b` prefix must beat the `a` prefix"
        );
    }

    /// Perspective 39: an aliased import resolves to the module's canonical
    /// symbol prefix, not to the name the importing file used.
    #[test]
    fn unit_mangle_39_alias_uses_the_canonical_prefix() {
        let known = modules(&[("m", "math")]);
        assert_eq!(
            class_method_symbol_name(&known, "m::Vec2", "len"),
            "math.Vec2.len"
        );
    }

    /// Perspective 40: a static property's storage carries the `$static` role,
    /// and the field name stays the last path component.
    #[test]
    fn unit_mangle_40_static_property_carries_its_role() {
        assert_eq!(
            static_property_symbol("Config", "size"),
            "Config.size$static"
        );
        assert_eq!(
            static_property_symbol("shapes::Config", "size"),
            "shapes.Config.size$static"
        );
    }

    /// Perspective 41: a static property and a method of the same name on the
    /// same class are different symbols. The role is what separates them.
    #[test]
    fn unit_mangle_41_static_property_and_method_differ() {
        let known = modules(&[]);
        assert_ne!(
            static_property_symbol("Config", "size"),
            class_method_symbol_name(&known, "Config", "size")
        );
    }

    /// Perspective 42: a vtable names both sides plus its role.
    #[test]
    fn unit_mangle_42_vtable_symbol_shape() {
        assert_eq!(vtable_symbol("Dog", "Animal"), "Dog$as$Animal$vtable");
    }

    /// Perspective 43: the two names in a vtable symbol are joined by a role
    /// marker, not by a path separator, so a qualified class and a qualified
    /// interface cannot trade a component between them.
    #[test]
    fn unit_mangle_43_vtable_pair_is_unambiguous() {
        assert_ne!(vtable_symbol("a::b", "c"), vtable_symbol("a", "b::c"));
    }

    /// Perspective 44: the three async roles derived from one function are
    /// distinct from each other and from the function itself.
    #[test]
    fn unit_mangle_44_async_roles_are_distinct() {
        let base = "math.compute";
        let derived = [
            poll_symbol(base),
            coop_poll_symbol(base),
            coop_cancel_symbol(base),
            base.to_string(),
        ];
        assert_eq!(
            derived.iter().collect::<HashSet<_>>().len(),
            derived.len(),
            "{derived:?}"
        );
        assert_eq!(poll_symbol(base), "math.compute$poll");
    }

    /// Perspective 45: lambda symbols are unique per index and unspellable in
    /// source, so a lifted lambda can never take a user declaration's symbol.
    #[test]
    fn unit_mangle_45_lambda_symbols_are_unspellable_and_unique() {
        assert_eq!(lambda_symbol(0), "$lambda.0");
        assert_ne!(lambda_symbol(0), lambda_symbol(1));
        assert!(lambda_symbol(7).contains(ROLE_SEP));
    }

    /// Perspective 46: the guarantee itself. Over an alphabet of component
    /// names built from the underscores that used to be ambiguous, every
    /// distinct component list produces a distinct symbol.
    ///
    /// Under the old scheme this same sweep collided: `["a", "b__c"]` and
    /// `["a__b", "c"]` both mangled to `a__b__c`.
    #[test]
    fn unit_mangle_46_distinct_component_lists_give_distinct_symbols() {
        let mut seen: HashMap<String, Vec<&str>> = HashMap::new();
        let mut lists: Vec<Vec<&str>> = Vec::new();
        for a in NASTY {
            lists.push(vec![*a]);
            for b in NASTY {
                lists.push(vec![*a, *b]);
                for c in NASTY {
                    lists.push(vec![*a, *b, *c]);
                }
            }
        }
        for list in lists {
            let symbol = symbol_path(&list);
            if let Some(previous) = seen.insert(symbol.clone(), list.clone()) {
                panic!("`{symbol}` is produced by both {previous:?} and {list:?}");
            }
        }
    }

    /// Perspective 47: a component that contains `__` stays one component. The
    /// scheme never reads user underscores as structure.
    #[test]
    fn unit_mangle_47_underscores_inside_a_component_are_not_structure() {
        let joined = symbol_path(&["a__b", "c"]);
        assert_eq!(joined, "a__b.c");
        assert_eq!(joined.split(PATH_SEP).collect::<Vec<_>>(), ["a__b", "c"]);
    }

    /// Perspective 48: every constructed symbol is recognizable as mangled,
    /// and a bare identifier is not. This is the predicate the reserved-name
    /// check narrows on, so a false negative here would re-open the runtime
    /// namespace to mangled symbols.
    #[test]
    fn unit_mangle_48_constructed_symbols_are_recognizable() {
        let known = modules(&[("shapes", "shapes")]);
        for symbol in [
            module_item_symbol("math", "add"),
            class_member_symbol("Point", "area"),
            class_method_symbol_name(&known, "shapes::Square", "area"),
            static_property_symbol("Config", "size"),
            vtable_symbol("Dog", "Animal"),
            poll_symbol("willow_user_main"),
            coop_poll_symbol("worker"),
            coop_cancel_symbol("worker"),
            lambda_symbol(3),
        ] {
            assert!(
                is_mangled_symbol(&symbol),
                "`{symbol}` must read as mangled"
            );
        }
        for symbol in ["main", "add", "willow_array_new", "my_helper", ""] {
            assert!(!is_mangled_symbol(symbol), "`{symbol}` must read as bare");
        }
    }

    /// Perspective 49: `fn main` is the one user function the compiler renames,
    /// and it renames nothing else.
    #[test]
    fn unit_mangle_49_only_main_is_renamed() {
        assert_eq!(user_function_symbol("main"), USER_MAIN_SYMBOL);
        assert_eq!(user_function_symbol("main_loop"), "main_loop");
        assert_eq!(user_function_symbol("Main"), "Main");
    }

    /// Perspective 50: a symbol decodes back to its components — split off the
    /// trailing role, then split on the path separator. Debug tooling and the
    /// module-import aliasing both rely on this being unambiguous.
    #[test]
    fn unit_mangle_50_symbols_decode_back_to_components() {
        let symbol = static_property_symbol("shapes::Config", "size");
        let (path, role) = symbol.rsplit_once(ROLE_SEP).expect("static carries a role");
        assert_eq!(role, "static");
        assert_eq!(
            path.split(PATH_SEP).collect::<Vec<_>>(),
            ["shapes", "Config", "size"]
        );

        let method = class_member_prefix("shapes.Config") + "reset";
        assert!(method.starts_with(&class_member_prefix("shapes.Config")));
        assert_eq!(
            method.split(PATH_SEP).collect::<Vec<_>>(),
            ["shapes", "Config", "reset"]
        );
    }
}
