//! Human-oriented semantic commands; compiler APIs own resolution and queries.
use super::*;
use serde_json::{Value, json};
use willow_compiler::ai::direct::{DirectSession, SelectorFilter};

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
    verbose: bool,
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
            verbose: false,
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
                    | "--verbose" => {
                        anyhow::ensure!(inline.is_none(), "unexpected value for {key}");
                        match key {
                            "--all" => result.all = true,
                            "--absolute-paths" => result.absolute = true,
                            "--explain" => result.explain = true,
                            "--dry-run" => result.dry_run = true,
                            "--verbose" => result.verbose = true,
                            _ => result.show_id = true,
                        }
                    }
                    "--format" | "--source" | "--project-dir" | "--kind" | "--module"
                    | "--package" => {
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
            result.command == "rename" || (!result.dry_run && !result.verbose),
            "rename-only option"
        );
        anyhow::ensure!(
            matches!(result.format.as_str(), "human" | "json" | "ndjson"),
            "format must be human, json or ndjson"
        );
        anyhow::ensure!(
            result.source.is_none() || result.project.is_none(),
            "--source and --project-dir are mutually exclusive"
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
        let compiler =
            willow_compiler::CompilerSession::new(&entry, "", &CompilerOptions::debug(), root);
        let snapshot = if self.command == "rename" {
            compiler
                .analysis_for_edit_with_emitter(&mut willow_compiler::diagnostics::HumanEmitter)?
        } else {
            compiler.overview_with_emitter(&mut willow_compiler::diagnostics::HumanEmitter)?
        };
        let mut session = DirectSession::new(snapshot)?;
        if self.command == "rename" {
            let mut result = session.rename(
                std::path::Path::new(&entry),
                project,
                &self.selector,
                &self.filter,
                self.new_name.as_deref().unwrap(),
                self.dry_run,
            )?;
            if !self.verbose {
                result.as_object_mut().unwrap().remove("transaction");
            }
            return Ok(
                json!({"schema":1,"kind":"rename","status":result["status"],"result":result}),
            );
        }
        let mut result = session.query(&self.command, &self.selector, &self.filter, self.all)?;
        if self.command == "effects" && self.format == "human" {
            session.display_effect_targets(&mut result);
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
fn human(value: &Value, options: &Options) -> String {
    let result = &value["result"];
    if options.command == "rename" && result["status"] == "ok" {
        if result["dry_run"] == true {
            return result["changes"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|c| c["diff"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
        }
        let mut output = format!(
            "Renamed {} -> {}\n{} files changed\n{} edits ({} declarations, {} references)\nValidation: passed",
            options.selector,
            options.new_name.as_deref().unwrap(),
            result["files_changed"],
            result["edits"],
            result["declarations_updated"],
            result["references_updated"]
        );
        if options.verbose {
            output.push_str(&format!(
                "\nTransaction: {}",
                result["transaction"].as_str().unwrap_or("?")
            ));
        }
        return output;
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
                "  {} {}",
                location(&reference["location"]),
                reference["certainty"]
                    .as_str()
                    .or_else(|| reference["role"].as_str())
                    .unwrap_or("resolved")
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
    if let Some(ty) = result["symbol"]["type_display"].as_str() {
        lines.push(format!("Type: {ty}"));
    }
    if options.command == "type" {
        match result["status"].as_str() {
            Some("unknown") if willow_compiler::ai::direct::split_location(&options.selector).is_none() => lines.push("No symbol matches this selector and its filters. Use a qualified symbol name or file:line:column on an identifier or expression.".into()),
            Some("unknown") => lines.push("No typed expression or declaration was found at this position (keywords and whitespace may have no type). Place the cursor on an expression or identifier, or use `willow type QUALIFIED_SYMBOL`.".into()),
            Some("unanalyzed") => lines.push("A semantic expression or declaration was found, but its type is unavailable in this analysis. Run `willow check .` for diagnostics; try a typed expression or declaration identifier.".into()),
            Some("ambiguous") => lines.push("Multiple semantic candidates overlap this position. Select a declaration identifier or a qualified symbol.".into()),
            _ => {}
        }
    }
    if result["runtime_effects"].is_number() {
        effect_lines(result, options.all, &mut lines);
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
    if options.explain {
        lines.push(serde_json::to_string_pretty(result).unwrap());
    }
    lines.join("\n")
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

fn effect_lines(result: &Value, all: bool, lines: &mut Vec<String>) {
    lines.push(format!(
        "Runtime effects: {}",
        effect_names(&result["runtime_effects"])
    ));
    lines.push(format!(
        "Compiler effects: {}",
        effect_names(&result["compiler_effects"])
    ));
    if ["runtime_effects", "compiler_effects"].iter().any(|key| {
        result[key].as_u64().is_some_and(|bits| {
            bits & u64::from(willow_abi::RuntimeEffects::NO_PREEMPT_REGION.bits()) != 0
        })
    }) {
        lines.push("Preemption evidence is conservative (for example, a loop or recursion); it does not mean the entire function disables preemption.".into());
    }
    let target = result["selected"]["selector"]
        .as_str()
        .unwrap_or("selected function");
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
                ("reason", &witness["reason"]),
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
    if format == "human" {
        if let Ok(options) = parsed {
            println!("{}", human(&value, &options));
        } else {
            eprintln!(
                "{}",
                value["message"].as_str().unwrap_or("invalid arguments")
            );
        }
    } else {
        println!("{}", serde_json::to_string(&value)?);
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn effect_evidence_output_is_bounded_and_all_is_linear() {
        for count in [0, 1, 16, 64, 256, 1024] {
            let facts: Vec<_> = (0..count).map(|i| json!({"effect":1,"witness":{"kind":"runtime-capability","cause":{"operation":format!("op-{i}"),"location":{"path":"main.wi","line":i+1,"column":1}}}})).collect();
            let value =
                json!({"runtime_effects":1,"compiler_effects":0,"compiler_witnesses":facts});
            for all in [false, true] {
                let mut lines = Vec::new();
                effect_lines(&value, all, &mut lines);
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
                assert_eq!(lines.len(), 5 + shown + usize::from(!all && count > 50));
            }
        }
        let bits = willow_abi::RuntimeEffects::NO_PREEMPT_REGION.bits();
        assert_eq!(effect_names(&json!(bits)), "may-run-without-preemption");
        let mut lines = Vec::new();
        effect_lines(
            &json!({"runtime_effects":bits,"compiler_effects":0}),
            false,
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
