//! CLI syntax only. All semantic analysis belongs to the compiler library.
use super::*;
use serde_json::{Value, json};
use willow_compiler::{
    CompilerSession,
    ai::{Direction, Limits, Snapshot},
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
    output: Option<String>,
    before: Option<String>,
    after: Option<String>,
    direction: Direction,
    limits: Limits,
}
impl AnalysisCommand {
    pub(super) fn parse(args: &[String]) -> Result<Self> {
        let (operation, args) = if matches!(args[0].as_str(), "impact" | "risk" | "query") {
            (args[0].as_str(), &args[1..])
        } else {
            let operation = args
                .get(1)
                .context("snapshot requires save or diff")?
                .as_str();
            anyhow::ensure!(
                matches!(operation, "save" | "diff"),
                "snapshot requires save or diff"
            );
            (operation, &args[2..])
        };
        let mut result = Self {
            operation: operation.into(),
            requests: None,
            build: BuildCommand::parse(&[])?,
            file: None,
            byte: None,
            functions: vec![],
            revision: None,
            output: None,
            before: None,
            after: None,
            direction: Direction::Callers,
            limits: Limits::default(),
        };
        let mut compiler_args = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut i = 0;
        while i < args.len() {
            let (key, inline) = args[i]
                .split_once('=')
                .map_or((args[i].as_str(), None), |(k, v)| (k, Some(v)));
            if matches!(
                key,
                "--requests"
                    | "--file"
                    | "--byte"
                    | "--function"
                    | "--revision"
                    | "--output"
                    | "--before"
                    | "--after"
                    | "--direction"
                    | "--max-nodes"
                    | "--max-depth"
            ) {
                anyhow::ensure!(
                    key == "--function" || seen.insert(key.to_string()),
                    "duplicate {key}"
                );
                let value = if let Some(value) = inline {
                    value
                } else {
                    i += 1;
                    args.get(i).context("missing analysis option value")?
                };
                match key {
                    "--requests" => result.requests = Some(value.into()),
                    "--file" => result.file = Some(value.into()),
                    "--byte" => result.byte = Some(value.parse()?),
                    "--function" => result.functions.push(value.into()),
                    "--revision" => result.revision = Some(value.into()),
                    "--output" => result.output = Some(value.into()),
                    "--before" => result.before = Some(value.into()),
                    "--after" => result.after = Some(value.into()),
                    "--max-nodes" => result.limits.max_nodes = value.parse()?,
                    "--max-depth" => result.limits.max_depth = value.parse()?,
                    "--direction" => {
                        result.direction = match value {
                            "callers" => Direction::Callers,
                            "callees" => Direction::Callees,
                            _ => anyhow::bail!("direction must be callers or callees"),
                        }
                    }
                    _ => unreachable!(),
                }
            } else {
                compiler_args.push(args[i].clone());
            }
            i += 1;
        }
        result.build = BuildCommand::parse(&compiler_args)?;
        anyhow::ensure!(
            !result.build.emit_hir && !result.build.emit_lir && result.build.output.is_none(),
            "analysis does not generate native/IR artifacts"
        );
        anyhow::ensure!(result.limits.max_nodes > 0, "max-nodes must be positive");
        match operation {
            "impact" => {
                anyhow::ensure!(
                    result.byte.is_some() == result.file.is_some(),
                    "--file and --byte must be specified together"
                );
                anyhow::ensure!(
                    result.byte.is_some() || !result.functions.is_empty(),
                    "impact requires --file/--byte or --function"
                );
                anyhow::ensure!(
                    result.functions.is_empty() || result.revision.is_some(),
                    "--function requires --revision"
                );
                anyhow::ensure!(
                    result.output.is_none() && result.before.is_none() && result.after.is_none(),
                    "snapshot options on impact"
                );
            }
            "save" => anyhow::ensure!(
                result.output.is_some()
                    && result.before.is_none()
                    && result.after.is_none()
                    && result.file.is_none()
                    && result.byte.is_none()
                    && result.functions.is_empty()
                    && result.revision.is_none()
                    && !seen.contains("--direction"),
                "save requires --output and no impact/diff options"
            ),
            "query" => anyhow::ensure!(
                result.requests.is_some()
                    && result.output.is_none()
                    && result.before.is_none()
                    && result.after.is_none()
                    && result.file.is_none()
                    && result.functions.is_empty()
                    && result.byte.is_none()
                    && result.revision.is_none()
                    && !seen.contains("--direction"),
                "query requires --requests and source/compiler options only"
            ),
            "diff" | "risk" => anyhow::ensure!(
                result.before.is_some()
                    && result.after.is_some()
                    && result.output.is_none()
                    && result.file.is_none()
                    && result.byte.is_none()
                    && result.functions.is_empty()
                    && result.revision.is_none()
                    && !seen.contains("--direction")
                    && compiler_args.is_empty(),
                "diff requires --before/--after and no source/compiler/impact options"
            ),
            _ => unreachable!(),
        }
        anyhow::ensure!(
            operation == "query" || result.requests.is_none(),
            "--requests requires query"
        );
        Ok(result)
    }
    pub(super) fn execute(self, emitter: &mut dyn DiagnosticEmitter) -> Result<Value> {
        if matches!(self.operation.as_str(), "diff" | "risk") {
            let before = Snapshot::load(std::path::Path::new(self.before.as_ref().unwrap()))?;
            let after = Snapshot::load(std::path::Path::new(self.after.as_ref().unwrap()))?;
            if self.operation == "risk" {
                return Ok(json!({"kind":"risk", "risk":after.risk(&before)?}));
            }
            return Ok(
                json!({"kind":"snapshot.diff", "difference": before.compare(&after,self.limits)?}),
            );
        }
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
            let requests: Vec<serde_json::Value> =
                serde_json::from_slice(&std::fs::read(self.requests.as_ref().unwrap())?)?;
            let mut session = willow_compiler::ai::QuerySession::new(snapshot)?;
            let mut results = Vec::with_capacity(requests.len());
            for mut request in requests {
                // A missing revision binds to this invocation's immutable snapshot.
                if request.get("revision").is_none() {
                    request
                        .as_object_mut()
                        .context("query must be an object")?
                        .insert("revision".into(), json!(session.revision()));
                }
                results.push(session.query(serde_json::from_value(request)?));
            }
            return Ok(json!({"kind":"query", "revision":session.revision(),"results":results}));
        }
        if self.operation == "save" {
            snapshot.save(std::path::Path::new(self.output.as_ref().unwrap()))?;
            return Ok(
                json!({"kind":"snapshot.saved", "revision":snapshot.revision,"path":self.output,"function_count":snapshot.functions.len()}),
            );
        }
        let mut seeds = self.functions;
        if let Some(file) = self.file {
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
        Ok(json!({"kind":"impact", "impact":impact,"functions":functions}))
    }
}

pub(super) const SNAPSHOT_COMMAND: &str =
    "willow snapshot save . --output snapshot.json --format ndjson --protocol-version 1";
pub(super) const QUERY_COMMAND: &str =
    "willow query . --requests queries.json --format ndjson --protocol-version 1";
pub(super) const IMPACT_COMMAND: &str = "willow impact . --function FUNCTION_ID --revision REVISION --direction callers --format ndjson --protocol-version 1";
