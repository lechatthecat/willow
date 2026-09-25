//! One process, one workspace, one current revision. EOF/shutdown releases every
//! query value and artifact. No background service, socket or global cache.
use super::*;
use serde::Deserialize;
use serde_json::json;
use std::io::{BufRead, Read, Write};
use willow_compiler::{
    CompilerSession,
    ai::{QueryRequest, WarmSession, edit::Workspace},
};
#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case", deny_unknown_fields)]
enum Request {
    Query { id: u64, request: QueryRequest },
    Refresh { id: u64 },
    Stats { id: u64 },
    Shutdown { id: u64 },
}
pub(super) fn serve<W: Write>(
    args: &[String],
    events: &mut protocol::EventWriter<W>,
) -> Result<()> {
    let build = BuildCommand::parse(args)?;
    anyhow::ensure!(
        !build.emit_hir && !build.emit_lir && build.output.is_none(),
        "daemon does not emit artifacts"
    );
    let (entry, project) = if let Some(source) = build.source {
        (PathBuf::from(source), None)
    } else {
        let (manifest, root) = project::find_project_manifest(
            &build.project_dir.unwrap_or_else(|| PathBuf::from(".")),
        )
        .context("no project.toml")?;
        (
            project::ProjectManifest::load(&manifest)?.entry_point(&root),
            Some(root),
        )
    };
    let entry = std::fs::canonicalize(entry)?;
    let root = project.as_deref().unwrap_or(entry.parent().unwrap());
    let analyze = |events: &mut protocol::EventWriter<W>| -> Result<_> {
        let workspace = Workspace::open(root)?;
        workspace.ensure_recovered()?;
        CompilerSession::new(
            entry.to_str().context("non UTF-8 entry")?,
            "",
            &build.options,
            project.clone(),
        )
        .analysis_with_emitter(events)
    };
    let mut warm = WarmSession::new(analyze(events)?, 256, 8 * 1024 * 1024)?;
    events.event(
        "analysis.result",
        "WT0010",
        json!({"kind":"daemon.ready", "revision":warm.revision(), "stats":warm.stats()}),
    )?;
    let mut frontend_runs = 1usize;
    let mut input = std::io::stdin().lock();
    loop {
        let mut line = Vec::new();
        let n = input
            .by_ref()
            .take(1024 * 1024 + 1)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            return Ok(());
        }
        anyhow::ensure!(n <= 1024 * 1024, "daemon request exceeds 1 MiB");
        let request: Request = match serde_json::from_slice(&line) {
            Ok(request) => request,
            Err(error) => {
                events.event(
                    "analysis.result",
                    "WT0010",
                    json!({"kind":"daemon.response", "id":null, "error":error.to_string()}),
                )?;
                continue;
            }
        };
        let id = match &request {
            Request::Query { id, .. }
            | Request::Refresh { id }
            | Request::Stats { id }
            | Request::Shutdown { id } => *id,
        };
        let mut shutdown = matches!(&request, Request::Shutdown { .. });
        let result = match request {
            Request::Query { request, .. } => warm.query(request),
            Request::Refresh { .. } => {
                if frontend_runs >= 32 {
                    shutdown = true;
                    Err(anyhow::anyhow!(
                        "daemon frontend budget exhausted; restart required"
                    ))
                } else {
                    frontend_runs += 1;
                    analyze(events).and_then(|snapshot| {
                        warm.update(snapshot)?;
                        Ok(json!({"revision":warm.revision()}))
                    })
                }
            }
            Request::Stats { .. } | Request::Shutdown { .. } => {
                let mut stats = warm.stats();
                stats["frontend_runs"] = json!(frontend_runs);
                Ok(stats)
            }
        };
        let response = match result {
            Ok(result) => {
                json!({"kind":"daemon.response", "id":id, "result":result, "revision":warm.revision()})
            }
            Err(error) => {
                json!({"kind":"daemon.response", "id":id, "error":format!("{error:#}"), "revision":warm.revision()})
            }
        };
        events.event("analysis.result", "WT0010", response)?;
        if shutdown {
            return Ok(());
        }
    }
}
