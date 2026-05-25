mod backend;
mod diagnostics;
mod ir;
mod lexer;
mod module;
mod parser;
mod project;
mod runtime;
mod semantic;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub enum BuildMode {
    Debug,
    Release,
}

pub struct CodegenOptions {
    pub build_mode: BuildMode,
    pub emit_debug_info: bool,
    pub emit_source_map: bool,
    pub strip_symbols: bool,
}

impl CodegenOptions {
    pub fn debug() -> Self {
        Self {
            build_mode: BuildMode::Debug,
            emit_debug_info: true,
            emit_source_map: true,
            strip_symbols: false,
        }
    }

    pub fn release() -> Self {
        Self {
            build_mode: BuildMode::Release,
            emit_debug_info: false,
            emit_source_map: false,
            strip_symbols: false,
        }
    }

    pub fn release_with_debug_info() -> Self {
        Self {
            build_mode: BuildMode::Release,
            emit_debug_info: true,
            emit_source_map: true,
            strip_symbols: false,
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
            compile(src, &out, &CodegenOptions::debug(), None)
        }
        _ => {
            eprintln!("Usage:");
            eprintln!(
                "  willowc build <source.wi> [-o <output>] [--debug|--release] [--debug-info]"
            );
            eprintln!("  willowc run   <source.wi> [--debug|--release] [--debug-info]");
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
    let out = format!("/tmp/willow_run_{}", stem(src));
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
    let out = format!("/tmp/willow_debug_{}", stem(src));
    compile(src, &out, &CodegenOptions::debug(), None)?;
    eprintln!("note: interactive debugger not yet implemented");
    eprintln!("running in debug mode: {}", out);
    let status = Command::new(&out)
        .status()
        .with_context(|| format!("failed to run {}", out))?;
    std::process::exit(status.code().unwrap_or(0));
}

fn parse_build_mode(args: &[String]) -> CodegenOptions {
    if args.iter().any(|a| a == "--release") {
        if args.iter().any(|a| a == "--debug-info") {
            CodegenOptions::release_with_debug_info()
        } else {
            CodegenOptions::release()
        }
    } else {
        CodegenOptions::debug()
    }
}

fn stem(path: &str) -> String {
    PathBuf::from(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("a")
        .to_string()
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
    let (program, parse_errors) = parser::Parser::new(tokens).parse();
    diagnostics::emit_all(&parse_errors, &map);

    // Resolve imports — collect errors but do not abort; we can still type-check
    // items that do not depend on the failed imports.
    let (modules, import_errors) = match module::resolve_imports(&program, &root) {
        Ok(mods) => (mods, vec![]),
        Err(errs) => {
            diagnostics::emit_all(&errs, &map);
            (vec![], errs)
        }
    };

    // Type check — register imported modules first, then check the entry program.
    let mut checker = semantic::TypeChecker::new();
    for m in &modules {
        checker.register_module(&m.name, &m.path.to_string_lossy(), &m.program);
    }
    checker.check_program(&program);
    diagnostics::emit_all(&checker.errors, &map);

    let concurrency = semantic::ConcurrencyAnalyzer::new().check_program(&program);
    diagnostics::emit_all(&concurrency.errors, &map);

    let entry_errors = validate_entry_point(&program);
    diagnostics::emit_all(&entry_errors, &map);

    // Abort if any errors were found across all stages.
    let error_count = parse_errors.len()
        + import_errors.len()
        + checker.errors.len()
        + concurrency.errors.len()
        + entry_errors.len();
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

    for m in &modules {
        codegen.compile_module(&m.name, &m.program).map_err(|e| {
            let d = Diagnostic::new(
                Severity::Error,
                ErrorCode::E0800,
                format!("internal compiler error in module `{}`: {e}", m.name),
            );
            diagnostics::emit(&d, &map);
            anyhow::anyhow!("internal compiler error")
        })?;
    }
    codegen.compile_program(&program).map_err(|e| {
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

    let obj_path = format!("{}.o", out);
    std::fs::write(&obj_path, &obj_bytes)?;

    let runtime_obj = write_runtime_obj(opts, out)?;

    // Link — wrap failure in a structured diagnostic.
    let mut link_args = vec![
        obj_path.clone(),
        runtime_obj.clone(),
        "-o".to_string(),
        out.to_string(),
        // Cranelift emits absolute relocations; disable PIE so the linker
        // does not need DT_TEXTREL in a read-only .text section.
        "-no-pie".to_string(),
        "-lm".to_string(),
    ];
    if opts.strip_symbols {
        link_args.push("-s".to_string());
    }
    let status = Command::new("cc")
        .args(&link_args)
        .status()
        .with_context(|| "failed to run linker")?;

    let _ = std::fs::remove_file(&obj_path);
    let _ = std::fs::remove_file(&runtime_obj);

    if !status.success() {
        let d = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0700,
            "linking failed: the linker exited with a non-zero status",
        )
        .with_help("check that all required symbols are defined and the linker is installed");
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

fn write_runtime_obj(opts: &CodegenOptions, out: &str) -> Result<String> {
    let runtime_c = r#"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
void willow_print_i64(long long v)       { printf("%lld", v); }
void willow_println_i64(long long v)     { printf("%lld\n", v); }
void willow_print_bool(unsigned char v)  { printf("%s", v ? "true" : "false"); }
void willow_println_bool(unsigned char v){ printf("%s\n", v ? "true" : "false"); }
static void f64_format(double v, char* buf, size_t len) {
    double rt; int p;
    if (v != v)            { snprintf(buf, len, "NaN");  return; }
    if (v ==  1.0/0.0)     { snprintf(buf, len, "inf");  return; }
    if (v == -1.0/0.0)     { snprintf(buf, len, "-inf"); return; }
    for (p = 1; p <= 17; p++) {
        snprintf(buf, len, "%.*g", p, v);
        sscanf(buf, "%lf", &rt);
        if (rt == v) break;
    }
    /* If the shortest representation uses scientific notation, try one more
       significant digit — %g may switch to fixed notation which is preferred
       when it also round-trips (e.g. 10.0: "1e+01" at p=1 -> "10" at p=2). */
    if (strchr(buf, 'e') || strchr(buf, 'E')) {
        char alt[32]; double rt2;
        snprintf(alt, sizeof(alt), "%.*g", p + 1, v);
        sscanf(alt, "%lf", &rt2);
        if (rt2 == v && !strchr(alt, 'e') && !strchr(alt, 'E'))
            snprintf(buf, len, "%s", alt);
    }
}
static void f64_write(double v, int nl) {
    char buf[32];
    f64_format(v, buf, sizeof(buf));
    printf(nl ? "%s\n" : "%s", buf);
}
void willow_print_f64(double v)          { f64_write(v, 0); }
void willow_println_f64(double v)        { f64_write(v, 1); }
double willow_pow_f64(double base, double exp) { return pow(base, exp); }
static char* willow_copy_string(const char* value) {
    size_t len = strlen(value);
    char* out = (char*)malloc(len + 1);
    if (!out) { abort(); }
    memcpy(out, value, len + 1);
    return out;
}
char* willow_f64_to_string(double v) {
    char buf[32];
    f64_format(v, buf, sizeof(buf));
    return willow_copy_string(buf);
}
char* willow_format_f64_17g(double v) {
    char buf[64];
    snprintf(buf, sizeof(buf), "%.17g", v);
    return willow_copy_string(buf);
}
char* willow_format_f64_16f(double v) {
    char buf[128];
    snprintf(buf, sizeof(buf), "%.16f", v);
    return willow_copy_string(buf);
}
char* willow_format_f64_6f(double v) {
    char buf[64];
    snprintf(buf, sizeof(buf), "%.6f", v);
    return willow_copy_string(buf);
}
void willow_print_string(const char* v)  { printf("%s", v ? v : "(null)"); }
void willow_println_string(const char* v){ printf("%s\n", v ? v : "(null)"); }
static int willow_runtime_argc_value = 0;
static char** willow_runtime_argv_value = NULL;
static int willow_runtime_user_argc_value = 0;
static char** willow_runtime_user_argv_value = NULL;
/* ── Garbage Collector ─────────────────────────────────────────────────────
 * Stop-the-world mark-and-sweep.
 * Object layout: [WillowGcHeader | payload bytes ...]
 * willow_alloc() returns the payload pointer (past the header).
 * ─────────────────────────────────────────────────────────────────────── */
typedef struct WillowGcHeader {
    unsigned char          marked;
    unsigned int           type_id;
    size_t                 size;   /* total bytes: header + payload */
    struct WillowGcHeader* next;
} WillowGcHeader;

static WillowGcHeader* wgc_head      = NULL;
static size_t          wgc_bytes     = 0;
/* 4 MiB auto-trigger threshold. */
static size_t          wgc_threshold = (size_t)(4 * 1024 * 1024);

#define WILLOW_ROOT_MAX 4096
static void** wgc_roots[WILLOW_ROOT_MAX];
static int    wgc_roots_top = 0;

static WillowGcHeader* wgc_hdr(void* p) {
    return (WillowGcHeader*)((char*)p - sizeof(WillowGcHeader));
}
static void wgc_mark(void* p) {
    if (!p) return;
    WillowGcHeader* h = wgc_hdr(p);
    if (h->marked) return;
    h->marked = 1;
    /* TODO: trace child fields via type_id/tracing metadata */
}
static void wgc_sweep(void) {
    WillowGcHeader** cur = &wgc_head;
    while (*cur) {
        WillowGcHeader* h = *cur;
        if (!h->marked) { *cur = h->next; wgc_bytes -= h->size; free(h); }
        else            { h->marked = 0;  cur = &h->next; }
    }
}
void willow_gc_init(void) {
    wgc_head = NULL; wgc_bytes = 0; wgc_roots_top = 0;
}
void willow_gc_collect(void) {
    int i;
    for (i = 0; i < wgc_roots_top; i++) wgc_mark(*wgc_roots[i]);
    wgc_sweep();
}
void willow_push_root(void** slot) {
    if (wgc_roots_top < WILLOW_ROOT_MAX) wgc_roots[wgc_roots_top++] = slot;
}
void willow_pop_root(void) {
    if (wgc_roots_top > 0) wgc_roots_top--;
}
void willow_pop_roots(int n) {
    wgc_roots_top -= n;
    if (wgc_roots_top < 0) wgc_roots_top = 0;
}
long long willow_gc_allocated_bytes(void) { return (long long)wgc_bytes; }
void* willow_alloc(long long payload_size) {
    size_t total = sizeof(WillowGcHeader) + (size_t)payload_size;
    if (wgc_bytes + total > wgc_threshold) willow_gc_collect();
    WillowGcHeader* h = (WillowGcHeader*)calloc(1, total);
    if (!h) abort();
    h->size = total; h->next = wgc_head;
    wgc_head = h; wgc_bytes += total;
    return (void*)((char*)h + sizeof(WillowGcHeader));
}

extern void willow_user_main(void);
void willow_runtime_store_args(int argc, char** argv) {
    willow_runtime_argc_value = argc;
    willow_runtime_argv_value = argv;
    if (argc > 1) {
        willow_runtime_user_argc_value = argc - 1;
        willow_runtime_user_argv_value = argv + 1;
    } else {
        willow_runtime_user_argc_value = 0;
        willow_runtime_user_argv_value = NULL;
    }
}
long long willow_runtime_args_len(void) { return (long long)willow_runtime_user_argc_value; }
const char* willow_runtime_arg(long long index) {
    if (index < 0 || index >= willow_runtime_user_argc_value || !willow_runtime_user_argv_value) {
        return NULL;
    }
    return willow_runtime_user_argv_value[index];
}
const char* willow_runtime_program_name(void) {
    if (willow_runtime_argc_value <= 0 || !willow_runtime_argv_value) {
        return "";
    }
    return willow_runtime_argv_value[0] ? willow_runtime_argv_value[0] : "";
}
void runtime_start(int argc, char** argv) {
    willow_runtime_store_args(argc, argv);
    willow_gc_init();
    willow_user_main();
}
int main(int argc, char** argv) {
    runtime_start(argc, argv);
    return 0;
}
char* willow_string_concat(const char* lhs, const char* rhs) {
    if (!lhs) { lhs = ""; }
    if (!rhs) { rhs = ""; }
    size_t lhs_len = strlen(lhs);
    size_t rhs_len = strlen(rhs);
    char* out = (char*)malloc(lhs_len + rhs_len + 1);
    if (!out) { abort(); }
    memcpy(out, lhs, lhs_len);
    memcpy(out + lhs_len, rhs, rhs_len);
    out[lhs_len + rhs_len] = '\0';
    return out;
}
void willow_abort(const char* file, int line) {
    fprintf(stderr, "panic at %s:%d\n", file, line);
    abort();
}
"#;
    // Use out path as prefix so parallel compilations don't race on the same tmp file.
    let c_path = format!("{}_runtime.c", out);
    let o_path = format!("{}_runtime.o", out);
    std::fs::write(&c_path, runtime_c)?;

    let mut cc_args = vec![
        "-c".to_string(),
        c_path.clone(),
        "-o".to_string(),
        o_path.clone(),
    ];
    if opts.build_mode == BuildMode::Release {
        cc_args.push("-O3".to_string());
    }

    let status = Command::new("cc")
        .args(&cc_args)
        .status()
        .with_context(|| "failed to compile runtime")?;
    if !status.success() {
        anyhow::bail!("runtime compilation failed");
    }
    Ok(o_path.to_string())
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

    for main in mains {
        let valid_args = match main.params.as_slice() {
            [] => true,
            [param] => matches!(&param.ty, Type::Array(element) if **element == Type::String),
            _ => false,
        };
        let valid_return = main.return_type == Type::Void;

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
