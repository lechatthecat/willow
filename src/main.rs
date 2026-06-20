use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;
use willow_compiler::{CodegenOptions, compile, project};

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
