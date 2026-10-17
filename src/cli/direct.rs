//! Human-oriented semantic commands; compiler APIs own resolution and queries.
use super::*;
use serde_json::{Value, json};
use willow_compiler::ai::direct::{DirectSession, RenameMode, SelectorFilter};

pub(super) fn requested(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        Some("refs" | "references" | "symbol" | "type" | "effects" | "rename") => true,
        Some("impact") => {
            !args.get(1).is_some_and(|a| {
                a.ends_with(".wi") || matches!(a.as_str(), "." | "..") || PathBuf::from(a).is_dir()
            }) && !args.iter().any(|a| {
                matches!(
                    a.split('=').next(),
                    Some("--function" | "--revision" | "--file" | "--byte" | "--protocol-version")
                )
            })
        }
        _ => false,
    }
}

struct Options {
    command: String,
    selector: String,
    format: String,
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
    max_chars: Option<usize>,
}
impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let mut result = Self {
            command: args[0].clone(),
            selector: String::new(),
            format: "human".into(),
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
            max_chars: None,
        };
        let mut seen = std::collections::HashSet::new();
        let mut i = 1;
        while i < args.len() {
            let (key, inline) = args[i]
                .split_once('=')
                .map_or((args[i].as_str(), None), |(k, v)| (k, Some(v)));
            if !key.starts_with('-') {
                if result.selector.is_empty() {
                    result.selector = args[i].clone();
                } else {
                    anyhow::ensure!(
                        result.command == "rename" && result.new_name.is_none(),
                        "unexpected positional argument"
                    );
                    result.new_name = Some(args[i].clone());
                }
            } else {
                anyhow::ensure!(seen.insert(key), "duplicate option {key}");
                match key {
                    "--all" | "--absolute-paths" | "--explain" | "--show-id" | "--dry-run"
                    | "--verbose" | "--diff" => {
                        anyhow::ensure!(inline.is_none(), "unexpected value for {key}");
                        match key {
                            "--all" => result.all = true,
                            "--absolute-paths" => result.absolute = true,
                            "--explain" => result.explain = true,
                            "--diff" => result.diff = true,
                            "--dry-run" => result.dry_run = true,
                            "--verbose" => result.verbose = true,
                            _ => result.show_id = true,
                        }
                    }
                    "--format" | "--source" | "--project-dir" | "--kind" | "--module"
                    | "--package" | "--max-chars" => {
                        let value = if let Some(value) = inline {
                            value
                        } else {
                            i += 1;
                            args.get(i).context("missing option value")?
                        };
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
                            "--format" => result.format = value.into(),
                            "--source" => result.source = Some(value.into()),
                            "--project-dir" => result.project = Some(value.into()),
                            "--kind" => result.filter.kind = Some(value.into()),
                            "--module" => result.filter.module = Some(value.into()),
                            _ => result.filter.package = Some(value.into()),
                        }
                    }
                    _ => anyhow::bail!("unknown option {key}"),
                }
            }
            i += 1;
        }
        anyhow::ensure!(!result.selector.is_empty(), "missing selector");
        anyhow::ensure!(
            result.command != "rename" || result.new_name.is_some(),
            "rename requires a new name"
        );
        anyhow::ensure!(
            result.command == "rename" || (!result.dry_run && !result.verbose && !result.diff),
            "rename-only option"
        );
        anyhow::ensure!(!result.diff || result.dry_run, "--diff requires --dry-run");
        anyhow::ensure!(
            matches!(result.format.as_str(), "human" | "json" | "ndjson"),
            "format must be human, json or ndjson"
        );
        anyhow::ensure!(
            result.source.is_none() || result.project.is_none(),
            "--source and --project-dir are mutually exclusive"
        );
        anyhow::ensure!(
            result.max_chars.is_none() || result.command != "rename" || result.dry_run,
            "--max-chars requires --dry-run for rename"
        );
        Ok(result)
    }
    fn execute(&self) -> Result<Value> {
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
        let emitter: &mut dyn willow_compiler::diagnostics::DiagnosticEmitter =
            if let Some(root) = root.as_deref() {
                project_emitter = willow_compiler::diagnostics::ProjectEmitter {
                    root,
                    inner: &mut human_emitter,
                };
                &mut project_emitter
            } else {
                &mut human_emitter
            };
        let snapshot = if self.command == "rename" {
            compiler.analysis_for_edit_with_emitter(emitter)?
        } else {
            compiler.overview_with_emitter(emitter)?
        };
        let mut session = DirectSession::new(snapshot)?;
        if self.command == "rename" {
            let mut result = session.rename(
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
            )?;
            if self.dry_run && result["status"] == "ok" {
                let mut summary = json!({
                    "files": result["files_changed"],
                    "references": result["references_updated"],
                    "collisions": 0,
                    "unresolved": 0,
                });
                if self.diff {
                    result
                        .as_object_mut()
                        .unwrap()
                        .extend(summary.as_object().unwrap().clone());
                    summary = result;
                }
                return Ok(json!({"schema":1,"kind":"rename","status":"ok","result":summary}));
            }
            if !self.verbose {
                result.as_object_mut().unwrap().remove("transaction");
            }
            return Ok(
                json!({"schema":1,"kind":"rename","status":result["status"],"result":result}),
            );
        }
        let mut result = session.query(&self.command, &self.selector, &self.filter, self.all)?;
        if matches!(self.command.as_str(), "effects" | "impact") && self.format == "human" {
            session.display_function_targets(&mut result, self.absolute)?;
        }
        session.display_locations(&mut result, self.absolute)?;
        if !(self.explain || self.command == "effects" && self.format == "human") {
            concise(&mut result);
        }
        Ok(
            json!({"schema":1,"kind":self.command,"status":result["status"],"revision":session.session.revision(),"result":result}),
        )
    }
}

fn concise(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for key in [
                "fingerprint",
                "body_fingerprint",
                "flow",
                "node",
                "occurrences",
                "callees",
                "witness",
                "compiler_witnesses",
            ] {
                fields.remove(key);
            }
            if fields.contains_key("type_display") {
                fields.remove("type");
            }
            for value in fields.values_mut() {
                concise(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                concise(value);
            }
        }
        _ => {}
    }
}

fn location(value: &Value) -> String {
    format!(
        "{}:{}:{}",
        value["path"].as_str().unwrap_or("?"),
        value["line"],
        value["column"]
    )
}
fn counted(n: u64, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}
fn human(value: &Value, options: &Options) -> String {
    human_items(value, options).join("\n")
}

fn human_items(value: &Value, options: &Options) -> Vec<String> {
    let result = &value["result"];
    if options.command == "rename" && options.dry_run && value["status"] == "ok" {
        let summary = format!(
            "{} / {} / {} / {} unresolved",
            counted(result["files"].as_u64().unwrap(), "file"),
            counted(result["references"].as_u64().unwrap(), "reference"),
            counted(result["collisions"].as_u64().unwrap(), "collision"),
            result["unresolved"]
        );
        let mut output = Vec::new();
        if let Some(changes) = result["changes"].as_array() {
            for change in changes {
                if let Some(diff) = change["diff"].as_str()
                    && (!diff.is_empty() || output.is_empty())
                {
                    output.push(diff.strip_suffix('\n').unwrap_or(diff).to_owned());
                }
            }
        }
        output.push(summary);
        return output;
    }
    if options.command == "rename" && result["status"] == "ok" {
        let count = |key: &str, noun: &str| {
            let n = result[key].as_u64().unwrap_or(0);
            format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
        };
        let files = count("files_changed", "file");
        let edits = format!(
            "{} ({}, {})",
            count("edits", "edit"),
            count("declarations_updated", "declaration"),
            count("references_updated", "reference")
        );
        let new_name = options.new_name.as_deref().unwrap();
        let mut output = format!(
            "Renamed {} -> {new_name}\n{files} changed\n{edits}\nValidation: passed",
            options.selector
        );
        if let Some(warnings) = result["warnings"].as_array() {
            for warning in warnings.iter().filter_map(Value::as_str) {
                output.push_str(&format!("\nWarning: {warning}"));
            }
        }
        if options.verbose {
            output.push_str(&format!(
                "\nTransaction: {}",
                result["transaction"].as_str().unwrap_or("?")
            ));
        }
        return vec![output];
    }
    let mut lines = vec![format!(
        "{} {}: {}",
        options.command,
        options.selector,
        value["status"].as_str().unwrap_or("error")
    )];
    if let Some(message) = value["message"].as_str() {
        lines.push(message.into());
    }
    if let Some(reason) = result["reason"].as_str() {
        lines.push(reason.into());
    }
    if let Some(candidates) = result["candidates"].as_array() {
        for candidate in candidates {
            lines.push(format!(
                "  {} ({}) {}",
                candidate["selector"].as_str().unwrap_or("?"),
                candidate["kind"].as_str().unwrap_or("?"),
                location(&candidate["location"])
            ));
        }
    }
    if let Some(suggestions) = result["suggestions"].as_array() {
        for suggestion in suggestions {
            lines.push(format!(
                "  suggestion: {}",
                suggestion.as_str().unwrap_or("?")
            ));
        }
    }
    if let Some(references) = result["references"].as_array() {
        for reference in references {
            lines.push(format!(
                "  {} {}{}",
                location(&reference["location"]),
                reference["role"].as_str().unwrap_or("reference"),
                if reference["certainty"] == "possible-dispatch" {
                    " (possible-dispatch)"
                } else {
                    ""
                }
            ));
        }
        lines.push(format!(
            "total={} shown={} truncated={}",
            result["total"], result["shown"], result["truncated"]
        ));
        lines.push(format!(
            "Coverage: {}",
            result["coverage"].as_str().unwrap_or("compiler-resolved")
        ));
    }
    if let Some(candidates) = result["unresolved_candidates"].as_array() {
        for candidate in candidates {
            lines.push(format!(
                "  {} indirect candidate: {}",
                location(&candidate["location"]),
                candidate["reason"].as_str().unwrap_or("unresolved target")
            ));
        }
    }
    if let Some(ty) = result["type_display"].as_str() {
        lines.push(format!("Type: {ty}"));
    }
    if let Some(selected) = result.get("selected").or_else(|| result.get("symbol")) {
        if let Some(name) = selected["selector"].as_str() {
            lines.push(format!(
                "Symbol: {name} ({})",
                selected["kind"].as_str().unwrap_or("function")
            ));
        }
        if selected["location"].is_object() {
            lines.push(location(&selected["location"]));
        }
        if options.show_id {
            lines.push(format!("ID: {}", selected["id"].as_str().unwrap_or("?")));
        }
    }
    if let Some(ty) = result["symbol"]["type_display"]
        .as_str()
        .filter(|_| result["type_display"].is_null())
    {
        lines.push(format!("Type: {ty}"));
    }
    if options.command == "type" {
        match result["status"].as_str() {
            Some("unknown") if willow_compiler::ai::direct::split_location(&options.selector).is_none() => lines.push("No symbol matches this selector and its filters. Use a qualified symbol name or file:line:column on an identifier or expression.".into()),
            Some("unknown") => lines.push("No typed expression or declaration was found at this position (keywords, whitespace and comments have no type). Place the cursor on an expression or identifier, or use `willow type QUALIFIED_SYMBOL`.".into()),
            Some("unanalyzed") => lines.push("A semantic expression or declaration was found, but its type is unavailable in this analysis. Run `willow check .` for diagnostics; try a typed expression or declaration identifier.".into()),
            Some("ambiguous") => lines.push("Multiple semantic candidates overlap this position. Select a declaration identifier or a qualified symbol.".into()),
            _ => {}
        }
    }
    if result["runtime_effects"].is_number() {
        effect_lines(result, options.all, options.explain, &mut lines);
    }
    if let Some(nodes) = result["impact"]["nodes"].as_array() {
        lines.push(format!("Affected functions: {}", nodes.len()));
        for node in nodes.iter().take(if options.all { usize::MAX } else { 50 }) {
            lines.push(format!(
                "  {}",
                node["selector"].as_str().unwrap_or("unknown symbol")
            ));
        }
        lines.push(format!(
            "total={} shown={} truncated={} unknown={}",
            nodes.len(),
            if options.all {
                nodes.len()
            } else {
                nodes.len().min(50)
            },
            result["impact"]["truncated"] == true || (!options.all && nodes.len() > 50),
            result["impact"]["unknown"]
        ));
    }
    if let Some(reason) = result["impact"]["reason"].as_str() {
        lines.push(reason.into());
    }
    if options.explain {
        lines.push(serde_json::to_string_pretty(result).unwrap());
    }
    lines
}

fn effect_names(bits: &Value) -> String {
    use willow_abi::RuntimeEffects as E;
    let Some(bits) = bits.as_u64() else {
        return "unavailable".into();
    };
    let names: Vec<_> = [
        (E::MAY_ALLOCATE, "may-allocate"),
        (E::MAY_BLOCK, "may-block"),
        (E::MAY_SUSPEND, "may-suspend"),
        (E::MAY_PREEMPT, "may-preempt"),
        (E::NO_PREEMPT_REGION, "may-run-without-preemption"),
        (E::MAY_PANIC, "may-panic"),
    ]
    .into_iter()
    .filter_map(|(effect, name)| (bits & u64::from(effect.bits()) != 0).then_some(name))
    .collect();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

fn effect_lines(result: &Value, all: bool, explain: bool, lines: &mut Vec<String>) {
    if let Some(causes) = result["unknown_causes"].as_array() {
        for cause in causes.iter().take(if all { usize::MAX } else { 50 }) {
            lines.push(format!(
                "Incomplete effect summary: {}",
                cause["name"].as_str().unwrap_or("unknown")
            ));
            if let Some(sites) = cause["call_sites"].as_array() {
                for site in sites {
                    lines.push(format!(
                        "  Unresolved call `{}` at {}",
                        site["operation"].as_str().unwrap_or("unknown"),
                        location(&site["location"])
                    ));
                }
            }
            if let Some(locations) = cause["locations"].as_array() {
                for loc in locations {
                    lines.push(format!("  {}", location(loc)));
                }
                if locations.is_empty() {
                    lines.push("  No declaration source is available.".into());
                    if cause["caller_location"].is_object() {
                        lines.push(format!(
                            "  Referenced from caller at {}",
                            location(&cause["caller_location"])
                        ));
                    }
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
    lines.push(format!(
        "Runtime effects: {}",
        effect_names(&result["runtime_effects"])
    ));
    lines.push(format!(
        "Compiler effects: {}",
        effect_names(&result["compiler_effects"])
    ));
    if explain
        && ["runtime_effects", "compiler_effects"].iter().any(|key| {
            result[key].as_u64().is_some_and(|bits| {
                bits & u64::from(willow_abi::RuntimeEffects::NO_PREEMPT_REGION.bits()) != 0
            })
        })
    {
        lines.push("Preemption evidence is conservative (for example, a loop or recursion); it does not mean the entire function disables preemption.".into());
    }
    let target = result["selected"]["selector"]
        .as_str()
        .unwrap_or("selected function");
    if !explain {
        compact_effect_evidence(result, target, all, lines);
        return;
    }
    for (label, key) in [
        ("Runtime evidence", "effect_evidence"),
        (
            "Local evidence (compiler facts and runtime capabilities)",
            "compiler_witnesses",
        ),
    ] {
        let evidence = result[key].as_array().map(Vec::as_slice).unwrap_or(&[]);
        lines.push(format!("{label} for {target}: {}", evidence.len()));
        let shown = if all {
            evidence.len()
        } else {
            evidence.len().min(50)
        };
        for fact in &evidence[..shown] {
            let witness = &fact["witness"];
            let mut detail = format!(
                "  {}: {}",
                effect_names(&fact["effect"]),
                fact["status"]
                    .as_str()
                    .or_else(|| witness["kind"].as_str())
                    .unwrap_or("unavailable")
            );
            for (name, value) in [
                ("via", &fact["via_function"]),
                ("target", &witness["target"]),
                ("owner", &witness["owner"]),
                ("operation", &witness["cause"]["operation"]),
                ("reason", fact.get("reason").unwrap_or(&witness["reason"])),
            ] {
                if let Some(value) = value.as_str() {
                    detail.push_str(&format!("; {name}={value}"));
                }
            }
            if witness["cause"]["location"].is_object() {
                detail.push_str(&format!(" at {}", location(&witness["cause"]["location"])));
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
    lines.push(result["meaning"].as_str().unwrap_or("").into());
}

// Preserve input order while indexing identities and reasons: expected O(input bytes).
fn compact_effect_evidence(result: &Value, target: &str, all: bool, lines: &mut Vec<String>) {
    use std::collections::{HashMap, HashSet};
    #[derive(Default)]
    struct Entry {
        bits: u64,
        details: Vec<String>,
        seen: HashSet<String>,
    }
    let mut identities = HashMap::new();
    let mut entries: Vec<Entry> = Vec::new();
    let mut reason_ids = HashMap::new();
    let mut reasons = Vec::new();
    // An unavailable compiler witness adds nothing for a bit the evidence explains.
    let explained = result["effect_evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .fold(0, |bits, fact| bits | fact["effect"].as_u64().unwrap_or(0));
    for fact in ["effect_evidence", "compiler_witnesses"]
        .into_iter()
        .flat_map(|key| result[key].as_array().into_iter().flatten())
    {
        let witness = &fact["witness"];
        if witness["kind"] == "unavailable"
            && fact["effect"]
                .as_u64()
                .is_some_and(|bits| bits & !explained == 0)
        {
            continue;
        }
        let cause = &witness["cause"];
        let index = if let (Some(operation), true) =
            (cause["operation"].as_str(), cause["location"].is_object())
        {
            *identities
                .entry((operation, cause["location"].to_string()))
                .or_insert_with(|| {
                    entries.push(Entry::default());
                    entries.len() - 1
                })
        } else {
            // Missing locations must not merge independent transitive/unknown evidence.
            entries.push(Entry::default());
            entries.len() - 1
        };
        if !all && index >= 50 {
            continue;
        }
        let entry = &mut entries[index];
        entry.bits |= fact["effect"].as_u64().unwrap_or(0);
        for (name, value) in [
            ("status", &fact["status"]),
            ("kind", &witness["kind"]),
            ("via", &fact["via_function"]),
            ("target", &witness["target"]),
            ("owner", &witness["owner"]),
            ("operation", &cause["operation"]),
        ] {
            if let Some(value) = value.as_str() {
                let detail = format!("{name}={value}");
                if entry.seen.insert(detail.clone()) {
                    entry.details.push(detail);
                }
            }
        }
        if cause["location"].is_object() {
            let detail = format!("at {}", location(&cause["location"]));
            if entry.seen.insert(detail.clone()) {
                entry.details.push(detail);
            }
        }
        for reason in [fact["reason"].as_str(), witness["reason"].as_str()]
            .into_iter()
            .flatten()
        {
            let id = *reason_ids.entry(reason).or_insert_with(|| {
                reasons.push(reason);
                reasons.len()
            });
            let detail = format!("reason=[{id}]");
            if entry.seen.insert(detail.clone()) {
                entry.details.push(detail);
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
        lines.push(format!(
            "  {}: {}",
            effect_names(&Value::from(entry.bits)),
            entry.details.join("; ")
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

fn render(value: &Value, options: &Options) -> Result<String> {
    let Some(limit) = options.max_chars else {
        return Ok(format!(
            "{}\n",
            if options.format == "human" {
                human(value, options)
            } else {
                serde_json::to_string(value)?
            }
        ));
    };
    let json = options.format != "human";
    let mut text = String::new();
    let mut boundaries = Vec::new();
    let mut chars = 0;
    if json {
        json_items(value, &mut text, &mut boundaries, &mut chars, 0);
        text.push('\n');
    } else {
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
    }
    capped_prefix(&text, &boundaries, limit, json)
}

pub(super) fn run(args: &[String]) -> Result<i32> {
    let parsed = Options::parse(args);
    let format = args
        .iter()
        .enumerate()
        .find_map(|(i, a)| {
            a.strip_prefix("--format=").or_else(|| {
                (a == "--format")
                    .then(|| args.get(i + 1).map(String::as_str))
                    .flatten()
            })
        })
        .unwrap_or("human");
    let (value, code) = match &parsed {
        Ok(options) => match options.execute() {
            Ok(value) => {
                let code = if value["status"] == "ok" { 0 } else { 1 };
                (value, code)
            }
            Err(error) => {
                let mut value = json!({"schema":1,"kind":options.command,"status":"error","message":format!("{error:#}")});
                if let Some(rejection) =
                    error.downcast_ref::<willow_compiler::ai::edit::Rejection>()
                {
                    value["location"] = json!(rejection.location);
                }
                (value, 1)
            }
        },
        Err(error) => (
            json!({"schema":1,"kind":args[0],"status":"invalid-arguments","message":format!("{error:#}")}),
            2,
        ),
    };
    if let Ok(options) = &parsed {
        match render(&value, options) {
            Ok(output) => print!("{output}"),
            Err(error) => {
                eprintln!("{error:#}");
                return Ok(2);
            }
        }
    } else if format == "human"
        || args
            .iter()
            .any(|a| a == "--max-chars" || a.starts_with("--max-chars="))
    {
        eprintln!(
            "{}",
            value["message"].as_str().unwrap_or("invalid arguments")
        );
    } else {
        println!("{}", serde_json::to_string(&value)?);
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
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
            for format in ["human", "json", "ndjson"] {
                options.format = format.into();
                options.max_chars = None;
                let baseline = render(&value, &options).unwrap();
                let expected = if format == "human" {
                    human(&value, &options)
                } else {
                    serde_json::to_string(&value).unwrap()
                };
                assert_eq!(baseline, format!("{expected}\n"));
                let len = baseline.chars().count();
                for cap in 0..=len + 1 {
                    options.max_chars = Some(cap);
                    match render(&value, &options) {
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
            let value = json!({"status":"ok","result":{"files":1,"references":2,"collisions":0,"unresolved":0,"changes":diffs.into_iter().map(|diff| json!({"diff":diff})).collect::<Vec<_>>()}});
            assert_eq!(
                human(&value, &options),
                format!("{expected}1 file / 2 references / 0 collisions / 0 unresolved")
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
            ("unknown", "No typed expression"),
            ("unanalyzed", "type is unavailable"),
            ("ambiguous", "Multiple semantic candidates"),
        ] {
            let text = human(
                &json!({"status":status,"result":{"status":status}}),
                &options,
            );
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
                effect_lines(&json!({"unknown_causes":causes}), all, false, &mut lines);
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
                compact_effect_evidence(&value, "pool::worker", all, &mut lines);
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
            compact_effect_evidence(&value, "worker", true, &mut lines);
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
        compact_effect_evidence(&value, "worker", true, &mut lines);
        assert_eq!(lines[0], "Evidence for worker: 6");
        let text = lines.join("\n");
        assert_eq!(text.matches("outer").count(), 1);
        assert_eq!(text.matches("inner").count(), 1);
        assert_eq!(text.matches("reason=[1]; reason=[2]").count(), 4);
        assert_eq!(text.matches("missing-witness").count(), 2);
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
            compact_effect_evidence(&value, "main::relay", true, &mut lines);
            assert_eq!(lines[0], format!("Evidence for main::relay: {entries}"));
            assert_eq!(lines.len(), entries + 1);
        }
        let value = json!({"compiler_witnesses":[unavailable(32)]});
        let mut lines = Vec::new();
        compact_effect_evidence(&value, "main::relay", true, &mut lines);
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
            let value =
                json!({"runtime_effects":1,"compiler_effects":0,"compiler_witnesses":facts});
            for all in [false, true] {
                let mut lines = Vec::new();
                effect_lines(&value, all, false, &mut lines);
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
                assert_eq!(lines.len(), 3 + shown + usize::from(!all && count > 50));
            }
        }
        let bits = willow_abi::RuntimeEffects::NO_PREEMPT_REGION.bits();
        assert_eq!(effect_names(&json!(bits)), "may-run-without-preemption");
        let mut lines = Vec::new();
        effect_lines(
            &json!({"runtime_effects":bits,"compiler_effects":0}),
            false,
            true,
            &mut lines,
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("does not mean the entire function"))
        );
        assert_eq!(effect_names(&json!(0)), "none");
        assert_eq!(effect_names(&Value::Null), "unavailable");
        assert_eq!(effect_names(&json!(63)).split(", ").count(), 6);
    }
}
