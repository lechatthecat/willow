//! Canonical agent instructions and managed-section editing.
use crate::ai::QueryRequest;
use anyhow::{Context, Result};
use serde::{Serialize, Serializer};

pub const BEGIN: &str = "<!-- BEGIN WILLOW MANAGED -->";
pub const END: &str = "<!-- END WILLOW MANAGED -->";
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Codex,
    Claude,
}
impl Agent {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            _ => anyhow::bail!("agent must be codex or claude"),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
    pub fn file(self) -> &'static str {
        match self {
            Self::Codex => "AGENTS.md",
            Self::Claude => "CLAUDE.md",
        }
    }
}

/// Commands are supplied by the driver that owns the public protocol surface.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct AgentCapabilities {
    #[serde(serialize_with = "available")]
    pub direct_refs: Option<&'static str>,
    #[serde(serialize_with = "available")]
    pub direct_symbol: Option<&'static str>,
    #[serde(serialize_with = "available")]
    pub direct_type: Option<&'static str>,
    #[serde(serialize_with = "available")]
    pub direct_effects: Option<&'static str>,
    #[serde(serialize_with = "available")]
    pub direct_impact: Option<&'static str>,
    #[serde(serialize_with = "available")]
    pub direct_rename: Option<&'static str>,
    #[serde(serialize_with = "available")]
    pub snapshot_clear: Option<&'static str>,
    #[serde(serialize_with = "available")]
    pub machine_check: Option<&'static str>,
    #[serde(serialize_with = "available")]
    pub machine_build: Option<&'static str>,
    #[serde(serialize_with = "available")]
    pub snapshots: Option<&'static str>,
    #[serde(serialize_with = "available")]
    pub symbol_query: Option<&'static str>,
    #[serde(serialize_with = "available")]
    pub references: Option<&'static str>,
    #[serde(serialize_with = "available")]
    pub callers: Option<&'static str>,
    #[serde(serialize_with = "available")]
    pub impact: Option<&'static str>,
    #[serde(serialize_with = "available")]
    pub structured_edits: Option<&'static str>,
}
// The wire contract exposes support flags; commands remain driver-owned examples.
fn available<S: Serializer>(command: &Option<&str>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_bool(command.is_some())
}
impl AgentCapabilities {
    pub fn instruction_schema(self) -> u32 {
        if [
            self.direct_refs,
            self.direct_symbol,
            self.direct_type,
            self.direct_effects,
            self.direct_impact,
            self.direct_rename,
        ]
        .iter()
        .any(Option::is_some)
        {
            3
        } else if self.snapshots.is_some() {
            2
        } else {
            1
        }
    }
}

pub fn render_agent_instructions(agent: Agent, caps: AgentCapabilities) -> String {
    let mut text = format!(
        "{BEGIN}\n<!-- willow-instruction-schema: {} -->\n# Willow instructions for {}\n\n## Willow core\n\n",
        caps.instruction_schema(),
        agent.name()
    );
    if caps.instruction_schema() >= 3 {
        text.push_str("Run commands from the project root. Prefer human output; use --format json when structured output helps. NDJSON is for low-level protocol integrations.\n\nUse compiler commands for semantic identity, references, dispatch, imports, inferred types, rename, impact and effects. Text search is useful for navigation, comments, string literals and broad exploration. Never substitute a textual match for a resolved symbol.\n\n");
        for (label, command) in [
            ("References", caps.direct_refs),
            ("Symbol", caps.direct_symbol),
            ("Type", caps.direct_type),
            ("Effects", caps.direct_effects),
            ("Impact", caps.direct_impact),
            ("Rename", caps.direct_rename),
        ] {
            if let Some(command) = command {
                text.push_str(&format!("- {label}: `{command}`.\n"));
            }
        }
        text.push_str("Selectors are qualified names or file:line:column positions (1-based Unicode columns). If a selector is ambiguous, retry with a selector returned in the candidate list. Read coverage, total/shown/truncated and result status; unknown or incomplete is not proof of absence.\n\n");
        if caps.direct_rename.is_some() {
            text.push_str("For rename, add --dry-run to inspect the diff, then run the same command without --dry-run. The command resolves the symbol, prepares and validates semantic edits, and applies the transaction. On failure, do not perform text replacement; report the location and any recovery transaction shown.\n\n");
        } else if let Some(command) = caps.structured_edits {
            text.push_str(&format!("Direct rename is unavailable. Use the low-level structured edit protocol: `{command}`, then edit validate and edit apply for the returned transaction.\n\n"));
        }
        if caps.snapshots.is_some() {
            text.push_str("Saved snapshots are optional for snapshot diff, before/after risk comparison, reproducible persisted analysis, external low-level clients and protocol debugging. Ordinary semantic queries do not require a snapshot file, request JSON, manual IDs or byte-offset calculation. Low-level query/edit/daemon protocols remain available for advanced batch integration.\n\n");
        }
        if caps.machine_check.is_some() {
            text.push_str("After changes run `willow check .`.\n");
        }
        if caps.machine_build.is_some() {
            text.push_str("When executable validation is needed, run `willow build .`.\n");
        }
        text.push_str("After source edits or branch switches, refresh a daemon before querying it. Restart it when configuration, entry point or compiler changes. Git branch names do not define analysis revisions; a clear is not a substitute for stale checks or refresh.\n");
        if let Some(command) = caps.snapshot_clear {
            text.push_str(&format!("Before a measurement session: shut down only its owned daemon and wait for exit; run `{command}` once on its dedicated registered snapshot directory; start a new process and create a fresh full baseline; then measure. Initialize a new empty measurement directory with `willow snapshot init --dir DIR` and save its snapshots with `--managed-dir DIR`. Never share these baselines outside the measurement set. Abort measurement if shutdown, refresh or clear fails. Do not clear between warm samples. Record initial analysis/baseline preparation time; include it in cold-start measurements and report it separately for snapshot-only IO measurements. Clear leaves OS page caches unchanged. Preserve historical/shared snapshots separately as complete independent sets.\n"));
        }
        text.push_str(&format!("\n{END}\n"));
        return text;
    }
    text.push_str(
        "Run commands from the project root. Use compiler results to validate changes.\n",
    );
    for (label, command) in [("Check", caps.machine_check), ("Build", caps.machine_build)] {
        if let Some(command) = command {
            text.push_str(&format!("- {label}: `{command}`.\n"));
        }
    }
    if caps.machine_check.is_some() || caps.machine_build.is_some() {
        text.push_str("Parse stdout as NDJSON only in toolchain machine mode; stderr is human output and must not be parsed as protocol. Require supported schema_version on every event, a single stream with contiguous seq, and request.started first and request.finished last. Missing request.finished is failure, never success. Require terminal status and exit_code to agree with the process exit status. Reject unsupported versions without guessing a fallback.\n\n");
    }
    text.push_str("Use willow add, willow remove, willow update, willow deps, willow fetch, and willow package verify for package work. Prefer --dry-run when planning add/remove/update changes. Package commands have their own output contract; do not interpret package output as the check/build NDJSON event protocol. Do not edit project.lock by hand.\n\n");
    if let Some(snapshot) = caps.snapshots {
        text.push_str(&format!("Before semantic or cross-file work, obtain a snapshot at the project root: `{snapshot}`. The --output destination must not exist; choose a new filename for every snapshot, including refreshes. Query compiler facts against the corresponding revision; do not infer calls, references or impact from text search alone. Text search remains useful for navigation.\n\nSource, project.toml, project.lock, or path-dependency edits make the old snapshot stale. After edits, run machine-readable check, then obtain a new snapshot before further semantic queries. Finish with check/build. On snapshot failure, explicitly describe investigation as textual and do not invent semantic results. Snapshots do not replace Git commits. Trivial comment, README or typo edits do not require snapshots.\n\n"));
    }
    for (label, command) in [
        ("Symbols", caps.symbol_query),
        ("References", caps.references),
        ("Callers", caps.callers),
        ("Impact", caps.impact),
        ("Structured edits", caps.structured_edits),
    ] {
        if let Some(command) = command {
            text.push_str(&format!("- {label}: `{command}`.\n"));
        }
    }
    if caps.symbol_query.is_some() || caps.references.is_some() {
        text.push_str("Query request files are JSON arrays. Use compiler-returned IDs and the current revision; a stale, unknown or incomplete result is not proof of absence. The query command constructs its own immutable snapshot; bind requests to the saved revision to detect changes between commands.\n");
        text.push_str("Replace REVISION with analysis.result.data.revision from snapshot.saved (or the current query result). Write each following JSON array to queries.json before running its command. Check each query result status as well as request.finished: a successful transport can contain stale or unknown results.\n");
        text.push_str("Keep symbol lists small: replace NAME_PREFIX with a name prefix, or filter by `name`, `module` or `symbol_kind`; `total`/`truncated` report what `limit` cut. Read `type_display` for types.\n");
        for (command, request) in [
            (
                caps.symbol_query,
                QueryRequest::Symbols {
                    revision: "REVISION".into(),
                    name: None,
                    prefix: Some("NAME_PREFIX".into()),
                    module: None,
                    symbol_kind: None,
                    limit: Some(50),
                },
            ),
            (
                caps.references,
                QueryRequest::References {
                    revision: "REVISION".into(),
                    function: "SYMBOL_ID".into(),
                },
            ),
        ] {
            if let Some(command) = command {
                let request =
                    serde_json::to_string(&[request]).expect("query example serialization");
                text.push_str(&format!(
                    "\nRun `{command}` with:\n\n```json\n{request}\n```\n"
                ));
            }
        }
        if caps.references.is_some() {
            text.push_str("Replace SYMBOL_ID with a compiler-returned declaration or callable ID from this revision; the references request field is named function even for non-callable declarations.\n");
        }
    }
    if caps.callers.is_some() || caps.impact.is_some() {
        text.push_str("Replace FUNCTION_ID with a callable id from the saved snapshot file's functions and REVISION with that snapshot's revision. Impact constructs a fresh snapshot and checks --revision before lookup. Read coverage, truncation and unresolved-call evidence; an incomplete result is not proof of no callers.\n");
    }
    text.push_str(&format!("\n{END}\n"));
    text
}

/// Reject malformed/duplicate markers instead of guessing which text we own.
pub fn managed_range(text: &str) -> Result<Option<std::ops::Range<usize>>> {
    let mut starts = text.match_indices(BEGIN).map(|(i, _)| i);
    let mut ends = text.match_indices(END).map(|(i, _)| i);
    let (start, end) = match (starts.next(), ends.next()) {
        (None, None) => return Ok(None),
        (Some(start), Some(end))
            if start < end && starts.next().is_none() && ends.next().is_none() =>
        {
            (start, end + END.len())
        }
        _ => anyhow::bail!("malformed or duplicate Willow managed block"),
    };
    anyhow::ensure!(
        marker_line(text, start, start + BEGIN.len()) && marker_line(text, end - END.len(), end),
        "managed markers must be on their own lines"
    );
    Ok(Some(start..end))
}

fn marker_line(text: &str, start: usize, end: usize) -> bool {
    (start == 0 || text.as_bytes()[start - 1] == b'\n')
        && (end == text.len() || text[end..].starts_with('\n') || text[end..].starts_with("\r\n"))
}

pub fn replace_managed(text: &str, rendered: &str) -> Result<Option<String>> {
    let Some(range) = managed_range(text)? else {
        return Ok(None);
    };
    let rendered = rendered.strip_suffix('\n').unwrap_or(rendered);
    let mut result = String::with_capacity(text.len() - range.len() + rendered.len());
    result.push_str(&text[..range.start]);
    result.push_str(rendered);
    result.push_str(&text[range.end..]);
    Ok(Some(result))
}
pub fn managed_version(text: &str) -> Result<Option<u32>> {
    let Some(range) = managed_range(text)? else {
        return Ok(None);
    };
    let value = text[range]
        .lines()
        .find_map(|line| {
            line.strip_prefix("<!-- willow-instruction-schema: ")
                .and_then(|v| v.strip_suffix(" -->"))
        })
        .context("missing instruction schema")?;
    Ok(Some(value.parse()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_flags_and_independent_omission() {
        let disabled = serde_json::to_value(AgentCapabilities::default()).unwrap();
        assert_eq!(disabled.as_object().unwrap().len(), 15);
        assert!(disabled.as_object().unwrap().values().all(|v| v == false));
        let caps = AgentCapabilities {
            references: Some("references-command"),
            ..Default::default()
        };
        let rendered = render_agent_instructions(Agent::Codex, caps);
        assert!(rendered.contains("references-command"));
        assert!(rendered.contains("\"kind\":\"references\""));
        assert!(!rendered.contains("\"kind\":\"symbols\""));
        assert!(!rendered.contains("Before semantic"));
        assert_eq!(caps.instruction_schema(), 1);
    }

    #[test]
    fn direct_capabilities_omit_disabled_commands_and_gate_edit_fallback() {
        for agent in [Agent::Codex, Agent::Claude] {
            let caps = AgentCapabilities {
                direct_refs: Some("refs-example"),
                direct_type: Some("type-example"),
                direct_rename: Some("rename-example"),
                structured_edits: Some("edit-fallback-example"),
                ..Default::default()
            };
            let complete = render_agent_instructions(agent, caps);
            assert!(complete.contains("refs-example"));
            assert!(complete.contains("type-example"));
            assert!(complete.contains("rename-example"));
            assert!(!complete.contains("edit-fallback-example"));
            assert!(!complete.contains("willow snapshot clear"));
            let partial = render_agent_instructions(
                agent,
                AgentCapabilities {
                    direct_refs: None,
                    direct_rename: None,
                    ..caps
                },
            );
            assert!(!partial.contains("refs-example"));
            assert!(!partial.contains("rename-example"));
            assert!(partial.contains("type-example"));
            assert!(partial.contains("edit-fallback-example"));
            assert_eq!(caps.instruction_schema(), 3);
        }
    }

    #[test]
    fn managed_replacement_preserves_bytes_at_increasing_sizes() {
        let rendered = render_agent_instructions(Agent::Codex, AgentCapabilities::default());
        for n in [16, 64, 256, 1024, 4096] {
            let prefix = "日本語\r\n".repeat(n);
            let suffix = "user suffix\r\n".repeat(n);
            let original = format!(
                "{prefix}{BEGIN}\r\n<!-- willow-instruction-schema: 1 -->\r\n{END}\r\n{suffix}"
            );
            let updated = replace_managed(&original, &rendered).unwrap().unwrap();
            assert_eq!(
                updated,
                format!("{prefix}{}\r\n{suffix}", rendered.trim_end())
            );
            assert_eq!(
                replace_managed(&updated, &rendered).unwrap().unwrap(),
                updated
            );
            println!(
                "user_bytes={} managed_bytes={} output_bytes={}",
                prefix.len() + suffix.len() + 2,
                rendered.trim_end().len(),
                updated.len()
            );
        }
    }
}
