//! Canonical agent instructions and managed-section editing.
use anyhow::{Context, Result};
use serde::Serialize;

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
    pub machine_check: Option<&'static str>,
    pub machine_build: Option<&'static str>,
    pub snapshots: Option<&'static str>,
    pub symbol_query: Option<&'static str>,
    pub references: Option<&'static str>,
    pub callers: Option<&'static str>,
    pub impact: Option<&'static str>,
    pub structured_edits: Option<&'static str>,
}
impl AgentCapabilities {
    pub fn instruction_schema(self) -> u32 {
        if self.snapshots.is_some() { 2 } else { 1 }
    }
}

pub fn render_agent_instructions(agent: Agent, caps: AgentCapabilities) -> String {
    let mut text = format!(
        "{BEGIN}\n<!-- willow-instruction-schema: {} -->\n# Willow instructions for {}\n\n## Willow core\n\n",
        caps.instruction_schema(),
        agent.name()
    );
    text.push_str(
        "Run commands from the project root. Use compiler results to validate changes.\n",
    );
    for (label, command) in [("Check", caps.machine_check), ("Build", caps.machine_build)] {
        if let Some(command) = command {
            text.push_str(&format!("- {label}: `{command}`.\n"));
        }
    }
    if caps.machine_check.is_some() || caps.machine_build.is_some() {
        text.push_str("Parse stdout as NDJSON only in toolchain machine mode; stderr is human output and must not be parsed as protocol. Require supported schema_version on every event, a single stream with contiguous seq, and a final request.finished event. Missing request.finished is failure, never success. Require terminal status and exit_code to agree with the process exit status. Reject unsupported versions without guessing a fallback.\n\n");
    }
    text.push_str("Use willow add, willow remove, willow update, willow deps, willow fetch, and willow package verify for package work. Prefer --dry-run when planning add/remove/update changes. Package commands have their own output contract; do not interpret package output as the check/build NDJSON event protocol. Do not edit project.lock by hand.\n\n");
    if let Some(snapshot) = caps.snapshots {
        text.push_str(&format!("Before semantic or cross-file work, obtain a snapshot at the project root: `{snapshot}`. Query compiler facts against the corresponding revision; do not infer calls, references or impact from text search alone. Text search remains useful for navigation.\n\nSource, project.toml, project.lock, or path-dependency edits make the old snapshot stale. After edits, run machine-readable check, then obtain a new snapshot before further semantic queries. Finish with check/build. On snapshot failure, explicitly describe investigation as textual and do not invent semantic results. Snapshots do not replace Git commits. Trivial comment, README or typo edits do not require snapshots.\n\n"));
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
    if caps.symbol_query.is_some() || caps.references.is_some() || caps.callers.is_some() {
        text.push_str("Query request files are JSON arrays. Use compiler-returned IDs and the current revision; a stale, unknown or incomplete result is not proof of absence. The query command constructs its own immutable snapshot; bind requests to the saved revision to detect changes between commands.\n");
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
        (start == 0 || text.as_bytes()[start - 1] == b'\n')
            && (end == text.len()
                || text[end..].starts_with('\n')
                || text[end..].starts_with("\r\n")),
        "managed markers must be on their own lines"
    );
    Ok(Some(start..end))
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
