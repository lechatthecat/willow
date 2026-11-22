use super::request::{Args, OutputFormat};
use anyhow::{Context, Result, ensure};
use std::{collections::HashSet, path::PathBuf};
use willow_compiler::{
    CompilerOptions, CompilerSession,
    ai::{
        direct::DirectSession,
        overview::{KINDS, Overview},
    },
    project,
};

pub(super) struct Options {
    target: PathBuf,
    depth: Option<u8>,
    pub(super) format: OutputFormat,
    max_chars: usize,
    all: bool,
    absolute: bool,
    kind: Option<String>,
}
impl Options {
    pub(super) fn parse(args: Args<'_, '_>) -> Result<Self> {
        let mut result = Self {
            target: ".".into(),
            depth: None,
            format: OutputFormat::Human,
            max_chars: 12000,
            all: false,
            absolute: false,
            kind: None,
        };
        let mut seen = HashSet::new();
        while let Some(arg) = args.next() {
            let (key, inline) = super::request::split_option(arg);
            if !key.starts_with('-') {
                ensure!(seen.insert("path"), "only one overview path is allowed");
                result.target = arg.clone().into();
            } else {
                ensure!(seen.insert(key), "duplicate option {key}");
                match key {
                    "--all" | "--absolute-paths" => {
                        ensure!(inline.is_none(), "unexpected value for {key}");
                        if key == "--all" {
                            result.all = true
                        } else {
                            result.absolute = true
                        }
                    }
                    "--depth" | "--format" | "--max-chars" | "--kind" => {
                        let value = if let Some(v) = inline {
                            v
                        } else {
                            args.next().context("missing option value")?
                        };
                        match key {
                            "--depth" => {
                                ensure!(matches!(value, "0" | "1"), "depth must be 0 or 1");
                                result.depth = Some(value.parse()?);
                            }
                            "--format" => {
                                result.format = OutputFormat::parse(value)
                                    .context("format must be human, json or ndjson")?;
                            }
                            "--max-chars" => {
                                result.max_chars = value
                                    .parse()
                                    .context("max-chars must be a positive integer")?;
                                ensure!(result.max_chars > 0, "max-chars must be positive");
                            }
                            _ => {
                                ensure!(KINDS.contains(&value), "unknown overview kind {value}");
                                result.kind = Some(value.into());
                            }
                        }
                    }
                    _ => anyhow::bail!("unknown option {key}"),
                }
            }
        }
        Ok(result)
    }
    fn execute(self) -> Result<(String, i32)> {
        let target = std::fs::canonicalize(&self.target).context("overview target not found")?;
        let directory = target.is_dir();
        ensure!(
            directory || target.extension().is_some_and(|e| e == "wi"),
            "overview requires a .wi file or project directory"
        );
        let depth = self.depth.unwrap_or(if directory { 0 } else { 1 });
        let discovery = project::find_project_manifest(if directory {
            &target
        } else {
            target.parent().unwrap()
        });
        let (entry, root) = match discovery {
            Some((manifest, root)) => {
                let manifest = project::ProjectManifest::load(&manifest)?;
                (manifest.entry_point(&root), Some(root))
            }
            None => {
                ensure!(!directory, "directory overview requires a Willow project");
                (target.clone(), None)
            }
        };
        let display_target = if self.absolute {
            target.as_path()
        } else {
            target
                .strip_prefix(root.as_deref().unwrap_or_else(|| target.parent().unwrap()))
                .unwrap_or(&target)
        };
        let display_target = if display_target.as_os_str().is_empty() {
            ".".into()
        } else {
            display_target.to_string_lossy().replace('\\', "/")
        };
        let mut emitter = ErrorCount::default();
        let snapshot = CompilerSession::new(
            entry.to_str().context("non UTF-8 entry")?,
            "",
            &CompilerOptions::debug(),
            root,
        )
        .overview_with_emitter(&mut emitter);
        let overview = match snapshot {
            Ok(snapshot) => Overview::project(
                &DirectSession::new(snapshot)?,
                &target,
                depth,
                self.max_chars,
                self.kind.as_deref(),
                self.absolute,
            ),
            Err(_) if emitter.errors > 0 => {
                Overview::empty(display_target, depth, self.max_chars, emitter.errors)
            }
            Err(error) => return Err(error),
        };
        ensure!(
            directory || !overview.files.is_empty() || overview.status != "ok",
            "overview target is outside analyzed source graph: {}",
            target.display()
        );
        let code = i32::from(overview.status != "ok");
        Ok((overview.render(self.format.as_str(), self.all)?, code))
    }
}
#[derive(Default)]
struct ErrorCount {
    errors: usize,
}
impl willow_compiler::diagnostics::DiagnosticEmitter for ErrorCount {
    fn emit(
        &mut self,
        diagnostic: &willow_compiler::diagnostics::Diagnostic,
        _: &dyn willow_compiler::diagnostics::source_map::SourceLookup,
    ) -> std::io::Result<()> {
        if diagnostic.severity == willow_compiler::diagnostics::Severity::Error {
            self.errors += 1;
        }
        Ok(())
    }
}
pub(super) fn run(parsed: Result<Options>, human_errors: bool) -> Result<i32> {
    let human_errors = parsed.as_ref().map_or(human_errors, |options| {
        options.format == OutputFormat::Human
    });
    let result = parsed
        .map_err(|e| (2, e))
        .and_then(|o| o.execute().map_err(|e| (1, e)));
    match result {
        Ok((text, code)) => {
            print!("{text}");
            Ok(code)
        }
        Err((code, error)) => {
            if human_errors {
                eprintln!("{error:#}");
            } else {
                println!(
                    "{}",
                    serde_json::json!({"schema":1,"kind":"overview","status":if code == 2 {"invalid-arguments"} else {"error"},"message":format!("{error:#}")})
                );
            }
            Ok(code)
        }
    }
}
