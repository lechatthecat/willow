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
    pub direct_overview: Option<&'static str>,
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
    pub machine_check: Option<&'static str>,
    #[serde(serialize_with = "available")]
    pub machine_build: Option<&'static str>,
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
        if self.direct_overview.is_some() {
            return 4;
        }
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
        text.push_str("Run commands from the project root. Direct commands discover project.toml automatically; use --project-dir DIR to select another project. Human-readable output is the default; use --format json only when structured output helps. NDJSON is for low-level protocol integrations.\n\nPrefer references and structured rename over text matching when symbol identity matters: same-named fields, imports and dispatch can make grep or bulk replacement misleading. Inspect inferred types with the type command without building an executable. Use text search and source reading for navigation, comments, strings and simple local investigation. Start with the smallest query that answers the question.\n\n");
        if let Some(command) = caps.direct_overview {
            text.push_str(&format!("For an unfamiliar file or project, use `{command}` before reading whole files. Overview shows structure and reusable selectors; it omits bodies, references and effects. Pass its selectors to symbol, refs, type, effects or impact for details. If truncated, narrow the path before using --all. When the target symbol is already known, query it directly; overview is optional.\n\n"));
        }
        for (label, command) in [
            ("References", caps.direct_refs),
            ("Rename", caps.direct_rename),
            ("Inferred type", caps.direct_type),
            ("Symbol details", caps.direct_symbol),
        ] {
            if let Some(command) = command {
                text.push_str(&format!("- {label}: `{command}`.\n"));
            }
        }
        text.push_str("\nSelectors are qualified names or file:line:column positions (1-based Unicode columns, not UTF-8 byte offsets). Use project-relative file paths; replace the example names and positions with actual declarations or uses. Willow resolves names to compiler identities and converts source positions internally. Do not manually search for or extract FunctionId, SymbolId, revision IDs or byte offsets for ordinary code work; these belong to the low-level compiler/protocol interface. If a selector is ambiguous, retry with a selector returned in the candidate list. Read coverage, total/shown/truncated and result status; unknown or incomplete is not proof of absence.\n\n");
        if caps.direct_rename.is_some() {
            text.push_str("For rename, add --dry-run to inspect the diff, then run the same command without --dry-run. The command resolves the symbol, prepares and validates semantic edits, and applies the transaction atomically; do not manually run prepare/validate/apply or write edit request files for ordinary rename. On failure, do not perform text replacement; report the location and any recovery transaction shown.\n\n");
        } else if let Some(command) = caps.structured_edits {
            text.push_str(&format!("Direct rename is unavailable. Use the low-level structured edit protocol: `{command}`, then edit validate and edit apply for the returned transaction.\n\n"));
        }
        if caps.machine_check.is_some() {
            text.push_str("After changes run `willow check .`.\n");
        }
        if caps.machine_build.is_some() {
            text.push_str("When executable validation is needed, run `willow build .`.\n");
        }
        text.push_str("\n## Optional deeper analysis\n\nUse effects or impact when the question needs transitive behavior or cross-file risk analysis, especially with heavy dispatch or a large codebase. Source reading is often enough for small local changes; do not run every query as a checklist.\n");
        for (label, command) in [
            ("Effects", caps.direct_effects),
            ("Impact", caps.direct_impact),
        ] {
            if let Some(command) = command {
                text.push_str(&format!("- {label}: `{command}`.\n"));
            }
        }
        text.push_str("\nUse query/edit --help for advanced batch integrations. Keep project context consistent (including --project where required), use compiler-returned IDs and UTF-8 byte offsets, and reject stale results. Query the current source again after changes. Never bypass stale checks.\n");
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
        text.push_str("Query request files are JSON arrays. Use compiler-returned IDs and the current revision; a stale, unknown or incomplete result is not proof of absence. The query command analyzes current source; bind requests to the returned revision to detect changes between commands.\n");
        text.push_str("Replace REVISION with analysis.result.data.revision from the current query result. Write each following JSON array to queries.json before running its command. Check each query result status as well as request.finished: a successful transport can contain stale or unknown results.\n");
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
        text.push_str("Replace FUNCTION_ID with a callable id from a symbols query and REVISION with its revision. Impact analyzes current source and checks --revision before lookup. Read coverage, truncation and unresolved-call evidence; an incomplete result is not proof of no callers.\n");
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
        assert_eq!(disabled.as_object().unwrap().len(), 14);
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
