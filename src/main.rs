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

/// Parse the prelude source and register its declarations with the type checker.
/// Resolve interface inheritance (willow-1js.2) by desugaring on the AST:
///  1. Compose each interface's method list as `[super methods..., own methods]`
///     (transitively, deduped by name; an own method overrides an inherited one
///     in place, preserving slot order so a sub-interface vtable stays layout-
///     compatible with its super's).
///  2. Expand each class's `implements` clause with the transitive super-
///     interfaces of every interface it implements, so the class is usable as
///     (and gets a vtable for) each super, and conformance covers the full set.
/// Only interfaces defined in this program are considered (cross-module
/// inheritance is future work). Must run BEFORE default-method injection.
fn resolve_interface_inheritance(program: &mut parser::ast::Program) {
    use parser::ast::{InterfaceMethodDecl, Item, Type, TypePath};
    use std::collections::{HashMap, HashSet};

    // name -> (direct supers, own methods)
    let snapshot: HashMap<String, (Vec<String>, Vec<InterfaceMethodDecl>)> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Interface(i) => Some((i.name.clone(), (i.extends.clone(), i.methods.clone()))),
            _ => None,
        })
        .collect();

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
    let no_iface_inheritance = snapshot.values().all(|(ext, _)| ext.is_empty());
    let no_class_inheritance = class_info.values().all(|(base, _)| base.is_none());
    if no_iface_inheritance && no_class_inheritance {
        return;
    }

    // Full method list for `name`: supers (in order, transitively) then own;
    // an own/earlier method of the same name overrides a later inherited one
    // in place. `visiting` guards against extends-cycles.
    fn compose(
        name: &str,
        snap: &HashMap<String, (Vec<String>, Vec<InterfaceMethodDecl>)>,
        visiting: &mut HashSet<String>,
    ) -> Vec<InterfaceMethodDecl> {
        let mut out: Vec<InterfaceMethodDecl> = Vec::new();
        if !visiting.insert(name.to_string()) {
            return out; // cycle: stop recursing
        }
        if let Some((extends, own)) = snap.get(name) {
            for sup in extends {
                for m in compose(sup, snap, visiting) {
                    upsert(&mut out, m);
                }
            }
            for m in own {
                upsert(&mut out, m.clone());
            }
        }
        visiting.remove(name);
        out
    }
    // Insert `m`, or replace an existing same-named method in place.
    fn upsert(out: &mut Vec<InterfaceMethodDecl>, m: InterfaceMethodDecl) {
        if let Some(existing) = out.iter_mut().find(|e| e.name == m.name) {
            *existing = m;
        } else {
            out.push(m);
        }
    }
    // Transitive super-interface names of `name`.
    fn all_supers(
        name: &str,
        snap: &HashMap<String, (Vec<String>, Vec<InterfaceMethodDecl>)>,
        visiting: &mut HashSet<String>,
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
                all_supers(sup, snap, visiting, out);
            }
        }
        visiting.remove(name);
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

    let composed: HashMap<String, Vec<InterfaceMethodDecl>> = snapshot
        .keys()
        .map(|n| (n.clone(), compose(n, &snapshot, &mut HashSet::new())))
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
                    all_supers(&iface, &snapshot, &mut HashSet::new(), &mut supers);
                    for sup in supers {
                        if implemented.insert(sup.clone()) {
                            c.implements.push(Type::Named(sup));
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Inject default interface methods (willow-1js.3): for each class, for each
/// interface it implements that defines a method with a default body, if the
/// class does not already declare a method of that name, synthesize a class
/// method whose body is the default. `self` then refers to the concrete class,
/// so sibling interface calls dispatch normally. Only interfaces defined in this
/// program are considered (cross-module default methods are future work).
fn inject_default_interface_methods(program: &mut parser::ast::Program) {
    use parser::ast::{Item, MethodDecl, Type};
    use std::collections::{HashMap, HashSet};

    // interface name -> its default (body-carrying) methods.
    let mut defaults: HashMap<String, Vec<parser::ast::InterfaceMethodDecl>> = HashMap::new();
    for item in &program.items {
        if let Item::Interface(iface) = item {
            let with_body: Vec<_> = iface
                .methods
                .iter()
                .filter(|m| m.default_body.is_some())
                .cloned()
                .collect();
            if !with_body.is_empty() {
                defaults.insert(iface.name.clone(), with_body);
            }
        }
    }
    if defaults.is_empty() {
        return;
    }

    for item in &mut program.items {
        let Item::Class(class) = item else { continue };
        let mut present: HashSet<String> = class.methods.iter().map(|m| m.name.clone()).collect();
        let mut injected: Vec<MethodDecl> = Vec::new();
        for iface_ty in &class.implements {
            let iface_name = match iface_ty {
                Type::Named(n) | Type::Generic(n, _) => n,
                _ => continue,
            };
            let Some(methods) = defaults.get(iface_name) else {
                continue;
            };
            for dm in methods {
                // Skip if the class overrides it, or another interface's default
                // of the same name was already injected (first one wins).
                if present.contains(&dm.name) {
                    continue;
                }
                let Some(body) = &dm.default_body else {
                    continue;
                };
                present.insert(dm.name.clone());
                injected.push(MethodDecl {
                    name: dm.name.clone(),
                    public: true, // interface methods are public by contract
                    protected: false,
                    is_async: false,
                    is_open: false,
                    is_override: false,
                    is_static: false,
                    params: dm.params.clone(),
                    has_self: dm.has_self,
                    return_type: dm.return_type.clone(),
                    body: body.clone(),
                    span: dm.span,
                });
            }
        }
        class.methods.extend(injected);
    }
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

fn compile(
    src: &str,
    out: &str,
    opts: &CodegenOptions,
    project_root: Option<PathBuf>,
) -> Result<()> {
    use diagnostics::{Diagnostic, ErrorCode, Severity};

    let src_path = PathBuf::from(src);
    let source = std::fs::read_to_string(&src_path)
        .with_context(|| format!("cannot read {}", src_path.display()))?;

    // Import resolution root: the directory containing the source file.
    let _ = project_root; // available for future use (e.g. package search paths)
    let root = src_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let map = diagnostics::SourceMap::new(src, &source);

    // Lex — hard stop: without tokens there is nothing to parse.
    let tokens = lexer::Lexer::new(&source).tokenize().map_err(|errs| {
        diagnostics::emit_all(&errs, &map);
        anyhow::anyhow!("aborting due to {} lexer error(s)", errs.len())
    })?;

    // Parse — collect errors but continue with the partial AST so downstream
    // stages can surface additional independent diagnostics.
    let (mut program, parse_errors) = parser::Parser::new(tokens).parse();
    diagnostics::emit_all(&parse_errors, &map);

    // Desugar interface inheritance first (compose super methods into each
    // interface + expand classes' implements with transitive supers), then
    // default methods — so a class inherits defaults declared on a super
    // interface too (willow-1js.2, willow-1js.3).
    resolve_interface_inheritance(&mut program);
    inject_default_interface_methods(&mut program);

    // Resolve imports — collect errors but do not abort; we can still type-check
    // items that do not depend on the failed imports.
    let import_resolution = module::resolve_imports(&program, &root);
    diagnostics::emit_all(&import_resolution.diagnostics, &map);
    let import_error_count = diagnostic_error_count(&import_resolution.diagnostics);
    let (modules, item_imports) = if import_error_count == 0 {
        (import_resolution.modules, import_resolution.item_imports)
    } else {
        (vec![], vec![])
    };

    // Type check — register prelude first, then imported modules, then the
    // entry file's single-item imports, then the entry program.
    let mut checker = semantic::TypeChecker::new();
    register_prelude(&mut checker)?;
    for m in &modules {
        checker.register_module(&m.name, &m.path.to_string_lossy(), &m.program);
        if item_imports.iter().any(|item| {
            item.canonical_module == m.canonical_path && item.canonical_module != m.name
        }) {
            checker.register_module(&m.canonical_path, &m.path.to_string_lossy(), &m.program);
        }
    }
    for item in &item_imports {
        checker.register_item_import(&item.local, &item.canonical_module, &item.item, item.span);
    }
    checker.check_program(&program);
    diagnostics::emit_all(&checker.errors, &map);

    let concurrency = semantic::ConcurrencyAnalyzer::new().check_program(&program);
    diagnostics::emit_all(&concurrency.errors, &map);

    let entry_errors = validate_entry_point(&program);
    diagnostics::emit_all(&entry_errors, &map);

    // Abort if any errors were found across all stages.
    let error_count = diagnostic_error_count(&parse_errors)
        + import_error_count
        + diagnostic_error_count(&checker.errors)
        + diagnostic_error_count(&concurrency.errors)
        + diagnostic_error_count(&entry_errors);
    if error_count > 0 {
        anyhow::bail!("aborting due to {} error(s)", error_count);
    }

    // Codegen — wrap internal errors in a structured diagnostic.
    let mut codegen = backend::Codegen::new(opts).map_err(|e| {
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0800,
            format!("internal compiler error: {e}"),
        );
        diagnostics::emit(&d, &map);
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
    // Resolved types of async-fn locals, so the backend can frame-back
    // unannotated live-across-await locals (willow-lpn.5c).
    codegen.register_async_local_types(checker.async_local_types.clone());

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
                diagnostics::emit(&d, &map);
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
        diagnostics::emit(&d, &map);
        anyhow::anyhow!("internal compiler error")
    })?;

    let debug_metadata = if opts.emit_debug_info || opts.emit_source_map {
        Some(debug_source_map_text(&map, &program, &modules))
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
                diagnostics::emit(&d, &map);
                anyhow::anyhow!("internal compiler error")
            })?;
    }

    let obj_bytes = codegen.finish().map_err(|e| {
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0800,
            format!("internal compiler error: {e}"),
        );
        diagnostics::emit(&d, &map);
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
        diagnostics::emit(&d, &map);
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
        diagnostics::emit(&d, &map);
        anyhow::bail!("linking failed");
    }

    if opts.emit_source_map {
        write_debug_source_maps(out, &map, &program, &modules)?;
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

fn diagnostic_error_count(diagnostics: &[diagnostics::Diagnostic]) -> usize {
    diagnostics
        .iter()
        .filter(|diag| diag.severity == diagnostics::Severity::Error)
        .count()
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
