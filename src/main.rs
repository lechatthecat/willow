mod backend;
mod diagnostics;
mod ir;
mod lexer;
mod module;
mod parser;
mod prelude;
mod project;
mod semantic;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BuildMode {
    Debug,
    Release,
}

pub struct CodegenOptions {
    pub build_mode: BuildMode,
    pub emit_debug_info: bool,
    pub emit_source_map: bool,
    pub strip_symbols: bool,
    pub runtime_lib: Option<PathBuf>,
}

impl CodegenOptions {
    pub fn debug() -> Self {
        Self {
            build_mode: BuildMode::Debug,
            emit_debug_info: true,
            emit_source_map: true,
            strip_symbols: false,
            runtime_lib: None,
        }
    }

    pub fn release() -> Self {
        Self {
            build_mode: BuildMode::Release,
            emit_debug_info: false,
            emit_source_map: false,
            strip_symbols: false,
            runtime_lib: None,
        }
    }

    pub fn release_with_debug_info() -> Self {
        Self {
            build_mode: BuildMode::Release,
            emit_debug_info: true,
            emit_source_map: true,
            strip_symbols: false,
            runtime_lib: None,
        }
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(|s| s.as_str()) {
        Some("build") => cmd_build(&args[2..]),
        Some("run") => cmd_run(&args[2..]),
        Some("debug") => cmd_debug(&args[2..]),
        Some(src) if src.ends_with(".wi") => {
            // 後方互換: willowc <file> [-o <output>]
            let out = if let Some(pos) = args.iter().position(|a| a == "-o") {
                args.get(pos + 1)
                    .cloned()
                    .unwrap_or_else(|| "a.out".to_string())
            } else {
                PathBuf::from(src)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("a")
                    .to_string()
            };
            let mut opts = CodegenOptions::debug();
            opts.runtime_lib = parse_runtime_lib_arg(&args[2..]);
            compile(src, &out, &opts, None)
        }
        _ => {
            eprintln!("Usage:");
            eprintln!(
                "  willowc build <source.wi> [-o <output>] [--debug|--release] [--debug-info] [--runtime-lib <path>]"
            );
            eprintln!(
                "  willowc run   <source.wi> [--debug|--release] [--debug-info] [--runtime-lib <path>] [-- <args>...]"
            );
            eprintln!("  willowc debug <source.wi>");
            std::process::exit(1);
        }
    }
}

fn truthy_env(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v != "0" && !v.is_empty())
}

fn worker_env_enables_data_race_check(value: Option<&str>) -> bool {
    value
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .is_some_and(|workers| workers > 1)
}

fn data_race_check_enabled_from_env() -> bool {
    truthy_env("WILLOW_DATA_RACE_CHECK")
        || worker_env_enables_data_race_check(std::env::var("WILLOW_WORKERS").ok().as_deref())
}

fn cmd_build(args: &[String]) -> Result<()> {
    // Single-file mode: explicit .wi file in args.
    if let Some(src) = args.iter().find(|a| a.ends_with(".wi")) {
        let out = if let Some(pos) = args.iter().position(|a| a == "-o") {
            args.get(pos + 1).cloned().unwrap_or_else(|| stem(src))
        } else {
            stem(src)
        };
        let opts = parse_build_mode(args);
        return compile(src, &out, &opts, None);
    }

    // Project mode: look for project.toml in the specified dir or cwd.
    let search_dir = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(|s| PathBuf::from(s))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let (manifest_path, project_root) =
        project::find_project_manifest(&search_dir).ok_or_else(|| {
            anyhow::anyhow!(
                "no source file or project.toml found (searched from {})",
                search_dir.display()
            )
        })?;

    let manifest = project::ProjectManifest::load(&manifest_path)?;
    let entry = manifest.entry_point(&project_root);

    if !entry.exists() {
        anyhow::bail!(
            "entry point not found: {} (declared in {})",
            entry.display(),
            manifest_path.display()
        );
    }

    let out = if let Some(pos) = args.iter().position(|a| a == "-o") {
        args.get(pos + 1)
            .cloned()
            .unwrap_or_else(|| manifest.project.name.clone())
    } else {
        manifest.project.name.clone()
    };

    let opts = parse_build_mode(args);
    eprintln!(
        "building project '{}' v{}",
        manifest.project.name, manifest.project.version
    );
    compile(entry.to_str().unwrap(), &out, &opts, Some(project_root))
}

fn cmd_run(args: &[String]) -> Result<()> {
    let separator = args.iter().position(|a| a == "--");
    let (compiler_args, program_args) = match separator {
        Some(pos) => (&args[..pos], &args[pos + 1..]),
        None => (args, &[][..]),
    };
    let src = compiler_args
        .iter()
        .find(|a| a.ends_with(".wi"))
        .ok_or_else(|| anyhow::anyhow!("no source file specified"))?;
    let out = temp_path(format!("willow_run_{}", stem(src)));
    let opts = parse_build_mode(compiler_args);
    compile(src, &out, &opts, None)?;
    let status = Command::new(&out)
        .args(program_args)
        .status()
        .with_context(|| format!("failed to run {}", out))?;
    std::process::exit(status.code().unwrap_or(0));
}

fn cmd_debug(args: &[String]) -> Result<()> {
    let src = args
        .iter()
        .find(|a| a.ends_with(".wi"))
        .ok_or_else(|| anyhow::anyhow!("no source file specified"))?;
    let out = temp_path(format!("willow_debug_{}", stem(src)));
    let mut opts = CodegenOptions::debug();
    opts.runtime_lib = parse_runtime_lib_arg(args);
    compile(src, &out, &opts, None)?;
    eprintln!("note: interactive debugger not yet implemented");
    eprintln!("running in debug mode: {}", out);
    let status = Command::new(&out)
        .status()
        .with_context(|| format!("failed to run {}", out))?;
    std::process::exit(status.code().unwrap_or(0));
}

fn parse_build_mode(args: &[String]) -> CodegenOptions {
    let mut opts = if args.iter().any(|a| a == "--release") {
        if args.iter().any(|a| a == "--debug-info") {
            CodegenOptions::release_with_debug_info()
        } else {
            CodegenOptions::release()
        }
    } else {
        CodegenOptions::debug()
    };
    opts.runtime_lib = parse_runtime_lib_arg(args);
    opts
}

fn parse_runtime_lib_arg(args: &[String]) -> Option<PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--runtime-lib" {
            return iter.next().map(PathBuf::from);
        }
        if let Some(path) = arg.strip_prefix("--runtime-lib=") {
            return Some(PathBuf::from(path));
        }
    }
    None
}

fn stem(path: &str) -> String {
    PathBuf::from(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("a")
        .to_string()
}

fn temp_path(path: impl AsRef<std::path::Path>) -> String {
    std::env::temp_dir()
        .join(path)
        .to_string_lossy()
        .into_owned()
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
                    if let Type::Named(n) | Type::Generic(n, _) = &iface_ty {
                        if implemented.insert(n.clone()) {
                            c.implements.push(iface_ty.clone());
                        }
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
        Type::Nullable(i) => Type::Nullable(Box::new(subst_iface_type(i, map))),
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
) -> Vec<diagnostics::Diagnostic> {
    use diagnostics::{Diagnostic, ErrorCode, Label, Severity};
    use parser::ast::{Item, MethodDecl, Type};
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
                            has_self: dm.has_self,
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
                        },
                    ),
                );
            }
        }
        class.methods.extend(chosen.into_values().map(|(_, m)| m));
    }
    diags
}

fn register_prelude(checker: &mut semantic::TypeChecker) -> Result<()> {
    let tokens = lexer::Lexer::new(prelude::PRELUDE_SOURCE)
        .tokenize()
        .map_err(|_| anyhow::anyhow!("internal error: prelude lexer failed"))?;
    let (program, errors) = parser::Parser::new(tokens).parse();
    if !errors.is_empty() {
        anyhow::bail!("internal error: prelude parse failed");
    }
    // Register only declarations; do not type-check the prelude body.
    use parser::ast::Item;
    for item in &program.items {
        match item {
            Item::Enum(e) => checker.register_prelude_enum(e),
            Item::Interface(i) => checker.register_prelude_interface(i),
            Item::Function(f) => {
                // Future: register prelude functions (e.g. panic) here.
                let _ = f;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Front-end artifacts produced by [`run_frontend`] and consumed by
/// [`run_backend`]: a fully desugared + type-checked program plus its resolved
/// modules and the type checker (whose symbol tables feed codegen).
struct Frontend {
    program: parser::ast::Program,
    modules: Vec<module::ResolvedModule>,
    item_imports: Vec<module::resolver::ItemImport>,
    checker: semantic::TypeChecker,
}

/// A single compilation request. Owns the shared context (paths, options,
/// source text, source map) and drives the explicit phases: front-end
/// (lex → parse → import resolution → desugar → type/concurrency checks) and
/// back-end (codegen → link → artifacts). Splitting the phases keeps the
/// driver testable and lets future front-ends (LSP, test harness) reuse them.
struct CompilerSession<'a> {
    src: &'a str,
    out: &'a str,
    opts: &'a CodegenOptions,
    project_root: Option<PathBuf>,
}

impl<'a> CompilerSession<'a> {
    fn new(
        src: &'a str,
        out: &'a str,
        opts: &'a CodegenOptions,
        project_root: Option<PathBuf>,
    ) -> Self {
        Self {
            src,
            out,
            opts,
            project_root,
        }
    }

    fn run(self) -> Result<()> {
        let src_path = PathBuf::from(self.src);
        let source = std::fs::read_to_string(&src_path)
            .with_context(|| format!("cannot read {}", src_path.display()))?;

        // Import resolution root: the directory containing the source file.
        let _ = self.project_root; // available for future use (e.g. package search paths)
        let root = src_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let map = diagnostics::SourceMap::new(self.src, &source);

        let frontend = run_frontend(&source, &root, &map)?;
        run_backend(frontend, self.src, self.out, source, self.opts, &map)
    }
}

/// Front-end phases: lex, parse, resolve imports, desugar interface inheritance
/// and default methods, then run the type checker and concurrency analyses.
/// Diagnostics are emitted as they are found; the phase aborts (returning `Err`)
/// if any stage produced an error, so a successful return yields a program that
/// is safe to hand to the back-end.
struct PhaseDiagnostics {
    diagnostics: Vec<diagnostics::Diagnostic>,
    error_count: usize,
}

impl PhaseDiagnostics {
    fn new(diagnostics: Vec<diagnostics::Diagnostic>) -> Self {
        let error_count = diagnostic_error_count(&diagnostics);
        Self {
            diagnostics,
            error_count,
        }
    }
}

struct ParsePhase {
    program: parser::ast::Program,
    outcome: PhaseDiagnostics,
}

struct ImportPhase {
    modules: Vec<module::ResolvedModule>,
    item_imports: Vec<module::resolver::ItemImport>,
    outcome: PhaseDiagnostics,
}

struct TypecheckPhase {
    checker: semantic::TypeChecker,
    error_count: usize,
}

struct ModulePhaseDiagnostics {
    path: String,
    source: String,
    diagnostics: Vec<diagnostics::Diagnostic>,
}

struct ConcurrencyPhase {
    entry_diagnostics: Vec<diagnostics::Diagnostic>,
    module_diagnostics: Vec<ModulePhaseDiagnostics>,
    error_count: usize,
}

fn run_frontend(
    source: &str,
    root: &std::path::Path,
    map: &diagnostics::SourceMap,
) -> Result<Frontend> {
    let tokens = lex_phase(source).map_err(|errs| {
        diagnostics::emit_all(&errs, map);
        anyhow::anyhow!("aborting due to {} lexer error(s)", errs.len())
    })?;

    let ParsePhase {
        mut program,
        outcome: parse,
    } = parse_phase(tokens);
    diagnostics::emit_all(&parse.diagnostics, map);

    let ImportPhase {
        mut modules,
        item_imports,
        outcome: imports,
    } = import_phase(&program, root);
    diagnostics::emit_all(&imports.diagnostics, map);

    let desugar = desugar_phase(&mut program, &mut modules);
    diagnostics::emit_all(&desugar.diagnostics, map);

    let TypecheckPhase {
        checker,
        error_count: typecheck_error_count,
    } = typecheck_phase(&program, &modules, &item_imports)?;
    diagnostics::emit_all(&checker.errors, map);

    let concurrency = concurrency_phase(&program, &modules, &item_imports);
    diagnostics::emit_all(&concurrency.entry_diagnostics, map);
    for module_diagnostics in &concurrency.module_diagnostics {
        let module_map = diagnostics::SourceMap::new(
            module_diagnostics.path.clone(),
            module_diagnostics.source.clone(),
        );
        diagnostics::emit_all(&module_diagnostics.diagnostics, &module_map);
    }

    let entry = PhaseDiagnostics::new(validate_entry_point(&program));
    diagnostics::emit_all(&entry.diagnostics, map);

    let error_count = parse.error_count
        + imports.error_count
        + desugar.error_count
        + typecheck_error_count
        + concurrency.error_count
        + entry.error_count;
    if error_count > 0 {
        anyhow::bail!("aborting due to {} error(s)", error_count);
    }

    Ok(Frontend {
        program,
        modules,
        item_imports,
        checker,
    })
}

/// Lexing is the only hard-stop front-end phase: parsing cannot proceed
/// without a token stream.
fn lex_phase(
    source: &str,
) -> std::result::Result<Vec<lexer::token::Token>, Vec<diagnostics::Diagnostic>> {
    lexer::Lexer::new(source).tokenize()
}

/// Parse into a partial AST and retain all parser diagnostics for downstream
/// aggregation.
fn parse_phase(tokens: Vec<lexer::token::Token>) -> ParsePhase {
    let (program, diagnostics) = parser::Parser::new(tokens).parse();
    ParsePhase {
        program,
        outcome: PhaseDiagnostics::new(diagnostics),
    }
}

/// Resolve imports while preserving diagnostics. Failed import resolution
/// yields no modules or item bindings, matching the previous pipeline policy.
fn import_phase(program: &parser::ast::Program, root: &std::path::Path) -> ImportPhase {
    let resolution = module::resolve_imports(program, root);
    let outcome = PhaseDiagnostics::new(resolution.diagnostics);
    let (modules, item_imports) = if outcome.error_count == 0 {
        (resolution.modules, resolution.item_imports)
    } else {
        (vec![], vec![])
    };
    ImportPhase {
        modules,
        item_imports,
        outcome,
    }
}

/// Compose interface inheritance and inject default methods across the entry
/// program and all imported modules.
fn desugar_phase(
    program: &mut parser::ast::Program,
    modules: &mut [module::ResolvedModule],
) -> PhaseDiagnostics {
    let iface_index = build_module_iface_index(modules);
    let default_index = build_module_default_methods(modules, &iface_index);
    let entry_ifaces = augment_index_with_import_aliases(&iface_index, &program.imports);
    let entry_defaults = augment_index_with_import_aliases(&default_index, &program.imports);

    let mut diagnostics = resolve_interface_inheritance(program, &entry_ifaces);
    for module in modules.iter_mut() {
        let module_ifaces =
            augment_index_with_import_aliases(&iface_index, &module.program.imports);
        diagnostics.extend(resolve_interface_inheritance(
            &mut module.program,
            &module_ifaces,
        ));
    }

    diagnostics.extend(inject_default_interface_methods(program, &entry_defaults));
    for module in modules.iter_mut() {
        let module_defaults =
            augment_index_with_import_aliases(&default_index, &module.program.imports);
        diagnostics.extend(inject_default_interface_methods(
            &mut module.program,
            &module_defaults,
        ));
    }
    PhaseDiagnostics::new(diagnostics)
}

/// Register prelude/module symbols and type-check the entry program.
fn typecheck_phase(
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    item_imports: &[module::resolver::ItemImport],
) -> Result<TypecheckPhase> {
    let mut checker = semantic::TypeChecker::new();
    if data_race_check_enabled_from_env() {
        checker.set_enforce_send_sync(true);
    }
    register_prelude(&mut checker)?;
    for m in modules {
        checker.register_module(&m.name, &m.path.to_string_lossy(), &m.program);
        if item_imports.iter().any(|item| {
            item.canonical_module == m.canonical_path && item.canonical_module != m.name
        }) {
            checker.register_module(&m.canonical_path, &m.path.to_string_lossy(), &m.program);
        }
    }
    for item in item_imports {
        checker.register_item_import(&item.local, &item.canonical_module, &item.item, item.span);
    }
    // Seed looping methods of imported classes so a cross-module typed-receiver
    // call (`w.heavy()` where `w: m::Work`) in a task context is flagged E0810
    // (willow-0a6k.2). Keyed by the receiver class name the checker resolves:
    // `module::Class::method` for a whole-module import, `Local::method` for a
    // direct class import.
    let mut module_method_owners: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for m in modules {
        let helpers = semantic::concurrency::compute_nonpreemptible_helpers(&m.program);
        let looping_methods: Vec<&String> = helpers.keys().filter(|k| k.contains("::")).collect();
        for key in &looping_methods {
            // Whole-module access: `name::Class::method`.
            module_method_owners.insert(format!("{}::{}", m.name, key), m.name.clone());
        }
        // Direct class imports re-key `Class::method` under the local name.
        for item in item_imports {
            if item.canonical_module == m.canonical_path {
                let prefix = format!("{}::", item.item);
                for key in &looping_methods {
                    if let Some(rest) = key.strip_prefix(&prefix) {
                        module_method_owners
                            .insert(format!("{}::{}", item.local, rest), m.name.clone());
                    }
                }
            }
        }
    }
    checker.set_nonpreemptible_module_methods(module_method_owners);
    checker.check_program(program);
    let error_count = diagnostic_error_count(&checker.errors);
    Ok(TypecheckPhase {
        checker,
        error_count,
    })
}

/// Run task-aware concurrency checks for the entry program and imported module
/// bodies, retaining each module's source context for later rendering.
fn concurrency_phase(
    program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
    item_imports: &[module::resolver::ItemImport],
) -> ConcurrencyPhase {
    let mut entry_concurrency = semantic::ConcurrencyAnalyzer::new();
    for m in modules {
        entry_concurrency = entry_concurrency.with_module_helpers(&m.name, &m.program);
    }
    // Single-item imports (`import worker::heavy;`) bind a module item under a
    // bare local name; seed it so `heavy()` from an entry async fn is flagged.
    for item in item_imports {
        if let Some(m) = modules
            .iter()
            .find(|m| m.canonical_path == item.canonical_module)
        {
            entry_concurrency = entry_concurrency.with_item_helper(
                &item.local,
                &item.item,
                &item.canonical_module,
                &m.program,
            );
        }
    }
    let entry = entry_concurrency.check_program(program);
    let mut error_count = diagnostic_error_count(&entry.errors);
    let mut module_diagnostics = Vec::new();
    for m in modules {
        let mut module_analyzer = semantic::ConcurrencyAnalyzer::new();
        for import in &m.program.imports {
            if let Some(dep) = modules.iter().find(|d| d.canonical_path == import.path) {
                let access = import.alias.as_deref().unwrap_or_else(|| {
                    import
                        .path
                        .rsplit("::")
                        .next()
                        .unwrap_or(import.path.as_str())
                });
                module_analyzer = module_analyzer.with_module_helpers(access, &dep.program);
            }
        }
        let module = module_analyzer.check_program(&m.program);
        if !module.errors.is_empty() {
            error_count += diagnostic_error_count(&module.errors);
            module_diagnostics.push(ModulePhaseDiagnostics {
                path: m.path.to_string_lossy().into_owned(),
                source: m.source.clone(),
                diagnostics: module.errors,
            });
        }
    }
    ConcurrencyPhase {
        entry_diagnostics: entry.errors,
        module_diagnostics,
        error_count,
    }
}

/// Back-end phases: drive Cranelift codegen over the modules and entry program,
/// emit the object file, resolve the runtime library, link the native
/// executable, and write debug/source-map artifacts.
fn run_backend(
    frontend: Frontend,
    src: &str,
    out: &str,
    source: String,
    opts: &CodegenOptions,
    map: &diagnostics::SourceMap,
) -> Result<()> {
    use diagnostics::{Diagnostic, ErrorCode, Severity};

    let Frontend {
        program,
        modules,
        item_imports,
        checker,
    } = frontend;

    // Codegen — wrap internal errors in a structured diagnostic.
    let mut codegen = backend::Codegen::new(opts).map_err(|e| {
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0800,
            format!("internal compiler error: {e}"),
        );
        diagnostics::emit(&d, map);
        anyhow::anyhow!("internal compiler error")
    })?;

    // register_builtin_generic_enums is now a no-op: all enums (including
    // prelude ones) come from the checker symbol table below.
    codegen.register_builtin_generic_enums();
    // Register all enum infos (prelude + user-declared) for the backend.
    for (name, info) in &checker.symbols.enums {
        codegen.register_enum_info(name.clone(), info.clone());
    }
    // Register interface metadata for vtable codegen + interface dispatch.
    for (name, info) in &checker.symbols.interfaces {
        codegen.register_interface_info(name.clone(), info.clone());
    }
    // Pass type-checker-inferred lambda return types so unannotated lambdas
    // get correct Cranelift signatures (instead of falling back to I64).
    codegen.register_lambda_return_types(checker.lambda_return_types.clone());
    // Full contextual lambda types carry parameter types inferred from expected
    // `fn(...) -> ...` positions.
    codegen.register_lambda_fn_types(checker.lambda_fn_types.clone());
    // Resolved types of async-fn locals, so the backend can frame-back
    // unannotated live-across-await locals (willow-lpn.5c).
    codegen.register_async_local_types(checker.async_local_types.clone());
    // Unqualified enum-variant constructions resolved by the type checker
    // (willow-60o.1), so the backend lowers them as variant allocations.
    codegen.register_enum_variant_resolutions(checker.enum_variant_resolutions.clone());
    codegen.register_pattern_resolutions(checker.pattern_resolutions.clone());

    for m in &modules {
        codegen
            .compile_module(
                &m.name,
                &m.canonical_path,
                &m.program,
                &m.path.to_string_lossy(),
            )
            .map_err(|e| {
                let d = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0800,
                    format!("internal compiler error in module `{}`: {e}", m.name),
                );
                diagnostics::emit(&d, map);
                anyhow::anyhow!("internal compiler error")
            })?;
    }
    // Bind the entry file's single-item imports to the module functions they
    // name, after all modules are compiled (so the mangled symbols exist).
    for item in &item_imports {
        codegen.register_item_import(&item.local, &item.canonical_module, &item.item);
    }
    codegen.compile_program(&program, src).map_err(|e| {
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0800,
            format!("internal compiler error: {e}"),
        );
        diagnostics::emit(&d, map);
        anyhow::anyhow!("internal compiler error")
    })?;

    for warning in codegen.take_async_frame_size_warnings() {
        let warning_source = if warning.source_file == src {
            source.clone()
        } else {
            std::fs::read_to_string(&warning.source_file).unwrap_or_default()
        };
        let warning_map = diagnostics::SourceMap::new(&warning.source_file, &warning_source);
        let point_span = diagnostics::Span::new(
            warning.span.start,
            warning.span.start.saturating_add(1),
            warning.span.line,
            warning.span.col,
        );
        let diagnostic = Diagnostic::new(
            Severity::Warning,
            ErrorCode::W0801,
            format!(
                "async frame for `{}` is large: {} bytes",
                warning.function_name, warning.size_bytes
            ),
        )
        .with_label(diagnostics::Label::primary(
            point_span,
            "large async frame allocated here",
        ))
        .with_help("avoid keeping large arrays or objects live across await points");
        diagnostics::emit(&diagnostic, &warning_map);
    }

    let debug_metadata = if opts.emit_debug_info || opts.emit_source_map {
        Some(debug_source_map_text(map, &program, &modules))
    } else {
        None
    };
    if opts.emit_debug_info {
        codegen
            .embed_runtime_metadata(debug_metadata.as_deref().unwrap_or(""))
            .map_err(|e| {
                let d = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0800,
                    format!("internal compiler error: {e}"),
                );
                diagnostics::emit(&d, map);
                anyhow::anyhow!("internal compiler error")
            })?;
    }

    let obj_bytes = codegen.finish().map_err(|e| {
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0800,
            format!("internal compiler error: {e}"),
        );
        diagnostics::emit(&d, map);
        anyhow::anyhow!("internal compiler error")
    })?;

    let obj_path = if cfg!(all(windows, target_env = "msvc")) {
        format!("{}.obj", out)
    } else {
        format!("{}.o", out)
    };
    std::fs::write(&obj_path, &obj_bytes)?;

    let runtime_lib = resolve_runtime_lib(opts).map_err(|err| {
        let _ = std::fs::remove_file(&obj_path);
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0700,
            format!("runtime library unavailable: {err}"),
        )
        .with_help("build willow_runtime with Cargo or pass --runtime-lib / WILLOW_RUNTIME_LIB");
        diagnostics::emit(&d, map);
        anyhow::anyhow!("runtime library unavailable")
    })?;

    let status = {
        #[cfg(all(windows, target_env = "msvc"))]
        {
            let target = if cfg!(target_arch = "x86_64") {
                "x86_64-pc-windows-msvc"
            } else if cfg!(target_arch = "aarch64") {
                "aarch64-pc-windows-msvc"
            } else if cfg!(target_arch = "x86") {
                "i686-pc-windows-msvc"
            } else {
                anyhow::bail!("unsupported Windows MSVC target architecture");
            };

            let mut cmd = cc::windows_registry::find_tool(target, "cl.exe")
                .ok_or_else(|| anyhow::anyhow!("failed to find MSVC cl.exe"))?
                .to_command();

            cmd.arg("/nologo");
            cmd.arg(&obj_path);
            cmd.arg(runtime_lib.display().to_string());
            cmd.arg("/link");
            cmd.arg(format!("/OUT:{out}"));
            cmd.arg("/SUBSYSTEM:CONSOLE");
            cmd.arg("legacy_stdio_definitions.lib");
            cmd.arg("kernel32.lib");
            cmd.arg("ntdll.lib");
            cmd.arg("userenv.lib");
            cmd.arg("ws2_32.lib");
            cmd.arg("dbghelp.lib");
            cmd.arg("/defaultlib:msvcrt");

            cmd.status()
                .with_context(|| "failed to run MSVC compiler driver")?
        }

        #[cfg(not(all(windows, target_env = "msvc")))]
        {
            let mut link_args = vec![
                obj_path.clone(),
                runtime_lib.display().to_string(),
                "-o".to_string(),
                out.to_string(),
                "-no-pie".to_string(),
                "-lm".to_string(),
                "-lpthread".to_string(),
                "-ldl".to_string(),
            ];

            if opts.strip_symbols {
                link_args.push("-s".to_string());
            }

            Command::new("cc")
                .args(&link_args)
                .status()
                .with_context(|| "failed to run linker")?
        }
    };

    let _ = std::fs::remove_file(&obj_path);

    if !status.success() {
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0700,
            "linking failed: the linker exited with a non-zero status",
        )
        .with_help(format!(
            "check that {} exports the required Willow runtime ABI symbols",
            runtime_lib.display()
        ));
        diagnostics::emit(&d, map);
        anyhow::bail!("linking failed");
    }

    if opts.emit_source_map {
        write_debug_source_maps(out, map, &program, &modules)?;
    } else {
        let _ = std::fs::remove_file(debug_source_map_path(out));
    }

    let mode = if opts.build_mode == BuildMode::Release {
        "release"
    } else {
        "debug"
    };
    eprintln!("compiled [{}]: {}", mode, out);
    Ok(())
}

fn compile(
    src: &str,
    out: &str,
    opts: &CodegenOptions,
    project_root: Option<PathBuf>,
) -> Result<()> {
    CompilerSession::new(src, out, opts, project_root).run()
}

fn diagnostic_error_count(diagnostics: &[diagnostics::Diagnostic]) -> usize {
    diagnostics
        .iter()
        .filter(|diag| diag.severity == diagnostics::Severity::Error)
        .count()
}

#[cfg(test)]
mod frontend_phase_tests {
    use super::*;

    fn parse_source(source: &str) -> parser::ast::Program {
        let tokens = lex_phase(source).expect("test source should lex");
        let parsed = parse_phase(tokens);
        assert_eq!(parsed.outcome.error_count, 0);
        parsed.program
    }

    #[test]
    fn lex_phase_separates_success_from_diagnostics() {
        assert!(lex_phase("fn main() {}").is_ok());
        assert!(lex_phase("fn main() { @ }").is_err());
    }

    #[test]
    fn parse_phase_retains_partial_ast_and_error_count() {
        let tokens = lex_phase("fn good() {} fn broken( {").unwrap();
        let parsed = parse_phase(tokens);
        assert!(!parsed.program.items.is_empty());
        assert!(parsed.outcome.error_count > 0);
    }

    #[test]
    fn import_phase_clears_bindings_after_resolution_error() {
        let program = parse_source("import definitely_missing; fn main() {}");
        let root = std::env::temp_dir().join(format!(
            "willow_frontend_import_phase_{}",
            std::process::id()
        ));
        let imports = import_phase(&program, &root);
        assert!(imports.outcome.error_count > 0);
        assert!(imports.modules.is_empty());
        assert!(imports.item_imports.is_empty());
    }

    #[test]
    fn desugar_phase_reports_its_own_diagnostic_count() {
        let mut program = parse_source("fn main() {}");
        let outcome = desugar_phase(&mut program, &mut []);
        assert_eq!(outcome.error_count, 0);
        assert!(outcome.diagnostics.is_empty());
    }

    #[test]
    fn typecheck_phase_returns_checker_and_error_count() {
        let program = parse_source("fn main() { println(1); }");
        let phase = typecheck_phase(&program, &[], &[]).unwrap();
        assert_eq!(phase.error_count, 0);
        assert!(phase.checker.errors.is_empty());
    }

    #[test]
    fn concurrency_phase_reports_entry_errors_without_rendering() {
        let program =
            parse_source("fn heavy() { while true {} } async fn run() { heavy(); } fn main() {}");
        let phase = concurrency_phase(&program, &[], &[]);
        assert!(phase.error_count > 0);
        assert!(!phase.entry_diagnostics.is_empty());
        assert!(phase.module_diagnostics.is_empty());
    }
}

fn debug_source_map_text(
    entry_map: &diagnostics::SourceMap,
    entry_program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
) -> String {
    let mut text = diagnostics::DebugSourceMap::from_program(
        &entry_map.path,
        entry_map.total_lines(),
        entry_program,
    )
    .to_text();

    for module in modules {
        let module_map =
            diagnostics::SourceMap::new(module.path.to_string_lossy().to_string(), &module.source);
        text.push_str("\n---\n");
        text.push_str(
            &diagnostics::DebugSourceMap::from_program(
                &module_map.path,
                module_map.total_lines(),
                &module.program,
            )
            .to_text(),
        );
    }

    text
}

fn write_debug_source_maps(
    out: &str,
    entry_map: &diagnostics::SourceMap,
    entry_program: &parser::ast::Program,
    modules: &[module::ResolvedModule],
) -> Result<()> {
    std::fs::write(
        debug_source_map_path(out),
        debug_source_map_text(entry_map, entry_program, modules),
    )?;
    Ok(())
}

fn debug_source_map_path(out: &str) -> String {
    format!("{out}.wsmap")
}

fn resolve_runtime_lib(opts: &CodegenOptions) -> Result<PathBuf> {
    if let Some(path) = &opts.runtime_lib {
        return validate_runtime_lib_path(path);
    }

    if let Some(path) = std::env::var_os("WILLOW_RUNTIME_LIB") {
        return validate_runtime_lib_path(PathBuf::from(path));
    }

    let path = default_runtime_lib_path(opts);
    build_default_runtime_lib(opts)?;
    validate_runtime_lib_path(path)
}

fn validate_runtime_lib_path(path: impl Into<PathBuf>) -> Result<PathBuf> {
    let path = path.into();
    if path.is_file() {
        Ok(path)
    } else {
        anyhow::bail!("{} does not exist or is not a file", path.display())
    }
}

fn default_runtime_lib_path(opts: &CodegenOptions) -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("target"));
    let profile = if opts.build_mode == BuildMode::Release {
        "release"
    } else {
        "debug"
    };
    target_dir.join(profile).join(if cfg!(target_env = "msvc") {
        "willow_runtime.lib"
    } else {
        "libwillow_runtime.a"
    })
}

fn build_default_runtime_lib(opts: &CodegenOptions) -> Result<()> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut args = vec![
        "build".to_string(),
        "-p".to_string(),
        "willow_runtime".to_string(),
    ];
    if opts.build_mode == BuildMode::Release {
        args.push("--release".to_string());
    }

    let status = Command::new("cargo")
        .args(&args)
        .current_dir(&manifest_dir)
        .status()
        .with_context(|| "failed to run cargo to build willow_runtime")?;

    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("cargo failed to build willow_runtime")
    }
}

fn validate_entry_point(program: &parser::ast::Program) -> Vec<diagnostics::Diagnostic> {
    use diagnostics::{Diagnostic, ErrorCode, Label, Severity};
    use parser::ast::{Item, Type};

    let mains = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Function(f) if f.name == "main" => Some(f),
            _ => None,
        })
        .collect::<Vec<_>>();

    if mains.is_empty() {
        return vec![
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E1303,
                "missing entry point `main`",
            )
            .with_help("define an entry point: `fn main() { ... }`"),
        ];
    }

    let mut errors = Vec::new();
    if let Some(first) = mains.first() {
        for duplicate in mains.iter().skip(1) {
            errors.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1302,
                    "duplicate entry point `main`",
                )
                .with_label(Label::primary(
                    duplicate.span,
                    "duplicate `main` defined here",
                ))
                .with_label(Label::secondary(first.span, "first `main` defined here"))
                .with_help("keep exactly one top-level `fn main`"),
            );
        }
    }

    let std_collections_module_imported = program.imports.iter().any(|import| {
        import.alias.is_none()
            && module::std_registry::is_std_path(&import.path)
            && matches!(
                module::std_registry::resolve_std_import(&import.path, import.span),
                Ok(module::std_registry::StdImport::Module { module }) if module == "collections"
            )
    });

    for main in mains {
        let valid_args = match main.params.as_slice() {
            [] => true,
            [param] => is_main_args_type(&param.ty, std_collections_module_imported),
            _ => false,
        };
        // `main` may return `void` or `Result<void, E>` (willow-exg). A
        // Result-returning main exits 0 on Ok and prints + exits non-zero on Err.
        let valid_return = main.return_type == Type::Void
            || matches!(
                &main.return_type,
                Type::Generic(name, args)
                    if name == "Result" && args.len() == 2 && args[0] == Type::Void
            );

        if !valid_args || !valid_return {
            errors.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1301,
                    "invalid entry point signature for `main`",
                )
                .with_label(Label::primary(
                    main.span,
                    "expected `fn main()` or `fn main(args: Array<String>)`",
                ))
                .with_help("use `fn main() { ... }` or `fn main(args: Array<String>) { ... }`"),
            );
        }
    }

    errors
}

fn is_main_args_type(ty: &parser::ast::Type, std_collections_module_imported: bool) -> bool {
    use parser::ast::Type;

    match ty {
        Type::Array(element) => **element == Type::String,
        Type::Generic(name, args) if std_collections_module_imported => {
            name == "collections::Array" && matches!(args.as_slice(), [Type::String])
        }
        _ => false,
    }
}
