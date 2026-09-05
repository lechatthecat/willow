//! AST-to-AST desugaring passes that run after module resolution.

use crate::{diagnostics, module, parser};

/// Result of desugaring the entry program and its resolved modules.
pub struct DesugarOutput {
    pub diagnostics: Vec<diagnostics::Diagnostic>,
}

/// Composes interface inheritance and injects inherited default methods.
pub struct DesugarPass;

impl DesugarPass {
    pub fn run(
        program: &mut parser::ast::Program,
        modules: &mut [module::ResolvedModule],
    ) -> DesugarOutput {
        let iface_index = build_module_iface_index(modules);
        let default_index = build_module_default_methods(modules, &iface_index);
        let class_shape_index = build_module_class_shapes(modules, &default_index);
        let entry_ifaces = augment_index_with_import_aliases(&iface_index, &program.imports);
        let entry_defaults = augment_index_with_import_aliases(&default_index, &program.imports);
        let entry_class_shapes =
            augment_index_with_import_aliases(&class_shape_index, &program.imports);

        let mut diagnostics = resolve_interface_inheritance(program, &entry_ifaces);
        for module in modules.iter_mut() {
            let module_ifaces =
                augment_index_with_import_aliases(&iface_index, &module.program.imports);
            diagnostics.extend(resolve_interface_inheritance(
                &mut module.program,
                &module_ifaces,
            ));
        }

        diagnostics.extend(inject_default_interface_methods(
            program,
            &entry_defaults,
            &entry_class_shapes,
        ));
        for module in modules.iter_mut() {
            let module_defaults =
                augment_index_with_import_aliases(&default_index, &module.program.imports);
            let module_class_shapes =
                augment_index_with_import_aliases(&class_shape_index, &module.program.imports);
            diagnostics.extend(inject_default_interface_methods(
                &mut module.program,
                &module_defaults,
                &module_class_shapes,
            ));
        }
        DesugarOutput { diagnostics }
    }
}

/// Interface inheritance index: interface name -> (direct super names, own
/// method declarations). Built per program, optionally enriched with the
/// module-qualified interfaces of every imported module so cross-module
/// `extends` / `implements` resolve (willow-1js.7, willow-1js.8).
type IfaceIndex =
    std::collections::HashMap<String, (Vec<String>, Vec<parser::ast::InterfaceMethodDecl>)>;

/// Full composed method list for interface `name`, with the interface that
/// originally contributed each effective method. Supers are visited in order,
/// transitively, then own methods; an own/later method of the same name replaces
/// an inherited one in place. `visiting` guards against extends-cycles.
fn iface_compose_methods_with_origin(
    name: &str,
    snap: &IfaceIndex,
    visiting: &mut std::collections::HashSet<String>,
) -> Vec<(parser::ast::InterfaceMethodDecl, String)> {
    fn upsert(
        out: &mut Vec<(parser::ast::InterfaceMethodDecl, String)>,
        m: parser::ast::InterfaceMethodDecl,
        origin: String,
    ) {
        if let Some(existing) = out.iter_mut().find(|(e, _)| e.name == m.name) {
            *existing = (m, origin);
        } else {
            out.push((m, origin));
        }
    }
    let mut out: Vec<(parser::ast::InterfaceMethodDecl, String)> = Vec::new();
    if !visiting.insert(name.to_string()) {
        return out; // cycle: stop recursing
    }
    if let Some((extends, own)) = snap.get(name) {
        for sup in extends {
            for (m, origin) in iface_compose_methods_with_origin(sup, snap, visiting) {
                upsert(&mut out, m, origin);
            }
        }
        for m in own {
            upsert(&mut out, m.clone(), name.to_string());
        }
    }
    visiting.remove(name);
    out
}

/// Full composed method list for interface `name`: supers (in order,
/// transitively) then own; an own/later method of the same name overrides an
/// inherited one in place. `visiting` guards against extends-cycles.
fn iface_compose_methods(
    name: &str,
    snap: &IfaceIndex,
    visiting: &mut std::collections::HashSet<String>,
) -> Vec<parser::ast::InterfaceMethodDecl> {
    iface_compose_methods_with_origin(name, snap, visiting)
        .into_iter()
        .map(|(m, _)| m)
        .collect()
}

/// Transitive super-interface names of `name` (in discovery order).
fn iface_all_supers(
    name: &str,
    snap: &IfaceIndex,
    visiting: &mut std::collections::HashSet<String>,
    out: &mut Vec<String>,
) {
    if !visiting.insert(name.to_string()) {
        return;
    }
    if let Some((extends, _)) = snap.get(name) {
        for sup in extends {
            if !out.contains(sup) {
                out.push(sup.clone());
            }
            iface_all_supers(sup, snap, visiting, out);
        }
    }
    visiting.remove(name);
}

fn iface_names_related(name: &str, other: &str, snap: &IfaceIndex) -> bool {
    if name == other {
        return true;
    }
    let mut name_supers = Vec::new();
    iface_all_supers(
        name,
        snap,
        &mut std::collections::HashSet::new(),
        &mut name_supers,
    );
    if name_supers.iter().any(|s| s == other) {
        return true;
    }
    let mut other_supers = Vec::new();
    iface_all_supers(
        other,
        snap,
        &mut std::collections::HashSet::new(),
        &mut other_supers,
    );
    other_supers.iter().any(|s| s == name)
}

fn iface_inherited_default_conflicts(
    iface_name: &str,
    iface_span: diagnostics::Span,
    extends: &[String],
    own_methods: &[parser::ast::InterfaceMethodDecl],
    snap: &IfaceIndex,
) -> Vec<diagnostics::Diagnostic> {
    use diagnostics::{Diagnostic, ErrorCode, Label, Severity};
    use std::collections::{HashMap, HashSet};

    #[derive(Clone)]
    struct DefaultProvider {
        origin: String,
        span: diagnostics::Span,
    }

    if extends.len() < 2 {
        return Vec::new();
    }

    let own_method_names: HashSet<&str> = own_methods.iter().map(|m| m.name.as_str()).collect();
    let mut inherited_defaults: HashMap<String, Vec<DefaultProvider>> = HashMap::new();

    for sup in extends {
        for (method, origin) in iface_compose_methods_with_origin(sup, snap, &mut HashSet::new()) {
            if method.default_body.is_none() || own_method_names.contains(method.name.as_str()) {
                continue;
            }
            let providers = inherited_defaults.entry(method.name.clone()).or_default();
            if providers
                .iter()
                .any(|p| p.origin == origin && p.span == method.span)
            {
                continue;
            }
            providers.push(DefaultProvider {
                origin,
                span: method.span,
            });
        }
    }

    let mut diags = Vec::new();
    for (method_name, providers) in inherited_defaults {
        'method: for (idx, left) in providers.iter().enumerate() {
            for right in providers.iter().skip(idx + 1) {
                if iface_names_related(&left.origin, &right.origin, snap) {
                    continue;
                }
                diags.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0425,
                        format!(
                            "interface `{iface_name}` inherits conflicting default method `{method_name}` from interfaces `{}` and `{}`",
                            left.origin, right.origin
                        ),
                    )
                    .with_label(Label::primary(
                        iface_span,
                        "ambiguous inherited default method",
                    ))
                    .with_help(format!(
                        "declare `{method_name}` in `{iface_name}` to choose a default or require implementors to override it"
                    )),
                );
                break 'method;
            }
        }
    }
    diags
}

/// Build the module-qualified interface index across every imported module:
/// `mod::Iface -> (qualified supers, own methods)`. A same-module super name is
/// qualified to `mod::Super`; an already-qualified super is kept as written.
/// This lets a class in one module `implements`/`extends` an interface defined
/// in another (willow-1js.7, willow-1js.8).
fn build_module_iface_index(modules: &[module::ResolvedModule]) -> IfaceIndex {
    use parser::ast::Item;
    let mut index = IfaceIndex::new();
    for m in modules {
        // Local interface names declared by this module (to detect same-module
        // supers that need qualifying).
        let local: std::collections::HashSet<&str> = m
            .program
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Interface(i) => Some(i.name.as_str()),
                _ => None,
            })
            .collect();
        for it in &m.program.items {
            if let Item::Interface(i) = it {
                let qualified = format!("{}::{}", m.name, i.name);
                let supers = i
                    .extends
                    .iter()
                    .map(|s| {
                        if !s.contains("::") && local.contains(s.as_str()) {
                            format!("{}::{}", m.name, s)
                        } else {
                            s.clone()
                        }
                    })
                    .collect();
                index.insert(qualified, (supers, i.methods.clone()));
            }
        }
    }
    index
}

/// Return a copy of `base` with each of `imports`' directly-imported type names
/// bound: `import mod::Iface` (path `mod::Iface`) aliases the bare local name
/// (`Iface`, or the `as` alias) to the qualified index entry. A whole-module
/// import (`import mod`, single segment) is skipped. Used so each program
/// resolves its own direct-import interface aliases during desugar
/// (willow-1js.7, willow-1js.8).
fn augment_index_with_import_aliases<V: Clone>(
    base: &std::collections::HashMap<String, V>,
    imports: &[parser::ast::ImportDecl],
) -> std::collections::HashMap<String, V> {
    let mut out = base.clone();
    for imp in imports {
        let segs: Vec<&str> = imp.path.split("::").collect();
        if segs.len() < 2 {
            continue; // whole-module import, not a direct type import
        }
        let local = imp
            .alias
            .clone()
            .unwrap_or_else(|| (*segs.last().unwrap()).to_string());
        if let Some(v) = base.get(&imp.path) {
            out.entry(local).or_insert_with(|| v.clone());
        }
    }
    out
}

/// Resolve interface inheritance (willow-1js.2 / willow-1js.8) by desugaring on
/// the AST:
///  1. Compose each interface's method list as `[super methods..., own methods]`
///     (transitively, deduped by name; an own method overrides an inherited one
///     in place, preserving slot order so a sub-interface vtable stays layout-
///     compatible with its super's).
///  2. Expand each class's `implements` clause with the transitive super-
///     interfaces of every interface it implements, so the class is usable as
///     (and gets a vtable for) each super, and conformance covers the full set.
///
/// `external` carries the module-qualified interfaces of every imported module
/// so cross-module `extends` / `implements` resolve. Must run BEFORE
/// default-method injection.
fn resolve_interface_inheritance(
    program: &mut parser::ast::Program,
    external: &IfaceIndex,
) -> Vec<diagnostics::Diagnostic> {
    use parser::ast::{Item, Type, TypePath};
    use std::collections::{HashMap, HashSet};

    // name -> (direct supers, own methods): this program's own interfaces (bare
    // names) merged with the qualified interfaces of imported modules.
    let mut snapshot: IfaceIndex = external.clone();
    for it in &program.items {
        if let Item::Interface(i) = it {
            snapshot.insert(i.name.clone(), (i.extends.clone(), i.methods.clone()));
        }
    }

    let mut diags = Vec::new();
    for it in &program.items {
        if let Item::Interface(i) = it {
            diags.extend(iface_inherited_default_conflicts(
                &i.name, i.span, &i.extends, &i.methods, &snapshot,
            ));
        }
    }

    // class name -> (base class name, directly-implemented interface TYPES), so
    // a subclass can inherit the interfaces its ancestors implement — keeping
    // generic type arguments, e.g. `Into<AppErr>` (willow-2s4i / willow-bpk6).
    let class_info: HashMap<String, (Option<String>, Vec<Type>)> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Class(c) => {
                let base = c.base_class.as_ref().map(|tp| match tp {
                    TypePath::Local(n) => n.clone(),
                    TypePath::Qualified(p) => p.join("::"),
                });
                Some((c.name.clone(), (base, c.implements.clone())))
            }
            _ => None,
        })
        .collect();
    // Nothing to do only when there is neither interface inheritance nor any
    // class with a base class (a subclass may inherit its base's interfaces).
    let own_has_inheritance = program.items.iter().any(|it| match it {
        Item::Interface(i) => !i.extends.is_empty(),
        Item::Class(c) => c.base_class.is_some() || !c.implements.is_empty(),
        _ => false,
    });
    if !own_has_inheritance {
        return diags;
    }

    // Interface TYPES implemented by `class`'s ANCESTORS (transitive base-class
    // chain), preserving generic type args; deduped by interface name.
    fn inherited_class_interfaces(
        class: &str,
        class_info: &HashMap<String, (Option<String>, Vec<Type>)>,
        out: &mut Vec<Type>,
    ) {
        fn iface_name(t: &Type) -> Option<&str> {
            match t {
                Type::Named(n) | Type::Generic(n, _) => Some(n.as_str()),
                _ => None,
            }
        }
        let mut current = class_info.get(class).and_then(|(base, _)| base.clone());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                break;
            }
            match class_info.get(&name) {
                Some((base, impls)) => {
                    for iface in impls {
                        let already = iface_name(iface)
                            .map(|n| out.iter().any(|o| iface_name(o) == Some(n)))
                            .unwrap_or(true);
                        if !already {
                            out.push(iface.clone());
                        }
                    }
                    current = base.clone();
                }
                None => break,
            }
        }
    }

    let composed: HashMap<String, Vec<parser::ast::InterfaceMethodDecl>> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Interface(i) => Some(i.name.clone()),
            _ => None,
        })
        .map(|n| {
            let methods = iface_compose_methods(&n, &snapshot, &mut HashSet::new());
            (n, methods)
        })
        .collect();

    for item in &mut program.items {
        match item {
            Item::Interface(i) => {
                if let Some(methods) = composed.get(&i.name) {
                    i.methods = methods.clone();
                }
            }
            Item::Class(c) => {
                let mut implemented: HashSet<String> = c
                    .implements
                    .iter()
                    .filter_map(|t| match t {
                        Type::Named(n) | Type::Generic(n, _) => Some(n.clone()),
                        _ => None,
                    })
                    .collect();
                // Interfaces implemented through the base-class chain are added
                // to this subclass too (preserving generic type args), so it gets
                // its own (class, interface) vtable and is usable as that
                // interface (willow-2s4i / willow-bpk6).
                let mut inherited = Vec::new();
                inherited_class_interfaces(&c.name, &class_info, &mut inherited);
                for iface_ty in inherited {
                    if let Type::Named(n) | Type::Generic(n, _) = &iface_ty
                        && implemented.insert(n.clone())
                    {
                        c.implements.push(iface_ty.clone());
                    }
                }
                // Add the transitive super-interfaces of every implemented
                // interface (by name).
                let names: Vec<String> = implemented.iter().cloned().collect();
                for iface in names {
                    let mut supers = Vec::new();
                    iface_all_supers(&iface, &snapshot, &mut HashSet::new(), &mut supers);
                    for sup in supers {
                        // `Send`/`Sync` are compiler-known markers (no methods, no
                        // vtable); a class's Send/Sync-ness is INFERRED, not carried
                        // in its `implements` list. Skipping them here keeps the
                        // transitive marker out of `implements` so the manual-impl
                        // check (E2401) only flags directly-written `implements
                        // Send/Sync` (willow-dgwo).
                        if sup == "Send" || sup == "Sync" {
                            continue;
                        }
                        if implemented.insert(sup.clone()) {
                            c.implements.push(Type::Named(sup));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    diags
}

/// Default (body-carrying) interface methods, indexed for injection: interface
/// name -> (its generic type-parameter names, its default methods). Built per
/// program and enriched with the qualified interfaces of imported modules.
type DefaultMethodIndex =
    std::collections::HashMap<String, (Vec<String>, Vec<parser::ast::InterfaceMethodDecl>)>;

/// The part of an imported class declaration needed to decide whether a
/// default method is already inherited. Unlike the type checker's full class
/// symbol, this is available during AST desugaring.
#[derive(Clone)]
struct ClassShape {
    base: Option<String>,
    declared: std::collections::HashSet<String>,
    implements: Vec<parser::ast::Type>,
}

/// Substitute interface generic type parameters (and `Self`) in a type. Used so
/// a default method inherited into a class that implements `Box<i64>` has its
/// `T`s replaced by `i64` and `Self` by the class (willow-1js.7).
fn subst_iface_type(
    ty: &parser::ast::Type,
    map: &std::collections::HashMap<String, parser::ast::Type>,
) -> parser::ast::Type {
    use parser::ast::Type;
    match ty {
        Type::Named(n) => map.get(n).cloned().unwrap_or_else(|| ty.clone()),
        Type::Generic(n, args) => {
            let args = args.iter().map(|a| subst_iface_type(a, map)).collect();
            // A bare type-parameter used as a generic head is unusual; keep the
            // head name (only its args are substituted).
            Type::Generic(n.clone(), args)
        }
        Type::Array(e) => Type::Array(Box::new(subst_iface_type(e, map))),
        Type::Fn(ps, r) => Type::Fn(
            ps.iter().map(|p| subst_iface_type(p, map)).collect(),
            Box::new(subst_iface_type(r, map)),
        ),
        _ => ty.clone(),
    }
}

/// Build the cross-module default-method index: for every interface declared in
/// an imported module, its module-qualified name -> (type params, composed
/// default methods). Composition pulls defaults inherited from super-interfaces
/// too (willow-1js.7). `iface_index` supplies the qualified inheritance graph.
fn build_module_default_methods(
    modules: &[module::ResolvedModule],
    iface_index: &IfaceIndex,
) -> DefaultMethodIndex {
    use parser::ast::Item;
    use std::collections::HashSet;
    let mut out = DefaultMethodIndex::new();
    for m in modules {
        for it in &m.program.items {
            if let Item::Interface(i) = it {
                let qualified = format!("{}::{}", m.name, i.name);
                let composed = iface_compose_methods(&qualified, iface_index, &mut HashSet::new());
                let with_body: Vec<_> = composed
                    .into_iter()
                    .filter(|mm| mm.default_body.is_some())
                    .collect();
                if !with_body.is_empty() {
                    out.insert(qualified, (i.type_params.clone(), with_body));
                }
            }
        }
    }
    out
}

/// Build module-qualified class shapes for default injection in importers.
///
/// A class's declared method set includes defaults its own `implements` clauses
/// will synthesize later in this pass. This lets an entry subclass inherit that
/// method from an imported base instead of receiving a second copy merely
/// because the base's AST lives in another `Program` (willow-3eo1).
fn build_module_class_shapes(
    modules: &[module::ResolvedModule],
    defaults: &DefaultMethodIndex,
) -> std::collections::HashMap<String, ClassShape> {
    use parser::ast::{Item, Type, TypePath};
    use std::collections::{HashMap, HashSet};

    let mut out = HashMap::new();
    for module in modules {
        let local_classes: HashSet<&str> = module
            .program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Class(class) => Some(class.name.as_str()),
                _ => None,
            })
            .collect();
        let local_interfaces: HashSet<&str> = module
            .program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Interface(interface) => Some(interface.name.as_str()),
                _ => None,
            })
            .collect();
        let import_aliases: HashMap<String, String> = module
            .program
            .imports
            .iter()
            .filter_map(|import| {
                import.path.rsplit_once("::").map(|(_, item)| {
                    (
                        import.alias.clone().unwrap_or_else(|| item.to_string()),
                        import.path.clone(),
                    )
                })
            })
            .collect();

        for item in &module.program.items {
            let Item::Class(class) = item else { continue };
            let mut declared: HashSet<String> = class
                .methods
                .iter()
                .map(|method| method.name.clone())
                .collect();
            let mut implements = Vec::with_capacity(class.implements.len());
            for interface_ty in &class.implements {
                let qualified = match interface_ty {
                    Type::Named(name) => {
                        let name = if local_interfaces.contains(name.as_str()) {
                            format!("{}::{name}", module.name)
                        } else {
                            import_aliases
                                .get(name)
                                .cloned()
                                .unwrap_or_else(|| name.clone())
                        };
                        Type::Named(name)
                    }
                    Type::Generic(name, args) => {
                        let name = if local_interfaces.contains(name.as_str()) {
                            format!("{}::{name}", module.name)
                        } else {
                            import_aliases
                                .get(name)
                                .cloned()
                                .unwrap_or_else(|| name.clone())
                        };
                        Type::Generic(name, args.clone())
                    }
                    _ => interface_ty.clone(),
                };
                let interface_name = match &qualified {
                    Type::Named(name) | Type::Generic(name, _) => Some(name.as_str()),
                    _ => None,
                };
                if let Some((_, methods)) = interface_name.and_then(|name| defaults.get(name)) {
                    declared.extend(methods.iter().map(|method| method.name.clone()));
                }
                implements.push(qualified);
            }

            let base = class.base_class.as_ref().map(|base| match base {
                TypePath::Local(name) if local_classes.contains(name.as_str()) => {
                    format!("{}::{name}", module.name)
                }
                TypePath::Local(name) => import_aliases
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| name.clone()),
                TypePath::Qualified(parts) => parts.join("::"),
            });
            out.insert(
                format!("{}::{}", module.name, class.name),
                ClassShape {
                    base,
                    declared,
                    implements,
                },
            );
        }
    }
    out
}

/// Inject default interface methods (willow-1js.3 / willow-1js.7): for each
/// class, for each interface it implements that defines a method with a default
/// body, if the class does not already declare a method of that name, synthesize
/// a class method whose body is the default. `self` then refers to the concrete
/// class, so sibling interface calls dispatch normally. Generic interface type
/// parameters are substituted from the class's `implements Iface<Args>` clause.
///
/// `external` carries the qualified default methods of imported modules so a
/// class can inherit a default from a cross-module interface. Returns diagnostics
/// for ambiguous defaults (E0425): two independent implemented interfaces that
/// both provide a default for the same method the class does not override.
fn inject_default_interface_methods(
    program: &mut parser::ast::Program,
    external: &DefaultMethodIndex,
    external_class_shapes: &std::collections::HashMap<String, ClassShape>,
) -> Vec<diagnostics::Diagnostic> {
    use diagnostics::{Diagnostic, ErrorCode, Label, Severity};
    use parser::ast::{Item, MethodDecl, Type, TypePath};
    use std::collections::{HashMap, HashSet};

    // interface name -> (type params, default methods): this program's own
    // interfaces (bare, already inheritance-composed) merged with the qualified
    // defaults of imported modules.
    let mut defaults: DefaultMethodIndex = external.clone();
    for item in &program.items {
        if let Item::Interface(iface) = item {
            let with_body: Vec<_> = iface
                .methods
                .iter()
                .filter(|m| m.default_body.is_some())
                .cloned()
                .collect();
            if !with_body.is_empty() {
                defaults.insert(iface.name.clone(), (iface.type_params.clone(), with_body));
            }
        }
    }
    if defaults.is_empty() {
        return Vec::new();
    }

    // Interfaces declared in THIS program: their non-generic default bodies are
    // type-checked once at the interface level (check_interface), so the injected
    // class copy is marked to be skipped there. A default inherited from another
    // module's interface is NOT checked at the interface level here, so its class
    // copy stays checkable (willow-1js.7).
    let own_iface_names: HashSet<String> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Interface(i) => Some(i.name.clone()),
            _ => None,
        })
        .collect();

    // Inheritance graph (own bare + imported qualified) for super/sub checks so
    // an inherited default does not count as "ambiguous" with its own super.
    let mut supers_index: IfaceIndex = IfaceIndex::new();
    for item in &program.items {
        if let Item::Interface(i) = item {
            supers_index.insert(i.name.clone(), (i.extends.clone(), Vec::new()));
        }
    }
    let related = |a: &str, b: &str| -> bool {
        if a == b {
            return true;
        }
        let mut sa = Vec::new();
        iface_all_supers(a, &supers_index, &mut HashSet::new(), &mut sa);
        if sa.iter().any(|s| s == b) {
            return true;
        }
        let mut sb = Vec::new();
        iface_all_supers(b, &supers_index, &mut HashSet::new(), &mut sb);
        sb.iter().any(|s| s == a)
    };

    // Base class and, per class, the methods it declares itself plus the
    // interfaces it implements — enough to ask whether an ANCESTOR already ends
    // up with a given method name.
    let mut shapes = external_class_shapes.clone();
    shapes.extend(program.items.iter().filter_map(|it| match it {
        Item::Class(c) => Some((
            c.name.clone(),
            ClassShape {
                base: c.base_class.as_ref().map(|tp| match tp {
                    TypePath::Local(n) => n.clone(),
                    TypePath::Qualified(p) => p.join("::"),
                }),
                declared: c.methods.iter().map(|m| m.name.clone()).collect(),
                implements: c.implements.clone(),
            },
        )),
        _ => None,
    }));

    // Does some ancestor CLASS of `class` already end up with `method`, either
    // because it declares one itself or because it implements `iface_ty` — the
    // very same interface, type arguments included — and so receives the same
    // injected copy?
    //
    // `implements` is propagated down an `extends` chain (willow-2s4i), so
    // without this every class in the chain would receive its own copy of the
    // same default body. Two copies of a method that is neither `open` nor
    // `override` have no vtable slot to disambiguate them, which the backend
    // reports as "no virtual slot but 2 candidate implementations". Injecting
    // only at the topmost class that implements the interface leaves the
    // subclasses to INHERIT it, exactly as they inherit any other base method
    // (willow-3eo1).
    //
    // The interface type must match exactly: an ancestor implementing
    // `Holder<String>` does not provide the `Holder<i64>` copy this class needs,
    // because the substituted signatures differ.
    let ancestor_provides = |class: &str, iface_ty: &Type, method: &str| -> bool {
        let mut seen: HashSet<String> = HashSet::new();
        let mut current = shapes.get(class).and_then(|s| s.base.clone());
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                return false;
            }
            let Some(shape) = shapes.get(&name) else {
                return false;
            };
            if shape.declared.contains(method) {
                return true;
            }
            if shape.implements.iter().any(|t| t == iface_ty) {
                return true;
            }
            current = shape.base.clone();
        }
        false
    };

    let mut diags = Vec::new();
    for item in &mut program.items {
        let Item::Class(class) = item else { continue };
        let overridden: HashSet<String> = class.methods.iter().map(|m| m.name.clone()).collect();
        // method name -> (providing interface, the synthesized decl).
        let mut chosen: HashMap<String, (String, MethodDecl)> = HashMap::new();
        for iface_ty in &class.implements {
            let (iface_name, type_args): (&str, &[Type]) = match iface_ty {
                Type::Named(n) => (n.as_str(), &[]),
                Type::Generic(n, args) => (n.as_str(), args.as_slice()),
                _ => continue,
            };
            let Some((type_params, methods)) = defaults.get(iface_name) else {
                continue;
            };
            // Substitution map: interface type params -> concrete args, Self -> class.
            let mut subst: HashMap<String, Type> = HashMap::new();
            for (p, a) in type_params.iter().zip(type_args.iter()) {
                subst.insert(p.clone(), a.clone());
            }
            subst.insert("Self".to_string(), Type::Named(class.name.clone()));
            for dm in methods {
                // The class explicitly overrides this default: nothing to inject.
                if overridden.contains(&dm.name) {
                    continue;
                }
                // A base class already ends up with this method: inherit it.
                if ancestor_provides(&class.name, iface_ty, &dm.name) {
                    continue;
                }
                let Some(body) = &dm.default_body else {
                    continue;
                };
                if let Some((prev_iface, _)) = chosen.get(&dm.name) {
                    // Two interfaces providing the same default: only ambiguous if
                    // they are independent (neither extends the other).
                    if !related(prev_iface, iface_name) {
                        diags.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0425,
                                format!(
                                    "class `{}` inherits conflicting default method `{}` from interfaces `{}` and `{}`",
                                    class.name, dm.name, prev_iface, iface_name
                                ),
                            )
                            .with_label(Label::primary(class.span, "ambiguous default method"))
                            .with_help(format!(
                                "override `{}` in `{}` to disambiguate",
                                dm.name, class.name
                            )),
                        );
                    }
                    continue;
                }
                let params = dm
                    .params
                    .iter()
                    .map(|p| {
                        let mut p = p.clone();
                        p.ty = subst_iface_type(&p.ty, &subst);
                        p
                    })
                    .collect();
                chosen.insert(
                    dm.name.clone(),
                    (
                        iface_name.to_string(),
                        MethodDecl {
                            name: dm.name.clone(),
                            public: true, // interface methods are public by contract
                            protected: false,
                            is_async: false,
                            is_open: false,
                            is_override: false,
                            is_static: false,
                            params,

                            return_type: subst_iface_type(&dm.return_type, &subst),
                            body: body.clone(),
                            span: dm.span,
                            // Non-generic default bodies of an interface declared
                            // in THIS program are checked once at the interface
                            // level (skipped on the class to avoid duplicate
                            // diagnostics); generic ones and cross-module ones need
                            // the (substituted) copy checked here (willow-1js.7).
                            is_default_injected: type_params.is_empty()
                                && own_iface_names.contains(iface_name),
                            is_interface_default: true,
                        },
                    ),
                );
            }
        }
        class.methods.extend(chosen.into_values().map(|(_, m)| m));
    }
    diags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::parser::ast::{Item, Type};

    fn parse(source: &str) -> parser::ast::Program {
        let tokens = Lexer::new(source).tokenize().unwrap();
        let (program, diagnostics) = Parser::new(tokens).parse();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        program
    }

    #[test]
    fn pass_accepts_program_without_desugaring_work() {
        let mut program = parse("fn main() {}");
        let output = DesugarPass::run(&mut program, &mut []);
        assert!(output.diagnostics.is_empty());
    }

    #[test]
    fn pass_composes_interface_methods_and_class_supers() {
        let mut program = parse(
            "interface A { fn a(self) -> i64; }\n\
             interface B extends A { fn b(self) -> i64; }\n\
             class C implements B {\n\
                 pub fn a(self) -> i64 { return 1; }\n\
                 pub fn b(self) -> i64 { return 2; }\n\
             }\
             fn main() {}",
        );
        let output = DesugarPass::run(&mut program, &mut []);
        assert!(output.diagnostics.is_empty());

        let b = program.items.iter().find_map(|item| match item {
            Item::Interface(interface) if interface.name == "B" => Some(interface),
            _ => None,
        });
        assert_eq!(
            b.unwrap()
                .methods
                .iter()
                .map(|method| method.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );

        let class = program.items.iter().find_map(|item| match item {
            Item::Class(class) if class.name == "C" => Some(class),
            _ => None,
        });
        assert!(class.unwrap().implements.contains(&Type::Named("A".into())));
    }

    #[test]
    fn pass_injects_default_method_body() {
        let mut program = parse(
            "interface Greeter { fn greet(self) -> i64 { return 42; } }\n\
             class C implements Greeter {}\n\
             fn main() {}",
        );
        let output = DesugarPass::run(&mut program, &mut []);
        assert!(output.diagnostics.is_empty());
        let class = program.items.iter().find_map(|item| match item {
            Item::Class(class) if class.name == "C" => Some(class),
            _ => None,
        });
        assert!(
            class
                .unwrap()
                .methods
                .iter()
                .any(|method| method.name == "greet")
        );
    }
}
