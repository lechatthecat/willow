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
            eprintln!("  willowc build <source.wi> [-o <output>] [--debug|--release]");
            eprintln!("  willowc run   <source.wi> [--debug|--release]");
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
    let src = args
        .iter()
        .find(|a| a.ends_with(".wi"))
        .ok_or_else(|| anyhow::anyhow!("no source file specified"))?;
    let out = format!("/tmp/willow_run_{}", stem(src));
    let opts = parse_build_mode(args);
    compile(src, &out, &opts, None)?;
    let status = Command::new(&out)
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
        CodegenOptions::release()
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
        checker.register_module(&m.name, &m.program);
    }
    checker.check_program(&program);
    diagnostics::emit_all(&checker.errors, &map);

    // Abort if any errors were found across all stages.
    let error_count = parse_errors.len() + import_errors.len() + checker.errors.len();
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

    let mode = if opts.build_mode == BuildMode::Release {
        "release"
    } else {
        "debug"
    };
    eprintln!("compiled [{}]: {}", mode, out);
    Ok(())
}

fn write_runtime_obj(opts: &CodegenOptions, out: &str) -> Result<String> {
    let runtime_c = r#"
#include <stdio.h>
#include <stdlib.h>
void willow_print_i64(long long v)       { printf("%lld", v); }
void willow_println_i64(long long v)     { printf("%lld\n", v); }
void willow_print_bool(unsigned char v)  { printf("%s", v ? "true" : "false"); }
void willow_println_bool(unsigned char v){ printf("%s\n", v ? "true" : "false"); }
void willow_print_f64(double v)          { printf("%g", v); }
void willow_println_f64(double v)        { printf("%g\n", v); }
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
