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

        let mut diagnostics =
            resolve_interface_inheritance(program, &entry_ifaces, &entry_class_shapes);
        for module in modules.iter_mut() {
            let module_ifaces =
                augment_index_with_import_aliases(&iface_index, &module.program.imports);
            let module_class_shapes =
                augment_index_with_import_aliases(&class_shape_index, &module.program.imports);
            diagnostics.extend(resolve_interface_inheritance(
                &mut module.program,
                &module_ifaces,
                &module_class_shapes,
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

/// Snapshot-scoped summaries. References avoid cloning default bodies while
/// merging; materialization happens only at the AST ownership boundary.
#[derive(Default)]
struct IfaceComposition<'a> {
    completed:
        std::collections::HashMap<&'a str, Vec<(&'a parser::ast::InterfaceMethodDecl, &'a str)>>,
    invalid: std::collections::HashSet<&'a str>,
    related_supers: std::collections::HashMap<String, std::collections::HashSet<String>>,
    #[cfg(test)]
    relationship_expansions: usize,
    #[cfg(test)]
    expanded: usize,
    #[cfg(test)]
    merged: usize,
}

impl<'a> IfaceComposition<'a> {
    fn new(snap: &'a IfaceIndex) -> Self {
        // Remove leaves toward their dependents. The remaining vertices are
        // precisely cycles and interfaces reaching a cycle. Invalid declarations
        // retain only own methods; the checker still diagnoses their unchanged
        // extends clauses. This avoids enumerating paths before E0423.
        let mut remaining = std::collections::HashMap::new();
        let mut dependents: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        let mut ready = Vec::new();
        for (name, (supers, _)) in snap {
            let mut count = 0;
            for sup in supers {
                if snap.contains_key(sup) {
                    count += 1;
                    dependents.entry(sup).or_default().push(name);
                }
            }
            remaining.insert(name.as_str(), count);
            if count == 0 {
                ready.push(name.as_str());
            }
        }
        while let Some(name) = ready.pop() {
            if let Some(children) = dependents.get(name) {
                for child in children {
                    let count = remaining.get_mut(child).unwrap();
                    *count -= 1;
                    if *count == 0 {
                        ready.push(child);
                    }
                }
            }
        }
        let invalid: std::collections::HashSet<_> = remaining
            .into_iter()
            .filter_map(|(name, count)| (count != 0).then_some(name))
            .collect();
        let mut result = Self {
            invalid,
            ..Self::default()
        };
        for &name in &result.invalid {
            let mut methods = Vec::new();
            let mut positions = std::collections::HashMap::new();
            for method in &snap[name].1 {
                if let Some(&position) = positions.get(&method.name) {
                    methods[position] = (method, name);
                } else {
                    positions.insert(&method.name, methods.len());
                    methods.push((method, name));
                }
            }
            result.completed.insert(name, methods);
        }
        result
    }

    fn related(&mut self, name: &str, other: &str, snap: &IfaceIndex) -> bool {
        if name == other {
            return true;
        }
        for root in [name, other] {
            if !self.related_supers.contains_key(root) {
                let mut supers = Vec::new();
                let visits = iface_all_supers(
                    root,
                    snap,
                    &mut std::collections::HashSet::new(),
                    &mut supers,
                );
                #[cfg(test)]
                {
                    self.relationship_expansions += visits;
                }
                #[cfg(not(test))]
                let _ = visits;
                self.related_supers
                    .insert(root.to_string(), supers.into_iter().collect());
            }
        }
        self.related_supers[name].contains(other) || self.related_supers[other].contains(name)
    }

    fn compose(
        &mut self,
        name: &'a str,
        snap: &'a IfaceIndex,
    ) -> Vec<(&'a parser::ast::InterfaceMethodDecl, &'a str)> {
        struct Frame<'a> {
            name: &'a str,
            next_super: usize,
            methods: Vec<(&'a parser::ast::InterfaceMethodDecl, &'a str)>,
            positions: std::collections::HashMap<&'a str, usize>,
        }
        impl<'a> Frame<'a> {
            fn new(name: &'a str) -> Self {
                Self {
                    name,
                    next_super: 0,
                    methods: Vec::new(),
                    positions: std::collections::HashMap::new(),
                }
            }
            fn merge(&mut self, method: &'a parser::ast::InterfaceMethodDecl, origin: &'a str) {
                if let Some(&position) = self.positions.get(method.name.as_str()) {
                    self.methods[position] = (method, origin);
                } else {
                    self.positions.insert(&method.name, self.methods.len());
                    self.methods.push((method, origin));
                }
            }
        }
        if let Some(summary) = self.completed.get(name) {
            return summary.clone();
        }
        let mut active = std::collections::HashSet::from([name]);
        let mut stack = vec![Frame::new(name)];
        while let Some(frame) = stack.last_mut() {
            if let Some(sup) = snap
                .get(frame.name)
                .and_then(|(supers, _)| supers.get(frame.next_super))
            {
                frame.next_super += 1;
                if active.contains(sup.as_str()) {
                    unreachable!("cycle prepass excludes invalid roots");
                } else if let Some(summary) = self.completed.get(sup.as_str()) {
                    for &(method, origin) in summary {
                        frame.merge(method, origin);
                        #[cfg(test)]
                        {
                            self.merged += 1;
                        }
                    }
                } else {
                    active.insert(sup.as_str());
                    stack.push(Frame::new(sup));
                }
                continue;
            }
            let mut frame = stack.pop().unwrap();
            #[cfg(test)]
            {
                self.expanded += 1;
            }
            if let Some((_, own)) = snap.get(frame.name) {
                for method in own {
                    frame.merge(method, frame.name);
                    #[cfg(test)]
                    {
                        self.merged += 1;
                    }
                }
            }
            active.remove(frame.name);
            self.completed.insert(frame.name, frame.methods.clone());
            if let Some(parent) = stack.last_mut() {
                for (method, origin) in frame.methods {
                    parent.merge(method, origin);
                    #[cfg(test)]
                    {
                        self.merged += 1;
                    }
                }
            } else {
                return frame.methods;
            }
        }
        unreachable!()
    }

    fn methods(
        &mut self,
        name: &'a str,
        snap: &'a IfaceIndex,
    ) -> Vec<parser::ast::InterfaceMethodDecl> {
        self.compose(name, snap)
            .into_iter()
            .map(|(method, _)| method.clone())
            .collect()
    }
}

/// Transitive super-interface names of `name` (in discovery order).
fn iface_all_supers(
    name: &str,
    snap: &IfaceIndex,
    visiting: &mut std::collections::HashSet<String>,
    out: &mut Vec<String>,
) -> usize {
    enum Step<'a> {
        Enter(&'a str),
        Super(&'a str),
        Leave(&'a str),
    }
    let mut work = vec![Step::Enter(name)];
    let mut seen: std::collections::HashSet<String> = out.iter().cloned().collect();
    let mut completed = std::collections::HashSet::new();
    let mut expanded = 0;
    while let Some(step) = work.pop() {
        match step {
            Step::Super(name) => {
                if seen.insert(name.to_string()) {
                    out.push(name.to_string());
                }
                work.push(Step::Enter(name));
            }
            Step::Enter(name) => {
                if completed.contains(name) || !visiting.insert(name.to_string()) {
                    continue;
                }
                expanded += 1;
                work.push(Step::Leave(name));
                if let Some((supers, _)) = snap.get(name) {
                    work.extend(supers.iter().rev().map(|sup| Step::Super(sup)));
                }
            }
            Step::Leave(name) => {
                visiting.remove(name);
                completed.insert(name);
            }
        }
    }
    expanded
}

fn iface_inherited_default_conflicts<'a>(
    iface_name: &str,
    iface_span: diagnostics::Span,
    extends: &'a [String],
    own_methods: &[parser::ast::InterfaceMethodDecl],
    snap: &'a IfaceIndex,
    composition: &mut IfaceComposition<'a>,
) -> Vec<diagnostics::Diagnostic> {
    use diagnostics::{Diagnostic, ErrorCode, Label, Severity};
    use std::collections::{HashMap, HashSet};

    struct DefaultProvider {
        origin: String,
    }

    if extends.len() < 2 || composition.invalid.contains(iface_name) {
        return Vec::new();
    }

    let own_method_names: HashSet<&str> = own_methods.iter().map(|m| m.name.as_str()).collect();
    let mut inherited_defaults: HashMap<String, Vec<DefaultProvider>> = HashMap::new();
    let mut provider_keys = HashSet::new();

    for sup in extends {
        for (method, origin) in composition.compose(sup, snap) {
            if method.default_body.is_none() || own_method_names.contains(method.name.as_str()) {
                continue;
            }
            let providers = inherited_defaults.entry(method.name.clone()).or_default();
            // A snapshot has one effective declaration per (origin, method).
            if !provider_keys.insert((method.name.as_str(), origin)) {
                continue;
            }
            providers.push(DefaultProvider {
                origin: origin.to_string(),
            });
        }
    }

    let mut diags = Vec::new();
    for (method_name, providers) in inherited_defaults {
        'method: for (idx, left) in providers.iter().enumerate() {
            for right in providers.iter().skip(idx + 1) {
                if composition.related(&left.origin, &right.origin, snap) {
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
/// so cross-module `extends` / `implements` resolve, and `external_classes` the
/// base class and `implements` clause of every imported CLASS, so step 2 keeps
/// working when the ancestor that names the interface lives in a module
/// (willow-himv). Must run BEFORE default-method injection.
fn resolve_interface_inheritance(
    program: &mut parser::ast::Program,
    external: &IfaceIndex,
    external_classes: &std::collections::HashMap<String, ClassShape>,
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

    let mut composition = IfaceComposition::new(&snapshot);
    let mut diags = Vec::new();
    for it in &program.items {
        if let Item::Interface(i) = it {
            diags.extend(iface_inherited_default_conflicts(
                &i.name,
                i.span,
                &i.extends,
                &i.methods,
                &snapshot,
                &mut composition,
            ));
        }
    }

    // class name -> (base class name, directly-implemented interface TYPES), so
    // a subclass can inherit the interfaces its ancestors implement — keeping
    // generic type arguments, e.g. `Into<AppErr>` (willow-2s4i / willow-bpk6).
    //
    // Imported classes go in FIRST, under their module-qualified name and every
    // import-alias spelling of it, because an ancestor of a class declared here
    // may live in a module: without them `class EntryParcel extends lib::Parcel`
    // inherited nothing, so no `(EntryParcel, lib::Measured)` vtable was ever
    // emitted and boxing it into that interface produced a box with no methods
    // (willow-himv). This program's own classes are inserted after, so a local
    // declaration always wins over an entry of the same spelling. The imported
    // shapes carry each class's DIRECT `implements` only — they are built before
    // any program's propagation runs — which is why the walk below is transitive
    // over the whole base chain rather than reading one ancestor's finished list.
    let mut class_info: HashMap<String, (Option<String>, Vec<Type>)> = external_classes
        .iter()
        .map(|(name, shape)| (name.clone(), (shape.base.clone(), shape.implements.clone())))
        .collect();
    class_info.extend(program.items.iter().filter_map(|it| match it {
        Item::Class(c) => {
            let base = c.base_class.as_ref().map(|tp| match tp {
                TypePath::Local(n) => n.clone(),
                TypePath::Qualified(p) => p.join("::"),
            });
            Some((c.name.clone(), (base, c.implements.clone())))
        }
        _ => None,
    }));
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
            let methods = composition.methods(snapshot.get_key_value(&n).unwrap().0, &snapshot);
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
    ty.substitute_names(|name| map.get(name).cloned())
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
    let mut composition = IfaceComposition::new(iface_index);
    let mut out = DefaultMethodIndex::new();
    for m in modules {
        for it in &m.program.items {
            if let Item::Interface(i) = it {
                let qualified = format!("{}::{}", m.name, i.name);
                let composed = composition.methods(
                    iface_index.get_key_value(&qualified).unwrap().0,
                    iface_index,
                );
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

    fn index_from_source(source: &str) -> IfaceIndex {
        parse(source)
            .items
            .into_iter()
            .filter_map(|item| match item {
                Item::Interface(i) => Some((i.name, (i.extends, i.methods))),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn repeated_diamonds_have_linear_summary_work() {
        for depth in [8, 16, 32, 64, 128] {
            let mut source = String::from("interface I0 { fn ping(self) -> i64; }\n");
            for i in 1..=depth {
                source += &format!(
                    "interface A{i} extends I{} {{}}\ninterface B{i} extends I{} {{}}\ninterface I{i} extends A{i}, B{i} {{}}\n",
                    i - 1,
                    i - 1
                );
            }
            let index = index_from_source(&source);
            let mut composition = IfaceComposition::new(&index);
            // All roots plus repeated equivalent calls share the same summaries.
            for _ in 0..3 {
                for name in index.keys() {
                    let methods = composition.compose(name, &index);
                    assert_eq!(methods.len(), 1);
                    assert_eq!(methods[0].1, "I0");
                }
            }
            assert_eq!(composition.expanded, 3 * depth + 1);
            assert_eq!(composition.merged, 4 * depth + 1);
            let mut supers = Vec::new();
            let expanded = iface_all_supers(
                &format!("I{depth}"),
                &index,
                &mut std::collections::HashSet::new(),
                &mut supers,
            );
            assert_eq!(expanded, 3 * depth + 1);
            assert_eq!(supers.len(), 3 * depth);
            println!(
                "depth={depth} expanded={} merged={} ancestors={expanded}",
                composition.expanded, composition.merged
            );
            let mut program = parse(&source);
            assert!(
                DesugarPass::run(&mut program, &mut [])
                    .diagnostics
                    .is_empty()
            );
            assert_eq!(
                program
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        Item::Interface(i) => Some(i.methods.len()),
                        _ => None,
                    })
                    .sum::<usize>(),
                3 * depth + 1
            );
        }
    }

    #[test]
    fn cyclic_graph_recovery_and_relationship_queries_are_bounded() {
        for size in [8, 16, 32, 64] {
            let mut index = IfaceIndex::new();
            for i in 0..size {
                index.insert(
                    format!("I{i}"),
                    ((0..size).map(|j| format!("I{j}")).collect(), vec![]),
                );
            }
            index.insert("Valid".into(), (vec![], vec![]));
            let mut composition = IfaceComposition::new(&index);
            assert_eq!(composition.invalid.len(), size);
            for name in index.keys() {
                assert!(composition.compose(name, &index).is_empty());
            }
            assert_eq!(
                composition.expanded, 1,
                "invalid roots never enter composition"
            );

            let mut chain = IfaceIndex::new();
            for i in 0..size {
                chain.insert(
                    format!("I{i}"),
                    (
                        if i == 0 {
                            vec![]
                        } else {
                            vec![format!("I{}", i - 1)]
                        },
                        vec![],
                    ),
                );
            }
            let mut composition = IfaceComposition::new(&chain);
            for _ in 0..3 {
                for i in 0..size {
                    for j in 0..size {
                        assert!(composition.related(&format!("I{i}"), &format!("I{j}"), &chain));
                    }
                }
            }
            assert_eq!(composition.relationship_expansions, size * (size + 1) / 2);
        }
    }

    #[test]
    fn growing_summaries_and_fanout_are_bounded_by_summary_inputs() {
        for size in [8, 16, 32, 64] {
            let mut source = String::new();
            for i in 0..size {
                let extends = if i == 0 {
                    String::new()
                } else {
                    format!(" extends I{}", i - 1)
                };
                source += &format!("interface I{i}{extends} {{ fn m{i}(self) -> i64; }}\n");
            }
            for i in 0..size {
                source += &format!("interface F{i} extends I{} {{}}\n", size - 1);
            }
            let index = index_from_source(&source);
            let mut composition = IfaceComposition::new(&index);
            for name in index.keys() {
                composition.compose(name, &index);
            }
            assert_eq!(composition.expanded, 2 * size);
            assert_eq!(composition.merged, size * (size + 1) / 2 + size * size);
        }
    }

    #[test]
    fn summaries_preserve_order_origins_and_cycle_context() {
        // Reference the original path traversal, including cycles. Exhaust all
        // directed graphs on three vertices and query every root twice.
        fn reference<'a>(
            name: &'a str,
            index: &'a IfaceIndex,
            active: &mut std::collections::HashSet<&'a str>,
            out: &mut Vec<(&'a parser::ast::InterfaceMethodDecl, &'a str)>,
        ) {
            if !active.insert(name) {
                return;
            }
            if let Some((supers, own)) = index.get(name) {
                for sup in supers {
                    reference(sup, index, active, out);
                }
                for method in own {
                    if let Some(position) = out.iter().position(|(m, _)| m.name == method.name) {
                        out[position] = (method, name);
                    } else {
                        out.push((method, name));
                    }
                }
            }
            active.remove(name);
        }
        for edges in 0..512 {
            let mut index = index_from_source(
                "interface A { fn same(self) -> i64 { return 1; } fn a(self) -> i64; } interface B { fn same(self) -> i64; fn b(self) -> i64; } interface C { fn c(self) -> i64; }",
            );
            let names = ["A", "B", "C"];
            for (i, name) in names.iter().enumerate() {
                index.get_mut(*name).unwrap().0 = names
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| edges & (1 << (3 * i + j)) != 0)
                    .map(|(_, name)| name.to_string())
                    .collect();
            }
            let mut composition = IfaceComposition::new(&index);
            for _ in 0..2 {
                for name in names {
                    let mut expected = Vec::new();
                    reference(
                        name,
                        &index,
                        &mut std::collections::HashSet::new(),
                        &mut expected,
                    );
                    let actual = composition.compose(name, &index);
                    if composition.invalid.contains(name) {
                        assert!(actual.iter().all(|(_, origin)| *origin == name));
                        continue;
                    }
                    let signature = |items: Vec<(&parser::ast::InterfaceMethodDecl, &str)>| {
                        items
                            .into_iter()
                            .map(|(m, origin)| {
                                (m.name.clone(), origin.to_string(), m.default_body.is_some())
                            })
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(
                        signature(actual),
                        signature(expected),
                        "edges={edges} root={name}"
                    );
                }
            }
        }
        let index = index_from_source(
            "interface A {} interface B extends A {} interface C extends A {} interface D extends B, C {}",
        );
        let mut supers = Vec::new();
        iface_all_supers(
            "D",
            &index,
            &mut std::collections::HashSet::new(),
            &mut supers,
        );
        assert_eq!(supers, ["B", "A", "C"]);
    }

    #[test]
    fn deep_interface_composition_and_type_substitution_use_explicit_worklists() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let mut index = IfaceIndex::new();
                for depth in 0..8_000 {
                    let supers = if depth == 7_999 {
                        vec![]
                    } else {
                        vec![format!("I{}", depth + 1)]
                    };
                    let methods = if depth == 7_999 {
                        vec![parser::ast::InterfaceMethodDecl {
                            name: "answer".into(),
                            params: vec![],
                            is_static: false,
                            return_type: Type::I64,
                            default_body: None,
                            span: diagnostics::Span::dummy(),
                        }]
                    } else {
                        vec![]
                    };
                    index.insert(format!("I{depth}"), (supers, methods));
                }
                let methods = IfaceComposition::new(&index).compose("I0", &index);
                assert_eq!(methods.len(), 1);
                assert_eq!(methods[0].1, "I7999");
                let mut supers = Vec::new();
                iface_all_supers(
                    "I0",
                    &index,
                    &mut std::collections::HashSet::new(),
                    &mut supers,
                );
                assert_eq!(supers.len(), 7_999);
                let mut ty = Type::Named("T".into());
                for _ in 0..8_000 {
                    ty = Type::Array(Box::new(ty));
                }
                let substituted = subst_iface_type(
                    &ty,
                    &std::collections::HashMap::from([("T".into(), Type::I64)]),
                );
                let mut leaf = &substituted;
                let mut depth = 0;
                while let Type::Array(inner) = leaf {
                    depth += 1;
                    leaf = inner;
                }
                assert_eq!(depth, 8_000);
                assert_eq!(*leaf, Type::I64);
            })
            .unwrap()
            .join()
            .unwrap();
    }

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

    /// A one-file module, as the driver hands them to [`DesugarPass::run`].
    fn resolved_module(name: &str, source: &str) -> module::ResolvedModule {
        module::ResolvedModule {
            id: crate::module::ModuleId(0),
            name: name.to_string(),
            canonical_path: name.to_string(),
            path: std::path::PathBuf::from(format!("{name}.wi")),
            source: source.to_string(),
            program: parse(source),
        }
    }

    fn class_implements(program: &parser::ast::Program, class: &str) -> Vec<Type> {
        program
            .items
            .iter()
            .find_map(|item| match item {
                Item::Class(c) if c.name == class => Some(c.implements.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("class `{class}` not found"))
    }

    #[test]
    fn pass_inherits_implements_from_a_module_base_class() {
        // willow-himv: the ancestor naming the interface lives in an imported
        // module, so the propagation only reaches it through the class shapes
        // built from `modules` — `program.items` alone never sees `lib::Parcel`.
        let mut modules = [resolved_module(
            "lib",
            "module lib;\n\
             pub interface Measured { fn size(self) -> i64; }\n\
             pub open class Parcel implements Measured {\n\
                 pub side: i64;\n\
                 pub open fn size(self) -> i64 { return self.side; }\n\
             }",
        )];
        let mut program = parse(
            "import lib::Parcel;\n\
             class EntryParcel extends Parcel {}\n\
             fn main() {}",
        );
        let output = DesugarPass::run(&mut program, &mut modules);
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        assert!(
            class_implements(&program, "EntryParcel")
                .contains(&Type::Named("lib::Measured".into())),
            "an entry subclass should inherit its module base's `implements`, got {:?}",
            class_implements(&program, "EntryParcel")
        );
    }

    #[test]
    fn pass_walks_a_chain_that_leaves_and_re_enters_the_entry_program() {
        // `Leaf -> Mid (entry) -> lib::Crate -> lib::Parcel`: the interface is
        // named only by the root, and the imported shapes carry each class's
        // DIRECT `implements`, so the walk has to keep going past `lib::Crate`.
        let mut modules = [resolved_module(
            "lib",
            "module lib;\n\
             pub interface Measured { fn size(self) -> i64; }\n\
             pub open class Parcel implements Measured {\n\
                 pub side: i64;\n\
                 pub open fn size(self) -> i64 { return self.side; }\n\
             }\n\
             pub open class Crate extends Parcel {}",
        )];
        let mut program = parse(
            "import lib::Crate;\n\
             open class Mid extends Crate {}\n\
             class Leaf extends Mid {}\n\
             fn main() {}",
        );
        let output = DesugarPass::run(&mut program, &mut modules);
        assert!(output.diagnostics.is_empty(), "{:?}", output.diagnostics);
        for class in ["Mid", "Leaf"] {
            assert!(
                class_implements(&program, class).contains(&Type::Named("lib::Measured".into())),
                "`{class}` should inherit `lib::Measured`, got {:?}",
                class_implements(&program, class)
            );
        }
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
