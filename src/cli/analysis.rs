//! CLI syntax only. All semantic analysis belongs to the compiler library.
use super::*;
use serde_json::{Value, json};
use willow_compiler::{
    CompilerSession,
    ai::{Direction, Limits, compact_output_paths},
    diagnostics::DiagnosticEmitter,
};

#[derive(Debug)]
pub(super) struct AnalysisCommand {
    operation: String,
    requests: Option<String>,
    build: BuildCommand,
    file: Option<String>,
    byte: Option<usize>,
    functions: Vec<String>,
    revision: Option<String>,
    direction: Direction,
    limits: Limits,
}
impl AnalysisCommand {
    pub(super) fn parse(operation: &str, args: Args<'_, '_>) -> Result<Self> {
        let mut requests = None;
        let mut file = None;
        let mut byte = None;
        let mut functions = Vec::new();
        let mut revision = None;
        let mut direction = Direction::Callers;
        let mut limits = Limits::default();
        let mut seen = std::collections::HashSet::new();
        // Analysis options have precedence even between a compiler flag and its value.
        let mut analysis_error = None;
        let mut next_compiler_arg = || -> Result<_> {
            while let Some(arg) = args.next() {
                let (key, inline) = super::request::split_option(arg);
                if matches!(
                    key,
                    "--requests"
                        | "--file"
                        | "--byte"
                        | "--function"
                        | "--revision"
                        | "--direction"
                        | "--max-nodes"
                        | "--max-depth"
                ) {
                    anyhow::ensure!(key == "--function" || seen.insert(key), "duplicate {key}");
                    let value = if let Some(value) = inline {
                        value
                    } else {
                        args.next().context("missing analysis option value")?
                    };
                    match key {
                        "--requests" => requests = Some(value.into()),
                        "--file" => file = Some(value.into()),
                        "--byte" => byte = Some(value.parse()?),
                        "--function" => functions.push(value.into()),
                        "--revision" => revision = Some(value.into()),
                        "--max-nodes" => limits.max_nodes = value.parse()?,
                        "--max-depth" => limits.max_depth = value.parse()?,
                        "--direction" => {
                            direction = match value {
                                "callers" => Direction::Callers,
                                "callees" => Direction::Callees,
                                _ => anyhow::bail!("direction must be callers or callees"),
                            }
                        }
                        _ => unreachable!(),
                    }
                } else {
                    return Ok(Some(arg));
                }
            }
            Ok(None)
        };
        let mut compiler_args = std::iter::from_fn(|| {
            if analysis_error.is_some() {
                return None;
            }
            next_compiler_arg().unwrap_or_else(|error| {
                analysis_error = Some(error);
                None
            })
        });
        let build = BuildCommand::parse(&mut compiler_args);
        compiler_args.for_each(drop);
        if let Some(error) = analysis_error {
            return Err(error);
        }
        let build = build?;
        anyhow::ensure!(
            !build.emit_hir && !build.emit_lir && build.output.is_none(),
            "analysis does not generate native/IR artifacts"
        );
        anyhow::ensure!(limits.max_nodes > 0, "max-nodes must be positive");
        match operation {
            "impact" => {
                anyhow::ensure!(
                    byte.is_some() == file.is_some(),
                    "--file and --byte must be specified together"
                );
                anyhow::ensure!(
                    byte.is_some() || !functions.is_empty(),
                    "impact requires --file/--byte or --function"
                );
                anyhow::ensure!(
                    functions.is_empty() || revision.is_some(),
                    "--function requires --revision"
                );
            }
            "query" => anyhow::ensure!(
                requests.is_some()
                    && file.is_none()
                    && functions.is_empty()
                    && byte.is_none()
                    && revision.is_none()
                    && !seen.contains("--direction"),
                "query requires --requests and source/compiler options only"
            ),
            _ => unreachable!(),
        }
        anyhow::ensure!(
            operation == "query" || requests.is_none(),
            "--requests requires query"
        );
        Ok(Self {
            operation: operation.into(),
            requests,
            build,
            file,
            byte,
            functions,
            revision,
            direction,
            limits,
        })
    }
    pub(super) fn execute(self, emitter: &mut dyn DiagnosticEmitter) -> Result<Value> {
        let (entry, root) = if let Some(source) = self.build.source {
            (source, None)
        } else {
            let directory = self.build.project_dir.unwrap_or_else(|| PathBuf::from("."));
            let (manifest, root) =
                project::find_project_manifest(&directory).context("no project.toml found")?;
            let manifest = project::ProjectManifest::load(&manifest)?;
            (
                manifest
                    .entry_point(&root)
                    .to_str()
                    .context("non UTF-8 entry")?
                    .to_string(),
                Some(root),
            )
        };
        let snapshot = CompilerSession::new(&entry, "", &self.build.options, root)
            .analysis_with_emitter(emitter)?;
        if self.operation == "query" {
            let workspace = snapshot.workspace.clone();
            let requests: Vec<serde_json::Value> =
                serde_json::from_slice(&std::fs::read(self.requests.as_ref().unwrap())?)?;
            let mut session = willow_compiler::ai::QuerySession::new(snapshot)?;
            let mut results = Vec::with_capacity(requests.len());
            for mut request in requests {
                if let Some(Value::String(file)) = request.get_mut("file") {
                    expand_request_path(file, &workspace);
                }
                // A missing revision binds to this invocation's immutable snapshot.
                if request.get("revision").is_none() {
                    request
                        .as_object_mut()
                        .context("query must be an object")?
                        .insert("revision".into(), json!(session.revision()));
                }
                results.push(session.query(serde_json::from_value(request)?));
            }
            return Ok(compact_output_paths(
                json!({"kind":"query", "revision":session.revision(),"results":results}),
                &workspace,
            ));
        }
        let mut seeds = self.functions;
        if let Some(mut file) = self.file {
            expand_request_path(&mut file, &snapshot.workspace);
            seeds.push(
                snapshot
                    .resolve_position(
                        std::path::Path::new(&file),
                        self.byte.unwrap(),
                        self.revision.as_deref(),
                    )?
                    .id
                    .clone(),
            );
        }
        let impact = snapshot.impact(
            &seeds,
            self.direction,
            self.limits,
            self.revision.as_deref(),
        )?;
        let ids: std::collections::HashSet<_> =
            impact.nodes.iter().map(|n| n.id.as_str()).collect();
        let functions: Vec<_> = snapshot
            .functions
            .iter()
            .filter(|f| ids.contains(f.id.as_str()))
            .collect();
        Ok(compact_output_paths(
            json!({"kind":"impact", "impact":impact,"functions":functions}),
            &snapshot.workspace,
        ))
    }
}

pub(super) fn expand_request_path(file: &mut String, workspace: &str) {
    // Only position-query paths are paths, not symbol IDs or name filters.
    if let Some(rest) = file.strip_prefix("${workspace}")
        && (rest.is_empty() || rest.starts_with(['/', '\\']))
    {
        *file = format!("{workspace}{rest}");
    }
}

pub(super) const QUERY_COMMAND: &str =
    "willow query . --requests queries.json --format ndjson --protocol-version 1";
pub(super) const IMPACT_COMMAND: &str = "willow impact . --function FUNCTION_ID --revision REVISION --direction callers --format ndjson --protocol-version 1";
