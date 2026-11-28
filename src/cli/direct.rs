//! Human-oriented semantic commands; compiler APIs own resolution and queries.
use super::*;
use serde::Serialize;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use willow_compiler::ai::direct::{DirectCommand, DirectSession, RenameMode, SelectorFilter};
use willow_compiler::ai::{
    SymbolKind,
    direct::{DirectQueryData, DirectQueryResult, RenamePreviewResult, RenameResult},
    query_result::{QueryData, Status, SymbolInfo},
};

#[derive(Serialize)]
struct CommandOutput {
    schema: u32,
    kind: DirectCommand,
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<CommandResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<willow_compiler::ai::edit::RejectionLocation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<WarningCounts>,
}
#[derive(Serialize)]
struct WarningCounts {
    warnings: usize,
}
#[derive(Serialize)]
#[serde(untagged)]
enum CommandResult {
    Query(Box<DirectQueryResult>),
    Rename(RenameResult),
    Summary(RenameSummary),
    Diff {
        #[serde(flatten)]
        preview: RenamePreviewResult,
        #[serde(flatten)]
        summary: RenameSummary,
    },
}
#[derive(Serialize)]
struct RenameSummary {
    files: usize,
    edits: usize,
    declarations: usize,
    references: usize,
    collisions: usize,
    unresolved: usize,
}
impl From<&RenamePreviewResult> for RenameSummary {
    fn from(preview: &RenamePreviewResult) -> Self {
        Self {
            files: preview.preview.files_changed,
            edits: preview.edits,
            declarations: preview.declarations_updated,
            references: preview.references_updated,
            collisions: 0,
            unresolved: 0,
        }
    }
}
impl CommandOutput {
    fn new(kind: DirectCommand, status: Status) -> Self {
        Self {
            schema: 1,
            kind,
            status,
            revision: None,
            result: None,
            message: None,
            location: None,
            diagnostics: None,
        }
    }
}

// Includes lexical symbols omitted from the overview's declaration-only list.
const SYMBOL_KINDS: &[&str] = &[
    "module",
    "import",
    "function",
    "const",
    "class",
    "interface",
    "enum",
    "field",
    "static-field",
    "method",
    "constructor",
    "variant",
    "parameter",
    "binding",
    "type-parameter",
];

pub(super) struct Options {
    command: DirectCommand,
    selector: String,
    format: OutputFormat,
    source: Option<String>,
    project: Option<PathBuf>,
    filter: SelectorFilter,
    all: bool,
    absolute: bool,
    explain: bool,
    show_id: bool,
    new_name: Option<String>,
    dry_run: bool,
    diff: bool,
    verbose: bool,
    warnings: bool,
    max_chars: Option<usize>,
}
pub(super) struct Parser<'a> {
    options: Options,
    seen: std::collections::HashSet<&'a str>,
    pending: Option<&'a str>,
    valid_format: bool,
}

impl<'a> Parser<'a> {
    pub(super) fn new(command: &str) -> Self {
        Self {
            options: Options {
                command: DirectCommand::parse(command)
                    .expect("direct command selected by request parser"),
                selector: String::new(),
                format: OutputFormat::Human,
                source: None,
                project: None,
                filter: SelectorFilter::default(),
                all: false,
                absolute: false,
                explain: false,
                show_id: false,
                new_name: None,
                dry_run: false,
                diff: false,
                verbose: false,
                warnings: false,
                max_chars: None,
            },
            seen: Default::default(),
            pending: None,
            valid_format: true,
        }
    }

    pub(super) fn push(&mut self, arg: &'a str) -> Result<()> {
        if let Some(key) = self.pending.take() {
            return self.value(key, arg);
        }
        let result = &mut self.options;
        let (key, inline) = super::request::split_option(arg);
        if !key.starts_with('-') {
            if result.selector.is_empty() {
                result.selector = arg.to_owned();
            } else {
                anyhow::ensure!(
                    result.command == DirectCommand::Rename && result.new_name.is_none(),
                    "unexpected positional argument"
                );
                result.new_name = Some(arg.to_owned());
            }
        } else {
            anyhow::ensure!(self.seen.insert(key), "duplicate option {key}");
            match key {
                "--all" | "--absolute-paths" | "--explain" | "--show-id" | "--dry-run"
                | "--verbose" | "--diff" | "--warnings" => {
                    anyhow::ensure!(inline.is_none(), "unexpected value for {key}");
                    match key {
                        "--all" => result.all = true,
                        "--absolute-paths" => result.absolute = true,
                        "--explain" => result.explain = true,
                        "--diff" => result.diff = true,
                        "--dry-run" => result.dry_run = true,
                        "--verbose" => result.verbose = true,
                        "--warnings" => result.warnings = true,
                        _ => result.show_id = true,
                    }
                }
                "--format" | "--source" | "--project-dir" | "--kind" | "--module" | "--package"
                | "--max-chars" => {
                    if let Some(value) = inline {
                        return self.value(key, value);
                    }
                    self.pending = Some(key);
                }
                _ => anyhow::bail!("unknown option {key}"),
            }
        }
        Ok(())
    }

    fn value(&mut self, key: &str, value: &str) -> Result<()> {
        let result = &mut self.options;
        anyhow::ensure!(
            !value.is_empty() && !value.starts_with("--"),
            "missing option value for {key}"
        );
        match key {
            "--max-chars" => {
                result.max_chars = Some(
                    value
                        .parse()
                        .context("max-chars must be a nonnegative integer")?,
                )
            }
            "--format" => {
                if let Some(format) = OutputFormat::parse(value) {
                    result.format = format;
                } else {
                    self.valid_format = false;
                }
            }
            "--source" => result.source = Some(value.into()),
            "--project-dir" => result.project = Some(value.into()),
            "--kind" => {
                anyhow::ensure!(
                    SYMBOL_KINDS.contains(&value),
                    "invalid --kind '{value}'. Valid symbol kinds: {}. \
                                     --kind filters symbol kinds, not effect names; \
                                     the effects command currently has no effect filter. \
                                     Use willow effects SELECTOR without --kind to inspect effects.",
                    SYMBOL_KINDS.join(", ")
                );
                result.filter.kind = Some(value.into());
            }
            "--module" => result.filter.module = Some(value.into()),
            _ => result.filter.package = Some(value.into()),
        }
        Ok(())
    }

    pub(super) fn finish(self) -> Result<Options> {
        anyhow::ensure!(self.pending.is_none(), "missing option value");
        let result = self.options;
        anyhow::ensure!(!result.selector.is_empty(), "missing selector");
        anyhow::ensure!(
            result.command != DirectCommand::Rename || result.new_name.is_some(),
            "rename requires a new name"
        );
        anyhow::ensure!(
            result.command == DirectCommand::Rename
                || (!result.dry_run && !result.verbose && !result.diff),
            "rename-only option"
        );
        anyhow::ensure!(!result.diff || result.dry_run, "--diff requires --dry-run");
        anyhow::ensure!(self.valid_format, "format must be human, json or ndjson");
        anyhow::ensure!(
            result.source.is_none() || result.project.is_none(),
            "--source and --project-dir are mutually exclusive"
        );
        anyhow::ensure!(
            result.max_chars.is_none() || result.command != DirectCommand::Rename || result.dry_run,
            "--max-chars requires --dry-run for rename"
        );
        Ok(result)
    }
}
impl Options {
    #[cfg(test)]
    fn parse(args: &[String]) -> Result<Self> {
        let mut parser = Parser::new(&args[0]);
        for arg in &args[1..] {
            parser.push(arg)?;
        }
        parser.finish()
    }

    /// `warnings` counts checker warnings even when analysis or the query
    /// later fails, so every result can report the suppressed total.
    fn execute(&self, warnings: &mut usize) -> Result<CommandOutput> {
        let (entry, root) = if let Some(source) = &self.source {
            (source.clone(), None)
        } else {
            let directory = self.project.clone().unwrap_or_else(|| PathBuf::from("."));
            let (manifest, root) = project::find_project_manifest(&directory)
                .context("no project.toml found; use --source for a standalone file")?;
            let manifest = project::ProjectManifest::load(&manifest)?;
            (
                manifest
                    .entry_point(&root)
                    .to_str()
                    .context("non UTF-8 entry")?
                    .to_owned(),
                Some(root),
            )
        };
        let project = root.is_some();
        let compiler = willow_compiler::CompilerSession::new(
            &entry,
            "",
            &CompilerOptions::debug(),
            root.clone(),
        );
        let mut human_emitter = willow_compiler::diagnostics::HumanEmitter;
        let mut project_emitter;
        let inner: &mut dyn willow_compiler::diagnostics::DiagnosticEmitter =
            if let Some(root) = root.as_deref() {
                project_emitter = willow_compiler::diagnostics::ProjectEmitter {
                    root,
                    inner: &mut human_emitter,
                };
                &mut project_emitter
            } else {
                &mut human_emitter
            };
        // Queries answer about symbols, not project hygiene: checker warnings
        // are counted and shown only on request; errors always pass through.
        let emitter = &mut WarningFilter {
            inner,
            show: self.warnings,
            count: warnings,
        };
        let snapshot = if self.command == DirectCommand::Rename {
            compiler.analysis_for_edit_with_emitter(emitter)?
        } else {
            compiler.overview_with_emitter(emitter)?
        };
        let mut session = DirectSession::new(snapshot)?;
        if self.command == DirectCommand::Rename {
            let mut result = session.rename_typed_with_emitter(
                std::path::Path::new(&entry),
                project,
                &self.selector,
                &self.filter,
                self.new_name.as_deref().unwrap(),
                if !self.dry_run {
                    RenameMode::Apply
                } else if self.diff {
                    RenameMode::Diff
                } else {
                    RenameMode::Summary
                },
                emitter,
            )?;
            let mut output = CommandOutput::new(self.command, result.status());
            if self.dry_run
                && result.status() == Status::Ok
                && let RenameResult::Preview(preview) = result
            {
                let summary = RenameSummary::from(&preview);
                output.result = Some(if self.diff {
                    CommandResult::Diff { preview, summary }
                } else {
                    CommandResult::Summary(summary)
                });
                return Ok(output);
            }
            if !self.verbose
                && let RenameResult::Applied(applied) = &mut result
            {
                applied.transaction = None;
            }
            output.result = Some(CommandResult::Rename(result));
            return Ok(output);
        }
        let mut result =
            session.query_typed(self.command, &self.selector, &self.filter, self.all)?;
        if matches!(self.command, DirectCommand::Effects | DirectCommand::Impact)
            && self.format == OutputFormat::Human
        {
            session.display_function_targets(&mut result, self.absolute)?;
        }
        session.display_query_locations(&mut result, self.absolute)?;
        if !(self.explain
            || self.command == DirectCommand::Effects && self.format == OutputFormat::Human)
        {
            concise(&mut result);
        }
        let mut output = CommandOutput::new(self.command, result.status);
        output.revision = Some(session.session.revision().to_owned());
        output.result = Some(CommandResult::Query(Box::new(result)));
        Ok(output)
    }
}

struct WarningFilter<'a> {
    inner: &'a mut dyn willow_compiler::diagnostics::DiagnosticEmitter,
    show: bool,
    count: &'a mut usize,
}
impl willow_compiler::diagnostics::DiagnosticEmitter for WarningFilter<'_> {
    fn emit(
        &mut self,
        diagnostic: &willow_compiler::diagnostics::Diagnostic,
        sources: &dyn willow_compiler::diagnostics::source_map::SourceLookup,
    ) -> std::io::Result<()> {
        if diagnostic.severity != willow_compiler::diagnostics::Severity::Warning {
            return self.inner.emit(diagnostic, sources);
        }
        *self.count += 1;
        if self.show {
            self.inner.emit(diagnostic, sources)?;
        }
        Ok(())
    }
}

fn concise(value: &mut DirectQueryResult) {
    match &mut value.data {
        DirectQueryData::Semantic(QueryData::Symbol {
            symbol: SymbolInfo::Function(function),
            ..
        }) => {
            function.fingerprint = None;
            function.body_fingerprint = None;
            function.callees = None;
        }
        DirectQueryData::Semantic(QueryData::Type(typed)) => {
            if typed.type_display.is_some() {
                typed.ty = None;
            }
        }
        DirectQueryData::Semantic(QueryData::References {
            references,
            unresolved_candidates,
            ..
        }) => {
            for reference in references
                .iter_mut()
                .chain(unresolved_candidates.iter_mut().flatten())
            {
                reference.fingerprint = None;
                reference.flow = None;
                reference.node = None;
                reference.occurrences = None;
            }
        }
        DirectQueryData::Semantic(QueryData::Effects(effects)) => {
            effects.witness = None;
            effects.compiler_witnesses = None;
            if let Some(facts) = &mut effects.effect_evidence {
                for fact in facts {
                    fact.witness = None;
                }
            }
        }
        _ => {}
    }
}

fn location(value: Option<&willow_compiler::ai::Location>) -> String {
    let Some(value) = value else {
        return "?:null:null".into();
    };
    let position = value.display.as_deref();
    format!(
        "{}:{}:{}",
        value.path,
        position.map_or_else(|| "null".into(), |p| p.line.to_string()),
        position.map_or_else(|| "null".into(), |p| p.column.to_string())
    )
}
fn selected_location(value: Option<&willow_compiler::ai::direct::SelectedLocation>) -> String {
    let Some(value) = value else {
        return "?:null:null".into();
    };
    format!(
        "{}:{}:{}",
        value.path,
        value.line.map_or_else(|| "null".into(), |v| v.to_string()),
        value
            .column
            .map_or_else(|| "null".into(), |v| v.to_string())
    )
}
fn counted(n: u64, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}
fn human(value: &CommandOutput, options: &Options) -> String {
    human_items(value, options).join("\n")
}

#[allow(
    clippy::too_many_lines,
    reason = "One ordered presentation for each typed direct response"
)]
fn human_items(value: &CommandOutput, options: &Options) -> Vec<String> {
    if let Some(CommandResult::Summary(summary) | CommandResult::Diff { summary, .. }) =
        &value.result
    {
        let mut output = Vec::new();
        if let Some(CommandResult::Diff { preview, .. }) = &value.result
            && let Some(changes) = &preview.preview.changes
        {
            for change in changes {
                let diff = &change.diff;
                if !diff.is_empty() || output.is_empty() {
                    output.push(diff.strip_suffix('\n').unwrap_or(diff).to_owned());
                }
            }
        }
        output.push(format!(
            "{} / {} ({}, {}) / {} / {} unresolved",
            counted(summary.files as u64, "file"),
            counted(summary.edits as u64, "edit"),
            counted(summary.declarations as u64, "declaration"),
            counted(summary.references as u64, "reference"),
            counted(summary.collisions as u64, "collision"),
            summary.unresolved
        ));
        return output;
    }
    if let Some(CommandResult::Rename(RenameResult::Applied(result))) = &value.result {
        let files = counted(result.files_changed as u64, "file");
        let edits = format!(
            "{} ({}, {})",
            counted(result.edits as u64, "edit"),
            counted(result.declarations_updated as u64, "declaration"),
            counted(result.references_updated as u64, "reference")
        );
        let new_name = options.new_name.as_deref().unwrap();
        let mut output = format!(
            "Renamed {} -> {new_name}\n{files} changed\n{edits}\nValidation: passed",
            options.selector
        );
        for warning in result.warnings.iter().flatten() {
            output.push_str(&format!("\nWarning: {warning}"));
        }
        if options.verbose {
            output.push_str(&format!(
                "\nTransaction: {}",
                result.transaction.as_deref().unwrap_or("?")
            ));
        }
        return vec![output];
    }
    let mut lines = vec![format!(
        "{} {}: {}",
        options.command,
        options.selector,
        value.status.as_str()
    )];
    if let Some(message) = &value.message {
        lines.push(message.clone());
    }
    let result = match &value.result {
        Some(
            CommandResult::Query(result) | CommandResult::Rename(RenameResult::Resolution(result)),
        ) => result,
        _ => return lines,
    };
    let reason = result.reason.as_deref().or_else(|| {
        result
            .effects()
            .and_then(|effects| effects.reason.as_deref())
    });
    if let Some(reason) = reason {
        lines.push(reason.into());
    }
    match &result.data {
        DirectQueryData::Ambiguous { candidates } => {
            for candidate in candidates {
                lines.push(format!(
                    "  {} ({}) {}",
                    candidate
                        .qualified_name
                        .as_deref()
                        .unwrap_or(&candidate.selector),
                    candidate.kind,
                    selected_location(candidate.location.as_ref())
                ));
            }
        }
        DirectQueryData::Unknown { suggestions } => {
            for suggestion in suggestions {
                lines.push(format!("  suggestion: {suggestion}"));
            }
        }
        _ => {}
    }
    if let DirectQueryData::Semantic(QueryData::References {
        references,
        coverage,
        unresolved_candidates,
    }) = &result.data
    {
        for reference in references {
            lines.push(format!(
                "  {} {}{}",
                location(Some(&reference.location)),
                reference.role,
                if reference.certainty
                    == Some(willow_compiler::ai::query_result::ReferenceCertainty::PossibleDispatch)
                {
                    " (possible-dispatch)"
                } else {
                    ""
                }
            ));
        }
        lines.push(format!(
            "total={} shown={} truncated={}",
            result.total.unwrap_or(0),
            result.shown.unwrap_or(0),
            result.truncated.unwrap_or(false)
        ));
        lines.push(format!(
            "Coverage: {}",
            coverage.as_deref().unwrap_or("compiler-resolved")
        ));
        for candidate in unresolved_candidates.iter().flatten() {
            lines.push(format!(
                "  {} indirect candidate: {}",
                location(Some(&candidate.location)),
                candidate.reason.as_deref().unwrap_or("unresolved target")
            ));
        }
    }
    let selected = result.selected.as_ref().or(match &result.data {
        DirectQueryData::Resolved { symbol } | DirectQueryData::Impact { symbol, .. } => {
            Some(symbol)
        }
        _ => None,
    });
    if let Some(selected) = selected {
        let name = selected
            .qualified_name
            .as_deref()
            .unwrap_or(&selected.selector);
        lines.push(format!("Symbol: {name} ({})", selected.kind));
        if selected.location.is_some() {
            lines.push(selected_location(selected.location.as_ref()));
        }
        if options.show_id {
            lines.push(format!("ID: {}", selected.id));
        }
    }
    let (typed, declared) = match &result.data {
        DirectQueryData::Semantic(QueryData::Type(typed)) => (Some(typed), None),
        DirectQueryData::Semantic(QueryData::Symbol {
            symbol: SymbolInfo::Declaration(declared),
            ..
        }) => (None, Some(declared)),
        _ => (None, None),
    };
    if let Some(ty) = result
        .type_display
        .as_deref()
        .or_else(|| typed.and_then(|t| t.type_display.as_deref()))
        .or_else(|| declared.and_then(|d| d.type_display.as_deref()))
    {
        lines.push(format!("Type: {ty}"));
    }
    if let Some(payload) = typed
        .map(|t| &t.payload_display)
        .or_else(|| declared.map(|d| &d.payload_display))
        .filter(|p| !p.is_empty())
    {
        lines.push(format!("Payload: ({})", payload.join(", ")));
    }
    if options.command == DirectCommand::Type {
        match result.status {
            Status::Unknown if willow_compiler::ai::direct::split_location(&options.selector).is_none() => lines.push("No symbol matches this selector and its filters. Use a qualified symbol name or file:line:column on an identifier or expression.".into()),
            Status::Unknown => lines.push("No typed expression or declaration was found at this position (keywords, whitespace and comments have no type). Place the cursor on an expression or identifier, or use `willow type QUALIFIED_SYMBOL`.".into()),
            Status::Unanalyzed => lines.push("A semantic expression or declaration was found, but its type is unavailable in this analysis. Run `willow check .` for diagnostics; try a typed expression or declaration identifier.".into()),
            Status::Ambiguous => lines.push("Multiple semantic candidates overlap this position. Select a declaration identifier or a qualified symbol.".into()),
            _ => {}
        }
    }
    if let Some(effects) = result.effects() {
        effect_lines(effects, selected, options.all, options.explain, &mut lines);
    }
    if let DirectQueryData::Impact { impact, symbol } = &result.data {
        use willow_compiler::ai::{ImpactKind, ImpactRelation};
        let nodes = &impact.nodes;
        lines.push(format!("Affected functions: {}", nodes.len()));
        let method = symbol.kind == SymbolKind::Method;
        let group = |node: &willow_compiler::ai::ImpactNode| match node.relation {
            Some(ImpactRelation::Definition) => Some(0),
            Some(ImpactRelation::DirectCaller) => Some(1),
            Some(ImpactRelation::IndirectCaller) => Some(2),
            None => None,
        };
        let mut counts = [0usize; 3];
        for node in nodes {
            if let Some(g) = group(node) {
                counts[g] += 1;
            }
        }
        let mut current = None;
        for node in nodes.iter().take(if options.all { usize::MAX } else { 50 }) {
            if let Some(g) = group(node)
                && current != Some(g)
            {
                current = Some(g);
                lines.push(format!(
                    "{}: {}",
                    match g {
                        0 if method =>
                            "Definitions (the method, its overrides and implementations)",
                        0 => "Definition",
                        1 => "Direct callers",
                        _ => "Indirect callers (only through another affected function)",
                    },
                    counts[g]
                ));
            }
            lines.push(format!(
                "  {}{}{}",
                if node.kind == Some(ImpactKind::Lambda) {
                    "lambda "
                } else {
                    ""
                },
                node.selector.as_deref().unwrap_or("unknown symbol"),
                if group(node) == Some(2) {
                    format!(" (distance {})", node.distance)
                } else {
                    String::new()
                }
            ));
            let start = lines.len();
            effect_state_lines(&node.effects, &mut lines);
            for line in &mut lines[start..] {
                line.insert_str(0, "    ");
            }
        }
        lines.push(format!(
            "total={} shown={} truncated={} unknown={}",
            nodes.len(),
            if options.all {
                nodes.len()
            } else {
                nodes.len().min(50)
            },
            impact.truncated || (!options.all && nodes.len() > 50),
            impact.unknown
        ));
        if let Some(reason) = impact.reason {
            lines.push(reason.into());
        }
    }
    if options.explain
        && options.command == DirectCommand::Type
        && let Some(typed) = typed
        && typed.location.is_some()
    {
        lines.push(format!("Typed at: {}", location(typed.location.as_ref())));
    }
    lines
}

fn effect_names(bits: Option<u8>) -> String {
    use willow_abi::RuntimeEffects as E;
    let Some(bits) = bits else {
        return "unavailable".into();
    };
    let names: Vec<_> = [
        (E::MAY_ALLOCATE, "may-allocate"),
        (E::MAY_BLOCK, "may-block"),
        (E::MAY_SUSPEND, "may-suspend"),
        (E::MAY_PREEMPT, "may-preempt"),
        (E::NO_PREEMPT_REGION, "may-run-without-preemption"),
        (E::MAY_PANIC, "may-panic"),
        (E::MAY_IO, "may-io"),
    ]
    .into_iter()
    .filter_map(|(effect, name)| (bits & effect.bits() != 0).then_some(name))
    .collect();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

fn effect_state_lines(
    effects: &[willow_compiler::ai::query_result::EffectState],
    lines: &mut Vec<String>,
) {
    use willow_compiler::ai::query_result::{State, UnknownReasonCode};
    for effect in effects {
        let state = match effect.state {
            State::May => "may",
            State::Unknown => "unknown",
            State::None => "none",
        };
        lines.push(format!("{}: {state}", effect_names(Some(effect.effect))));
        if let Some(witness) = &effect.witness {
            lines.push(format!("  Witness: {}", location(Some(&witness.location))));
        }
        for reason in &effect.unknown_reasons {
            let code = match reason.code {
                UnknownReasonCode::IndirectCall => "indirect-call",
                UnknownReasonCode::UnresolvedCallee => "unresolved-callee",
                UnknownReasonCode::IncompleteFlow => "incomplete-flow",
                UnknownReasonCode::MissingBody => "missing-body",
                UnknownReasonCode::ExternalInterop => "external-interop",
                UnknownReasonCode::MissingWitness => "missing-witness",
                UnknownReasonCode::MissingSource => "missing-source",
                UnknownReasonCode::FunctionFlowExternalEscape => "function-flow-external-escape",
                UnknownReasonCode::FunctionFlowUnknownStorage => "function-flow-unknown-storage",
                UnknownReasonCode::FunctionFlowCandidateLimit => "function-flow-candidate-limit",
                UnknownReasonCode::FunctionFlowDepthLimit => "function-flow-depth-limit",
                UnknownReasonCode::FunctionFlowUnresolvedSource => {
                    "function-flow-unresolved-source"
                }
            };
            let mut detail = format!("  Unknown: {code}");
            if let Some(callee) = &reason.callee {
                detail.push_str(&format!(" `{callee}`"));
            }
            if let Some(site) = &reason.location {
                detail.push_str(&format!(" at {}", location(Some(site))));
            }
            lines.push(detail);
        }
    }
}

fn effect_lines(
    result: &willow_compiler::ai::query_result::EffectResult,
    selected: Option<&willow_compiler::ai::direct::SelectedSymbol>,
    all: bool,
    explain: bool,
    lines: &mut Vec<String>,
) {
    use willow_compiler::ai::evidence::AllocationPath;
    effect_state_lines(&result.effects, lines);
    if let Some(causes) = &result.unknown_causes {
        for cause in causes.iter().take(if all { usize::MAX } else { 50 }) {
            lines.push(format!("Incomplete effect summary: {}", cause.name));
            for site in cause.call_sites.iter().flatten() {
                lines.push(format!(
                    "  Unresolved call `{}` at {}",
                    site.operation,
                    location(Some(&site.location))
                ));
            }
            for loc in &cause.locations {
                lines.push(format!("  {}", location(Some(loc))));
            }
            if cause.locations.is_empty() {
                lines.push("  No declaration source is available.".into());
                if let Some(caller) = &cause.caller_location {
                    lines.push(format!(
                        "  Referenced from caller at {}",
                        location(Some(caller))
                    ));
                }
            }
        }
        if !all && causes.len() > 50 {
            lines.push(format!(
                "  {} unknown causes omitted; use --all",
                causes.len() - 50
            ));
        }
    }
    if explain {
        lines.push(format!(
            "Conservative runtime bounds: {}",
            effect_names(result.bounds.as_ref().map(|bounds| bounds.runtime))
        ));
        lines.push(format!(
            "Compiler bounds: {}",
            effect_names(result.bounds.as_ref().and_then(|bounds| bounds.compiler))
        ));
    }
    if explain
        && result.bounds.as_ref().is_some_and(|bounds| {
            (bounds.runtime | bounds.compiler.unwrap_or(0))
                & willow_abi::RuntimeEffects::NO_PREEMPT_REGION.bits()
                != 0
        })
    {
        lines.push("Preemption evidence is conservative (for example, a loop or recursion); it does not mean the entire function disables preemption.".into());
    }
    let target = selected.map_or("selected function", |symbol| symbol.selector.as_str());
    if let Some(Some(proof)) = &result.no_panic {
        lines.push(format!(
            "no_panic: {} ({}; runtime checks retained)",
            proof.status.as_str(),
            proof.basis.as_str()
        ));
    }
    let paths: Vec<_> = result
        .effect_evidence
        .iter()
        .flatten()
        .filter_map(|fact| fact.paths.as_ref())
        .collect();
    if !paths.is_empty() {
        lines.push(
            "Builtin allocation paths (operation only; aggregate bounds include panic):".into(),
        );
        let shown = if all {
            paths.len()
        } else {
            paths.len().min(50)
        };
        let allocation = |value: &AllocationPath| match value {
            AllocationPath::May => "may",
            AllocationPath::None => "none",
        };
        for path in &paths[..shown] {
            lines.push(format!(
                "  {} at {}: normal may-allocate: {}; panic may-allocate: {}",
                path.operation
                    .as_ref()
                    .map_or("builtin", |operation| operation.as_str()),
                location(path.location.as_ref().and_then(Option::as_ref)),
                allocation(&path.normal.may_allocate),
                allocation(&path.panic.may_allocate),
            ));
        }
        if shown < paths.len() {
            lines.push(format!(
                "  {} builtin paths omitted; use --all",
                paths.len() - shown
            ));
        }
    }
    if !explain {
        compact_effect_evidence(result, target, all, lines);
        return;
    }
    for (label, facts) in [
        ("Runtime evidence", result.effect_evidence.as_deref()),
        (
            "Local evidence (compiler facts and runtime capabilities)",
            result
                .compiler_witnesses
                .as_ref()
                .and_then(|facts| facts.as_deref()),
        ),
    ] {
        let evidence: Vec<_> = facts
            .into_iter()
            .flatten()
            .filter(|fact| fact.paths.is_none())
            .collect();
        lines.push(format!("{label} for {target}: {}", evidence.len()));
        let shown = if all {
            evidence.len()
        } else {
            evidence.len().min(50)
        };
        for fact in &evidence[..shown] {
            let witness = fact.witness.as_ref();
            let cause = witness.and_then(|witness| witness.cause.as_ref());
            let mut detail = format!(
                "  {}: {}",
                effect_names(Some(fact.effect)),
                fact.status
                    .as_ref()
                    .map(|status| status.as_str())
                    .or_else(|| witness.map(|witness| witness.kind.as_str()))
                    .unwrap_or("unavailable")
            );
            let locked = locked_label(cause);
            let chain = effect_call_chain(fact);
            for (name, value) in [
                ("chain", chain.as_deref()),
                ("via", fact.via_function.as_deref()),
                (
                    "target",
                    witness.and_then(|witness| witness.target.as_deref()),
                ),
                (
                    "owner",
                    witness.and_then(|witness| witness.owner.as_deref()),
                ),
                ("operation", cause.map(|cause| cause.operation.as_str())),
                ("locked", locked.as_deref()),
                (
                    "reason",
                    fact.reason
                        .as_deref()
                        .or_else(|| witness.and_then(|witness| witness.reason.as_deref())),
                ),
            ] {
                if let Some(value) = value {
                    detail.push_str(&format!("; {name}={value}"));
                }
            }
            if let Some(task) = task_relationship_label(fact) {
                detail.push_str(&format!("; {task}"));
            }
            if let Some(at) = cause.and_then(|cause| cause.location()) {
                detail.push_str(&format!(" at {}", location(Some(at))));
            }
            lines.push(detail);
        }
        if shown < evidence.len() {
            lines.push(format!(
                "{} omitted; use --all to show all evidence",
                evidence.len() - shown
            ));
        }
    }
    lines.push(result.meaning.unwrap_or("").into());
}

fn task_relationship_label(
    fact: &willow_compiler::ai::evidence::ExplainedEffect,
) -> Option<String> {
    use willow_compiler::ai::evidence::TaskKind;
    let task = fact.task_relationship.as_ref()?;
    let kind = match task.kind {
        TaskKind::Spawn => "spawn",
        TaskKind::Await => "await",
    };
    if task.location.as_ref() == fact.witness.as_ref().and_then(|witness| witness.location()) {
        Some(format!("task={kind}"))
    } else {
        Some(format!(
            "task={kind}; task-site={}",
            location(task.location.as_ref())
        ))
    }
}

/// Source operand and displayed lock type, retaining the read/write mode.
fn locked_label(cause: Option<&willow_compiler::ai::evidence::Cause>) -> Option<String> {
    let lock = cause?.lock.as_ref()?;
    let mut label = match (lock.expression.as_deref(), lock.type_display.as_deref()) {
        (Some(expression), Some(ty)) => format!("{expression}: {ty}"),
        (Some(expression), None) => expression.to_owned(),
        (None, Some(ty)) => ty.to_owned(),
        (None, None) => "unknown".to_owned(),
    };
    if let Some(mode) = lock.mode.qualifier() {
        label.push_str(&format!(" ({mode})"));
    }
    Some(label)
}

fn effect_call_chain(fact: &willow_compiler::ai::evidence::ExplainedEffect) -> Option<String> {
    fact.call_chain
        .as_ref()
        .filter(|chain| chain.len() > 1)
        .map(|chain| chain.join(" -> "))
}

// Preserve input order while indexing typed site identities and reasons: O(input bytes).
fn compact_effect_evidence(
    result: &willow_compiler::ai::query_result::EffectResult,
    target: &str,
    all: bool,
    lines: &mut Vec<String>,
) {
    use std::collections::{HashMap, HashSet};
    use willow_compiler::ai::evidence::WitnessKind;
    const KEYS: [&str; 9] = [
        "status",
        "kind",
        "via",
        "target",
        "owner",
        "operation",
        "locked",
        "task",
        "chain",
    ];
    #[derive(Default)]
    struct Entry {
        bits: u8,
        fields: [Vec<String>; KEYS.len()],
        location: Option<String>,
        reasons: Vec<usize>,
        seen: HashSet<(usize, String)>,
        seen_reasons: HashSet<usize>,
    }
    let mut identities = HashMap::new();
    let mut entries: Vec<Entry> = Vec::new();
    let mut reason_ids = HashMap::new();
    let mut reasons = Vec::new();
    let explained = result
        .effect_evidence
        .iter()
        .flatten()
        .fold(0, |bits, fact| bits | fact.effect);
    for fact in result
        .effect_evidence
        .iter()
        .flatten()
        .chain(result.compiler_witnesses.iter().flatten().flatten())
    {
        if fact.paths.is_some() {
            continue;
        }
        let witness = fact.witness.as_ref();
        if witness.is_some_and(|witness| witness.kind == WitnessKind::Unavailable)
            && fact.effect & !explained == 0
        {
            continue;
        }
        let cause = witness.and_then(|witness| witness.cause.as_ref());
        let index = if let Some(cause) = cause
            && let Some(at) = cause.location()
        {
            *identities.entry((&cause.operation, at)).or_insert_with(|| {
                entries.push(Entry::default());
                entries.len() - 1
            })
        } else {
            entries.push(Entry::default());
            entries.len() - 1
        };
        if !all && index >= 50 {
            continue;
        }
        let entry = &mut entries[index];
        entry.bits |= fact.effect;
        let locked = locked_label(cause);
        let chain = effect_call_chain(fact);
        let task = task_relationship_label(fact);
        for (key, value) in [
            fact.status.as_ref().map(|status| status.as_str()),
            witness.map(|witness| witness.kind.as_str()),
            fact.via_function.as_deref(),
            witness.and_then(|witness| witness.target.as_deref()),
            witness.and_then(|witness| witness.owner.as_deref()),
            cause.map(|cause| cause.operation.as_str()),
            locked.as_deref(),
            task.as_deref()
                .map(|label| label.trim_start_matches("task=")),
            chain.as_deref(),
        ]
        .into_iter()
        .enumerate()
        {
            if let Some(value) = value
                && entry.seen.insert((key, value.to_owned()))
            {
                entry.fields[key].push(value.to_owned());
            }
        }
        if entry.location.is_none()
            && let Some(at) = cause.and_then(|cause| cause.location())
        {
            entry.location = Some(location(Some(at)));
        }
        for reason in [
            fact.reason.as_deref(),
            witness.and_then(|witness| witness.reason.as_deref()),
        ]
        .into_iter()
        .flatten()
        {
            let id = *reason_ids.entry(reason).or_insert_with(|| {
                reasons.push(reason);
                reasons.len()
            });
            if entry.seen_reasons.insert(id) {
                entry.reasons.push(id);
            }
        }
    }
    lines.push(format!("Evidence for {target}: {}", entries.len()));
    let shown = if all {
        entries.len()
    } else {
        entries.len().min(50)
    };
    for entry in &entries[..shown] {
        let mut details: Vec<String> = KEYS
            .iter()
            .zip(&entry.fields)
            .filter(|(_, values)| !values.is_empty())
            .map(|(key, values)| format!("{key}={}", values.join(", ")))
            .collect();
        details.extend(entry.location.iter().map(|at| format!("at {at}")));
        details.extend(entry.reasons.iter().map(|id| format!("reason=[{id}]")));
        lines.push(format!(
            "  {}: {}",
            effect_names(Some(entry.bits)),
            details.join("; ")
        ));
    }
    if shown < entries.len() {
        lines.push(format!(
            "{} omitted; use --all to show all evidence",
            entries.len() - shown
        ));
    }
    for (index, reason) in reasons.iter().enumerate() {
        lines.push(format!("  [{}] {reason}", index + 1));
    }
}

// A boundary ends a complete human entry or JSON field/array element.
// Offsets are bytes for slicing; budgets count Unicode scalar values.
struct Boundary {
    end: usize,
    chars: usize,
    closing: usize,
}

fn marker(json: bool, omitted: usize, has_items: bool) -> String {
    if json {
        format!(
            "{}\"truncated\":true,\"omitted\":{omitted}}}\n",
            if has_items { "," } else { "{" }
        )
    } else {
        format!("... {omitted} more\n")
    }
}

fn capped_prefix(text: &str, boundaries: &[Boundary], limit: usize, json: bool) -> Result<String> {
    if text.chars().count() <= limit {
        return Ok(text.to_owned());
    }
    let total = boundaries.len();
    let empty = marker(json, total, false);
    anyhow::ensure!(
        empty.chars().count() <= limit,
        "--max-chars {limit} cannot fit omission metadata (requires at least {} characters)",
        empty.chars().count()
    );
    let mut chosen = None;
    let marker_base = marker(json, 0, true).len() - 1;
    for (index, boundary) in boundaries.iter().enumerate() {
        let omitted = total - index - 1;
        let digits = omitted.checked_ilog10().unwrap_or(0) as usize + 1;
        // JSON's marker supplies the root closing brace; human entries include newline.
        let needed = boundary.chars + boundary.closing.saturating_sub(1) + marker_base + digits;
        if needed > limit {
            break;
        }
        chosen = Some((index, boundary));
    }
    let Some((index, boundary)) = chosen else {
        return Ok(empty);
    };
    let mut output = text[..boundary.end].to_owned();
    if json {
        // Recover only the selected prefix's open containers, without retaining
        // a copy of the nesting stack for every item (which would cost O(items * depth)).
        let mut closing = Vec::new();
        let mut quoted = false;
        let mut escaped = false;
        for byte in output.bytes() {
            if quoted {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quoted = false;
                }
            } else {
                match byte {
                    b'"' => quoted = true,
                    b'{' => closing.push('}'),
                    b'[' => closing.push(']'),
                    b'}' | b']' => {
                        closing.pop();
                    }
                    _ => {}
                }
            }
        }
        // Keep the root open for the truncation metadata.
        for close in closing.into_iter().skip(1).rev() {
            output.push(close);
        }
    }
    output.push_str(&marker(json, total - index - 1, true));
    Ok(output)
}

fn json_items(
    value: &Value,
    text: &mut String,
    boundaries: &mut Vec<Boundary>,
    chars: &mut usize,
    depth: usize,
) {
    let start = text.len();
    match value {
        Value::Object(fields) if !fields.is_empty() => {
            text.push('{');
            *chars += 1;
            for (index, (key, value)) in fields.iter().enumerate() {
                let start = text.len();
                if index > 0 {
                    text.push(',');
                }
                text.push_str(&serde_json::to_string(key).unwrap());
                text.push(':');
                *chars += text[start..].chars().count();
                json_items(value, text, boundaries, chars, depth + 1);
            }
            text.push('}');
            *chars += 1;
        }
        Value::Array(items) if !items.is_empty() => {
            text.push('[');
            *chars += 1;
            for (index, item) in items.iter().enumerate() {
                let start = text.len();
                if index > 0 {
                    text.push(',');
                }
                text.push_str(&serde_json::to_string(item).unwrap());
                *chars += text[start..].chars().count();
                boundaries.push(Boundary {
                    end: text.len(),
                    chars: *chars,
                    closing: depth + 1,
                });
            }
            text.push(']');
            *chars += 1;
        }
        _ => {
            text.push_str(&serde_json::to_string(value).unwrap());
            *chars += text[start..].chars().count();
            boundaries.push(Boundary {
                end: text.len(),
                chars: *chars,
                closing: depth,
            });
        }
    }
}

fn render_json(value: &Value, limit: Option<usize>) -> Result<String> {
    let Some(limit) = limit else {
        return Ok(format!("{}\n", serde_json::to_string(value)?));
    };
    let mut text = String::new();
    let mut boundaries = Vec::new();
    let mut chars = 0;
    json_items(value, &mut text, &mut boundaries, &mut chars, 0);
    text.push('\n');
    capped_prefix(&text, &boundaries, limit, true)
}
fn render(value: &CommandOutput, options: &Options) -> Result<String> {
    if options.format != OutputFormat::Human {
        return render_json(&serde_json::to_value(value)?, options.max_chars);
    }
    let Some(limit) = options.max_chars else {
        return Ok(format!("{}\n", human(value, options)));
    };
    let mut text = String::new();
    let mut boundaries = Vec::new();
    let mut chars = 0;
    for item in human_items(value, options) {
        chars += item.chars().count() + 1;
        text.push_str(&item);
        text.push('\n');
        boundaries.push(Boundary {
            end: text.len(),
            chars,
            closing: 0,
        });
    }
    capped_prefix(&text, &boundaries, limit, false)
}

pub(super) fn run(command: &str, parsed: Result<Options>, human_errors: bool) -> Result<i32> {
    let mut warnings = 0;
    let (mut value, code) = match &parsed {
        Ok(options) => match options.execute(&mut warnings) {
            Ok(value) => {
                let code = if value.status == Status::Ok { 0 } else { 1 };
                (value, code)
            }
            Err(error) => {
                let mut value = CommandOutput::new(options.command, Status::Error);
                value.message = Some(format!("{error:#}"));
                if let Some(rejection) =
                    error.downcast_ref::<willow_compiler::ai::edit::Rejection>()
                {
                    let l = &rejection.location;
                    value.location = Some(willow_compiler::ai::edit::RejectionLocation {
                        path: l.path.clone(),
                        start: l.start,
                        end: l.end,
                        line: l.line,
                        column: l.column,
                    });
                }
                (value, 1)
            }
        },
        Err(error) => {
            let mut value = CommandOutput::new(
                DirectCommand::parse(command).expect("direct dispatch validates command"),
                Status::InvalidArguments,
            );
            value.message = Some(format!("{error:#}"));
            (value, 2)
        }
    };
    if let Ok(options) = &parsed {
        if warnings > 0 {
            value.diagnostics = Some(WarningCounts { warnings });
        }
        match render(&value, options) {
            Ok(output) => print!("{output}"),
            Err(error) => {
                eprintln!("{error:#}");
                return Ok(2);
            }
        }
        if warnings > 0 && !options.warnings && options.format == OutputFormat::Human {
            eprintln!(
                "note: {} suppressed; rerun with --warnings to show them",
                counted(warnings as u64, "checker warning")
            );
        }
    } else if human_errors {
        eprintln!(
            "{}",
            value.message.as_deref().unwrap_or("invalid arguments")
        );
    } else {
        println!("{}", serde_json::to_string(&serde_json::to_value(&value)?)?);
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    use willow_compiler::ai::{
        self,
        evidence::{Cause, ExplainedEffect},
        query_result::{EffectBounds, EffectResult, UnknownCause},
    };

    // Fixtures omit byte offsets when only presentation coordinates matter.
    fn fixture_locations(value: &mut Value) {
        match value {
            Value::Object(object) => {
                if object.get("path").is_some_and(Value::is_string) {
                    object.entry("start").or_insert(json!(0));
                    object.entry("end").or_insert(json!(0));
                }
                for value in object.values_mut() {
                    fixture_locations(value);
                }
            }
            Value::Array(values) => {
                for value in values {
                    fixture_locations(value);
                }
            }
            _ => {}
        }
    }
    fn fixture_location(value: &Value) -> Option<ai::Location> {
        if value.is_null() {
            return None;
        }
        let mut value = value.clone();
        fixture_locations(&mut value);
        Some(serde_json::from_value(value).unwrap())
    }
    fn fixture_cause(value: &Value) -> Cause {
        let mut value = value.clone();
        fixture_locations(&mut value);
        serde_json::from_value(value).unwrap()
    }
    fn fixture_fact(value: &Value) -> ExplainedEffect {
        let witness = value.get("witness").filter(|v| !v.is_null()).map(|v| {
            let mut v = v.clone();
            v.as_object_mut()
                .unwrap()
                .entry("kind")
                .or_insert(json!("unknown"));
            fixture_locations(&mut v);
            serde_json::from_value(v).unwrap()
        });
        ExplainedEffect {
            effect: value["effect"].as_u64().unwrap() as u8,
            status: value["status"].as_str().map(Into::into),
            witness,
            via_function: value["via_function"].as_str().map(str::to_owned),
            reason: value["reason"].as_str().map(str::to_owned),
            paths: None,
            task_relationship: None,
            call_chain: None,
        }
    }
    fn fixture_effects(value: &Value) -> EffectResult {
        EffectResult {
            schema: "willow.effects.v3",
            effects: Vec::new(),
            reason: None,
            assumptions: None,
            unknown_causes: value["unknown_causes"].as_array().map(|causes| {
                causes
                    .iter()
                    .map(|cause| UnknownCause {
                        function: String::new(),
                        name: cause["name"].as_str().unwrap().into(),
                        module: String::new(),
                        locations: cause["locations"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|location| fixture_location(location).unwrap())
                            .collect(),
                        unresolved: Vec::new(),
                        call_sites: None,
                        caller_location: fixture_location(&cause["caller_location"]),
                        reason: None,
                    })
                    .collect()
            }),
            bounds: value.get("bounds").map(|bounds| EffectBounds {
                runtime: bounds["runtime"].as_u64().unwrap() as u8,
                compiler: bounds["compiler"].as_u64().map(|bits| bits as u8),
            }),
            effect_evidence: value["effect_evidence"]
                .as_array()
                .map(|facts| facts.iter().map(fixture_fact).collect()),
            compiler_witnesses: value.get("compiler_witnesses").map(|facts| {
                facts
                    .as_array()
                    .map(|facts| facts.iter().map(fixture_fact).collect())
            }),
            no_panic: None,
            witness: None,
            meaning: value["meaning"].as_str().map(|meaning| {
                assert_eq!(meaning, "");
                ""
            }),
        }
    }
    fn fixture_selected() -> willow_compiler::ai::direct::SelectedSymbol {
        use willow_compiler::ai::direct::SelectedSymbol;
        SelectedSymbol {
            id: "α猫".into(),
            identity: None,
            kind: SymbolKind::Function,
            location: None,
            name: "α猫".into(),
            qualified_name: None,
            selector: "α猫".into(),
        }
    }
    fn fixture_diff(diffs: Vec<&str>) -> CommandOutput {
        let mut output = CommandOutput::new(DirectCommand::Rename, Status::Ok);
        output.result = Some(CommandResult::Diff {
            preview: RenamePreviewResult {
                preview: ai::edit::RenamePreview {
                    status: Status::Ok,
                    files_changed: 1,
                    work: Default::default(),
                    dry_run: true,
                    revision: String::new(),
                    changes: Some(
                        diffs
                            .into_iter()
                            .map(|diff| ai::edit::DiffChange {
                                path: "α猫.wi".into(),
                                diff: diff.into(),
                            })
                            .collect(),
                    ),
                },
                edits: 3,
                declarations_updated: 1,
                references_updated: 2,
                selected: fixture_selected(),
            },
            summary: RenameSummary {
                files: 1,
                edits: 3,
                declarations: 1,
                references: 2,
                collisions: 0,
                unresolved: 0,
            },
        });
        output
    }
    fn fixture_command(command: DirectCommand) -> CommandOutput {
        if command == DirectCommand::Rename {
            return fixture_diff(vec!["--- α猫\n+++ β犬\n- old\n+ new\n"]);
        }
        let data = if command == DirectCommand::Impact {
            DirectQueryData::Impact {
                symbol: fixture_selected(),
                impact: ai::Impact {
                    schema: "willow.impact.v1",
                    rust_dependencies: Vec::new(),
                    dependency_boundary: Vec::new(),
                    external_dependencies: Vec::new(),
                    revision: String::new(),
                    nodes: ["α猫", "β犬"]
                        .into_iter()
                        .map(|name| ai::ImpactNode {
                            identity: None,
                            id: name.into(),
                            level: 0,
                            distance: 0,
                            graph_distance: 0,
                            effects: Vec::new(),
                            selector: Some(name.into()),
                            kind: None,
                            relation: None,
                        })
                        .collect(),
                    truncated: false,
                    unknown: false,
                    reason: None,
                    edge_visits: 0,
                },
            }
        } else {
            DirectQueryData::Unknown {
                suggestions: vec!["α猫".into(), "β犬".into()],
            }
        };
        let mut result = DirectQueryResult::new(Status::Ok, data);
        result.selected = Some(fixture_selected());
        result.type_display = Some("α猫\"\\[]{}".into());
        let mut output = CommandOutput::new(command, Status::Ok);
        output.result = Some(CommandResult::Query(Box::new(result)));
        output
    }
    #[test]
    fn max_chars_all_commands_unicode_boundaries_and_compatibility() {
        for command in ["refs", "symbol", "type", "effects", "impact", "rename"] {
            let mut args = vec![command.into(), "α猫".into()];
            if command == "rename" {
                args.extend(["new".into(), "--dry-run".into(), "--diff".into()]);
            }
            let mut options = Options::parse(&args).unwrap();
            let value = json!({"schema":1,"kind":command,"status":"ok","result":{
                "files":1,"references":2,"collisions":0,"unresolved":0,
                "changes":[{"diff":"--- α猫\n+++ β犬\n- old\n+ new\n"}],
                "type_display":"α猫\"\\[]{}", "selected":{"selector":"α猫","kind":"function"},
                "suggestions":["α猫","β犬"], "impact":{"nodes":[{"selector":"α猫"},{"selector":"β犬"}]}
            }});
            let typed = fixture_command(options.command);
            for format in ["human", "json", "ndjson"] {
                options.format = OutputFormat::parse(format).unwrap();
                options.max_chars = None;
                let baseline = if format == "human" {
                    render(&typed, &options)
                } else {
                    render_json(&value, options.max_chars)
                }
                .unwrap();
                let expected = if format == "human" {
                    human(&typed, &options)
                } else {
                    serde_json::to_string(&value).unwrap()
                };
                assert_eq!(baseline, format!("{expected}\n"));
                let len = baseline.chars().count();
                for cap in 0..=len + 1 {
                    options.max_chars = Some(cap);
                    match if format == "human" {
                        render(&typed, &options)
                    } else {
                        render_json(&value, options.max_chars)
                    } {
                        Err(_) => assert!(cap < 40),
                        Ok(output) => {
                            assert!(
                                output.chars().count() <= cap,
                                "{command} {format} {cap}: {output}"
                            );
                            assert!(output.ends_with('\n'));
                            if cap >= len {
                                assert_eq!(output, baseline);
                            } else if format == "human" {
                                let (prefix, marker) = output.rsplit_once("... ").unwrap();
                                assert!(baseline.starts_with(prefix));
                                assert!(marker.ends_with(" more\n"));
                                let omitted: usize =
                                    marker.trim_end_matches(" more\n").parse().unwrap();
                                assert!(omitted > 0);
                                if command == "rename" && prefix.contains("---") {
                                    assert!(prefix.contains("+ new\n"));
                                }
                            } else {
                                let parsed: Value = serde_json::from_str(&output).unwrap();
                                assert_eq!(parsed["truncated"], true);
                                assert!(parsed["omitted"].as_u64().unwrap() > 0);
                                if let Some(items) = parsed["result"]["changes"].as_array() {
                                    assert_eq!(items[0], value["result"]["changes"][0]);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn max_chars_item_counts_scale_linearly() {
        for count in [1, 16, 64, 256, 1024] {
            let value = json!({"result":{"items":(0..count).map(|_| json!({"text":"猫\"{}[]\\","nested":[1,2]})).collect::<Vec<_>>()}});
            let mut text = String::new();
            let mut boundaries = Vec::new();
            let mut chars = 0;
            json_items(&value, &mut text, &mut boundaries, &mut chars, 0);
            assert_eq!(text, serde_json::to_string(&value).unwrap());
            assert_eq!(chars, text.chars().count());
            assert_eq!(boundaries.len(), count);
            assert!(boundaries.iter().all(|b| b.closing == 3));
            text.push('\n');
            let output = capped_prefix(&text, &boundaries, 160, true).unwrap();
            let parsed: Value = serde_json::from_str(&output).unwrap();
            let kept = parsed["result"]["items"].as_array().map_or(0, Vec::len);
            if count > 1 {
                assert_eq!(parsed["omitted"].as_u64().unwrap() as usize + kept, count);
            }
        }
    }

    #[test]
    fn max_chars_empty_containers_deep_objects_and_exact_marker() {
        for depth in [1, 8, 32, 64] {
            let mut value = json!({"a":[],"b":{},"c":[{"escaped":"\"\\[]{}猫"},{}],"d":null});
            for _ in 0..depth {
                value = json!({"nested":value});
            }
            let mut text = String::new();
            let mut boundaries = Vec::new();
            let mut chars = 0;
            json_items(&value, &mut text, &mut boundaries, &mut chars, 0);
            text.push('\n');
            assert_eq!(boundaries.len(), 5);
            for cap in 0..=chars + 1 {
                let result = capped_prefix(&text, &boundaries, cap, true);
                if cap < marker(true, 5, false).len() {
                    assert!(result.is_err());
                } else {
                    let output = result.unwrap();
                    assert!(output.chars().count() <= cap);
                    let parsed: Value = serde_json::from_str(&output).unwrap();
                    if cap == chars + 1 {
                        assert_eq!(parsed, value);
                    } else {
                        assert_eq!(parsed["truncated"], true);
                    }
                }
            }
        }
        let options =
            Options::parse(&["rename", "old", "new", "--dry-run", "--diff"].map(str::to_owned))
                .unwrap();
        for (diffs, expected) in [
            (vec![], ""),
            (vec![""], "\n"),
            (vec!["", ""], "\n"),
            (vec!["one", "", "two\n\n"], "one\ntwo\n\n"),
        ] {
            let value = fixture_diff(diffs);
            assert_eq!(
                human(&value, &options),
                format!(
                    "{expected}1 file / 3 edits (1 declaration, 2 references) / 0 collisions / 0 unresolved"
                )
            );
        }
    }

    #[test]
    fn max_chars_parser_rejects_invalid_and_mutating_rename() {
        for extra in [
            vec!["--max-chars"],
            vec!["--max-chars=-1"],
            vec!["--max-chars=abc"],
            vec!["--max-chars=1", "--max-chars=2"],
        ] {
            let args: Vec<_> = [vec!["refs", "foo"], extra]
                .concat()
                .into_iter()
                .map(str::to_owned)
                .collect();
            assert!(Options::parse(&args).is_err());
        }
        assert!(
            Options::parse(&["rename", "foo", "bar", "--max-chars=100"].map(str::to_owned))
                .is_err()
        );
        assert_eq!(
            Options::parse(&["refs", "foo", "--max-chars", "100"].map(str::to_owned))
                .unwrap()
                .max_chars,
            Some(100)
        );
    }

    #[test]
    fn type_failure_kinds_have_distinct_explanations() {
        let options = Options::parse(&["type".into(), "main.wi:1:1".into()]).unwrap();
        for (status, reason) in [
            (Status::Unknown, "No typed expression"),
            (Status::Unanalyzed, "type is unavailable"),
            (Status::Ambiguous, "Multiple semantic candidates"),
        ] {
            let mut value = CommandOutput::new(DirectCommand::Type, status);
            value.result = Some(CommandResult::Query(Box::new(DirectQueryResult::new(
                status,
                DirectQueryData::Empty {},
            ))));
            let text = human(&value, &options);
            assert!(text.contains(reason), "{text}");
        }
    }
    #[test]
    fn unknown_effect_origins_are_bounded_and_missing_source_is_explicit() {
        for count in [0, 1, 16, 64, 256] {
            let causes: Vec<_> = (0..count)
                .map(|i| {
                    json!({"name":format!("m::callee{i}"),
                "locations":[], "caller_location":{"path":"m.wi","line":i+1,"column":1}})
                })
                .collect();
            for all in [false, true] {
                let mut lines = Vec::new();
                effect_lines(
                    &fixture_effects(&json!({"unknown_causes":causes})),
                    None,
                    all,
                    false,
                    &mut lines,
                );
                let shown = if all { count } else { count.min(50) };
                assert_eq!(
                    lines
                        .iter()
                        .filter(|l| l.starts_with("Incomplete effect summary: m::callee"))
                        .count(),
                    shown
                );
                assert_eq!(
                    lines
                        .iter()
                        .filter(|l| l.contains("No declaration source"))
                        .count(),
                    shown
                );
                assert_eq!(
                    lines
                        .iter()
                        .filter(|l| l.contains("Referenced from caller at m.wi:"))
                        .count(),
                    shown
                );
                assert_eq!(
                    lines
                        .iter()
                        .any(|l| l.contains("unknown causes omitted; use --all")),
                    !all && count > 50
                );
            }
        }
    }
    #[test]
    fn lock_evidence_names_locked_value_type_and_mode() {
        let at = json!({"path":"arena.wi","line":116,"column":9});
        let lock = |mode: &str, expression: Option<&str>, ty: Option<&str>| {
            let mut lock = json!({"mode":mode,"target":at});
            if let Some(expression) = expression {
                lock["expression"] = json!(expression);
            }
            if let Some(ty) = ty {
                lock["type_display"] = json!(ty);
            }
            json!({"operation":"lock","location":at,"lock":lock})
        };
        for (cause, label) in [
            (
                lock("lock", Some("seen"), Some("Mutex<Map<i64, i64>>")),
                "seen: Mutex<Map<i64, i64>>",
            ),
            (
                lock("lock read", Some("self.cache"), Some("RwLock<Cfg>")),
                "self.cache: RwLock<Cfg> (read)",
            ),
            (
                lock("lock write", Some("h.cache"), Some("RwLock<Cfg>")),
                "h.cache: RwLock<Cfg> (write)",
            ),
            (lock("lock", Some("m"), None), "m"),
            (lock("lock", None, Some("Mutex<i64>")), "Mutex<i64>"),
            (lock("lock read", None, None), "unknown (read)"),
        ] {
            assert_eq!(
                locked_label(Some(&fixture_cause(&cause))),
                Some(label.into())
            );
            // Compact and explained renderings both name the value once,
            // even when runtime and compiler evidence merge at one site.
            let fact = json!({"effect":4,"status":"compiler-fact",
                "witness":{"kind":"wait","owner":"arena::play","cause":cause}});
            let value = json!({"effect_evidence":[fact],"compiler_witnesses":[fact],
                "bounds":{"runtime":4,"compiler":4},"meaning":""});
            let mut lines = Vec::new();
            compact_effect_evidence(&fixture_effects(&value), "arena::play", false, &mut lines);
            let line = &lines[1];
            assert!(
                line.contains(&format!(
                    "operation=lock; locked={label}; at arena.wi:116:9"
                )),
                "{line}"
            );
            assert_eq!(line.matches("locked=").count(), 1, "{line}");
            let mut lines = Vec::new();
            effect_lines(&fixture_effects(&value), None, false, true, &mut lines);
            let explained: Vec<_> = lines
                .iter()
                .filter(|l| l.contains("operation=lock"))
                .collect();
            assert_eq!(explained.len(), 2, "{lines:?}");
            assert!(
                explained
                    .iter()
                    .all(|l| l.contains(&format!("; locked={label} at arena.wi:116:9"))),
                "{lines:?}"
            );
        }
        // Other waits carry no lock and render no `locked=` field.
        assert!(locked_label(Some(&fixture_cause(&json!({"operation":"await"})))).is_none());
        let fact = json!({"effect":4,"witness":{"kind":"wait",
            "cause":{"operation":"await","location":at}}});
        let mut lines = Vec::new();
        compact_effect_evidence(
            &fixture_effects(&json!({"effect_evidence":[fact]})),
            "f",
            false,
            &mut lines,
        );
        assert!(!lines.join("\n").contains("locked="), "{lines:?}");
    }

    #[test]
    fn compact_effect_evidence_merges_sites_and_interns_reasons() {
        for count in [1, 16, 64, 256, 1024] {
            let facts: Vec<_> = (0..count).map(|i| json!({"effect":1,
                "witness":{"kind":"runtime-capability","reason":"shared reason",
                    "cause":{"operation":"send","location":{"path":"pool.wi","line":i+1,"column":1}}}})).collect();
            let mut runtime = facts.clone();
            for fact in &mut runtime {
                fact["effect"] = json!(4);
            }
            let value = json!({"effect_evidence":runtime,"compiler_witnesses":facts});
            for all in [false, true] {
                let mut lines = Vec::new();
                compact_effect_evidence(&fixture_effects(&value), "pool::worker", all, &mut lines);
                let text = lines.join("\n");
                let shown = if all { count } else { count.min(50) };
                assert_eq!(text.matches("operation=send").count(), shown);
                assert_eq!(text.matches("may-allocate, may-suspend").count(), shown);
                assert_eq!(text.matches("shared reason").count(), 1);
                assert_eq!(text.matches("reason=[1]").count(), shown);
                assert_eq!(lines.len(), shown + 2 + usize::from(shown < count));
            }
        }
        let fact = json!({"effect":1,"reason":"outer","witness":{"reason":"inner",
            "cause":{"operation":"send","location":{"path":"a.wi","line":1,"column":1}}}});
        for count in [1, 16, 64, 256, 1024] {
            let repeated = vec![fact.clone(); count];
            let value = json!({"effect_evidence":repeated,"compiler_witnesses":repeated});
            let mut lines = Vec::new();
            compact_effect_evidence(&fixture_effects(&value), "worker", true, &mut lines);
            assert_eq!(lines.len(), 4); // header, one site, two distinct reasons
            assert_eq!(lines[0], "Evidence for worker: 1");
        }
        let mut other = fact.clone();
        other["witness"]["cause"]["operation"] = json!("recv");
        let mut file = fact.clone();
        file["witness"]["cause"]["location"]["path"] = json!("b.wi");
        let mut column = fact.clone();
        column["witness"]["cause"]["location"]["column"] = json!(2);
        let value = json!({"effect_evidence":[fact,other,file,column,
            {"effect":1,"status":"missing-witness"},{"effect":4,"status":"missing-witness"}]});
        let mut lines = Vec::new();
        compact_effect_evidence(&fixture_effects(&value), "worker", true, &mut lines);
        assert_eq!(lines[0], "Evidence for worker: 6");
        let text = lines.join("\n");
        assert_eq!(text.matches("outer").count(), 1);
        assert_eq!(text.matches("inner").count(), 1);
        assert_eq!(text.matches("reason=[1]; reason=[2]").count(), 4);
        assert_eq!(text.matches("missing-witness").count(), 2);
    }

    #[test]
    fn compact_effect_evidence_lists_each_key_once_per_merged_site() {
        let at = json!({"path":"m.wi","line":2,"column":5});
        let runtime = json!({"effect":8,"status":"conservative-bound","via_function":"m::f",
            "witness":{"kind":"runtime-capability","reason":"parks",
                "cause":{"operation":"await","location":at}}});
        let wait = json!({"effect":4,"status":"compiler-fact","via_function":"m::f",
            "witness":{"kind":"wait","owner":"m::f","reason":"parks",
                "cause":{"operation":"await","location":at}}});
        for (first, second) in [(&runtime, &wait), (&wait, &runtime)] {
            let value =
                json!({"effect_evidence":[first, second, first],"compiler_witnesses":[second]});
            let mut lines = Vec::new();
            compact_effect_evidence(&fixture_effects(&value), "m::f", false, &mut lines);
            assert_eq!(lines.len(), 3, "{lines:?}");
            assert_eq!(lines[0], "Evidence for m::f: 1");
            let line = &lines[1];
            assert!(
                line.starts_with("  may-suspend, may-preempt: status="),
                "{line}"
            );
            for key in [
                "status=",
                "kind=",
                "via=",
                "owner=",
                "operation=",
                " at ",
                "reason=[1]",
            ] {
                assert_eq!(line.matches(key).count(), 1, "{key}: {line}");
            }
            assert!(
                line.contains("status=conservative-bound, compiler-fact")
                    || line.contains("status=compiler-fact, conservative-bound"),
                "{line}"
            );
            assert!(
                line.ends_with("operation=await; at m.wi:2:5; reason=[1]"),
                "{line}"
            );
            assert_eq!(lines[2], "  [1] parks");
        }
    }

    #[test]
    fn unavailable_compiler_witness_is_shown_only_for_unexplained_bits() {
        let site = json!({"effect":32,"status":"conservative-bound","witness":{"kind":"runtime-capability",
            "cause":{"operation":"indirect-call","location":{"path":"main.wi","line":16,"column":45}}}});
        let unavailable = |effect: u64| json!({"effect":effect,"witness":{"kind":"unavailable"}});
        for (compiler, entries) in [
            (vec![unavailable(32)], 1),
            (vec![unavailable(32 | 4)], 2),
            (vec![unavailable(4)], 2),
            (vec![unavailable(32), unavailable(4)], 2),
        ] {
            let value = json!({"effect_evidence":[site.clone()],"compiler_witnesses":compiler});
            let mut lines = Vec::new();
            compact_effect_evidence(&fixture_effects(&value), "main::relay", true, &mut lines);
            assert_eq!(lines[0], format!("Evidence for main::relay: {entries}"));
            assert_eq!(lines.len(), entries + 1);
        }
        let value = json!({"compiler_witnesses":[unavailable(32)]});
        let mut lines = Vec::new();
        compact_effect_evidence(&fixture_effects(&value), "main::relay", true, &mut lines);
        assert_eq!(
            lines,
            [
                "Evidence for main::relay: 1",
                "  may-panic: kind=unavailable"
            ]
        );
    }

    #[test]
    fn effect_evidence_output_is_bounded_and_all_is_linear() {
        for count in [0, 1, 16, 64, 256, 1024] {
            let facts: Vec<_> = (0..count).map(|i| json!({"effect":1,"witness":{"kind":"runtime-capability","cause":{"operation":format!("op-{i}"),"location":{"path":"main.wi","line":i+1,"column":1}}}})).collect();
            let value = json!({"bounds":{"runtime":1,"compiler":0},"compiler_witnesses":facts});
            for all in [false, true] {
                let mut lines = Vec::new();
                effect_lines(&fixture_effects(&value), None, all, false, &mut lines);
                let shown = if all { count } else { count.min(50) };
                assert_eq!(
                    lines
                        .iter()
                        .filter(|l| l.starts_with("  may-allocate:"))
                        .count(),
                    shown
                );
                assert_eq!(
                    lines.iter().any(|l| l.contains("omitted; use --all")),
                    !all && count > 50
                );
                assert_eq!(lines.len(), 1 + shown + usize::from(!all && count > 50));
            }
        }
        let bits = willow_abi::RuntimeEffects::NO_PREEMPT_REGION.bits();
        assert_eq!(effect_names(Some(bits)), "may-run-without-preemption");
        let mut lines = Vec::new();
        effect_lines(
            &fixture_effects(&json!({"bounds":{"runtime":bits,"compiler":0}})),
            None,
            false,
            true,
            &mut lines,
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("does not mean the entire function"))
        );
        assert_eq!(effect_names(Some(0)), "none");
        assert_eq!(effect_names(None), "unavailable");
        assert_eq!(effect_names(Some(63)).split(", ").count(), 6);
    }
}

#[cfg(test)]
mod kind_tests {
    use super::*;

    #[test]
    fn direct_kind_accepts_every_symbol_kind() {
        for kind in SYMBOL_KINDS {
            for inline in [false, true] {
                let mut args = vec!["effects".to_owned(), "main::parallel".to_owned()];
                if inline {
                    args.push(format!("--kind={kind}"));
                } else {
                    args.extend(["--kind".to_owned(), (*kind).to_owned()]);
                }
                let options = Options::parse(&args).unwrap();
                assert_eq!(
                    options.filter.kind.as_ref().map(SymbolKind::as_str),
                    Some(*kind)
                );
            }
        }
    }
}
