//! Source-facing selectors over one immutable compiler query session.
use super::*;
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Default)]
pub struct SelectorFilter {
    pub kind: Option<String>,
    pub module: Option<String>,
    pub package: Option<String>,
}

/// Columns count Unicode scalar values, starting at one; offsets remain UTF-8 bytes.
pub struct SourceIndex {
    text: String,
    lines: Vec<usize>,
    characters: Vec<usize>,
    line_characters: Vec<usize>,
    #[cfg(test)]
    search_steps: std::cell::Cell<usize>,
}
impl SourceIndex {
    pub fn new(text: String) -> Self {
        let mut lines = vec![0];
        lines.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        let characters: Vec<_> = text
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(text.len()))
            .collect();
        let mut line_characters = Vec::with_capacity(lines.len());
        let mut next_line = 0;
        for (i, &byte) in characters.iter().enumerate() {
            if lines.get(next_line) == Some(&byte) {
                line_characters.push(i);
                next_line += 1;
            }
        }
        Self {
            text,
            lines,
            characters,
            line_characters,
            #[cfg(test)]
            search_steps: std::cell::Cell::new(0),
        }
    }
    pub fn byte(&self, line: usize, column: usize) -> Result<usize> {
        let start = *line
            .checked_sub(1)
            .and_then(|i| self.lines.get(i))
            .context("invalid-location: line is out of range")?;
        let end = self.lines.get(line).copied().unwrap_or(self.text.len());
        let text = self.text[start..end]
            .strip_suffix('\n')
            .unwrap_or(&self.text[start..end]);
        let text = text.strip_suffix('\r').unwrap_or(text);
        let column = column
            .checked_sub(1)
            .context("invalid-location: column is zero")?;
        let index = self.line_characters[line - 1]
            .checked_add(column)
            .context("invalid-location: column overflow")?;
        let byte = *self
            .characters
            .get(index)
            .context("invalid-location: column is out of range")?;
        ensure!(
            byte <= start + text.len(),
            "invalid-location: column is out of range"
        );
        Ok(byte)
    }
    pub fn position(&self, byte: usize) -> Result<(usize, usize)> {
        ensure!(
            byte <= self.text.len() && self.text.is_char_boundary(byte),
            "invalid-location: byte is out of range"
        );
        let line = self.lines.partition_point(|&start| {
            #[cfg(test)]
            self.search_steps.set(self.search_steps.get() + 1);
            start <= byte
        }) - 1;
        Ok((
            line + 1,
            self.characters
                .binary_search_by(|offset| {
                    #[cfg(test)]
                    self.search_steps.set(self.search_steps.get() + 1);
                    offset.cmp(&byte)
                })
                .expect("validated UTF-8 boundary")
                - self.line_characters[line]
                + 1,
        ))
    }
}

/// Split from the right so Windows drive letters remain part of the filename.
pub fn split_location(selector: &str) -> Option<(&str, &str, &str)> {
    let (head, column) = selector.rsplit_once(':')?;
    let (file, line) = head.rsplit_once(':')?;
    if file.ends_with(".wi") {
        Some((file, line, column))
    } else {
        None
    }
}

pub struct DirectSession {
    pub session: QuerySession,
    sources: HashMap<String, SourceIndex>,
    function_names: HashMap<String, String>,
    contract_bodies: std::collections::HashSet<String>,
}
fn source_module(path: &str, workspace: &str) -> String {
    let path = Path::new(path)
        .strip_prefix(workspace)
        .unwrap_or(Path::new(path))
        .with_extension("");
    let mut parts: Vec<_> = path.iter().map(|part| part.to_string_lossy()).collect();
    if parts.last().is_some_and(|p| p == "mod") {
        parts.pop();
    }
    parts.join("::")
}
impl DirectSession {
    pub fn new(snapshot: Snapshot) -> Result<Self> {
        let function_names = snapshot
            .functions
            .iter()
            .map(|f| {
                let module = f
                    .identity
                    .as_ref()
                    .map(|i| i.module.clone())
                    .unwrap_or_else(|| source_module(&f.module, &snapshot.workspace));
                (f.id.clone(), format!("{module}::{}", f.name))
            })
            .collect();
        let contract_bodies = snapshot
            .functions
            .iter()
            .filter(|f| {
                f.name
                    .rsplit("::")
                    .next()
                    .is_some_and(|n| n.starts_with("$default$"))
            })
            .map(|f| f.id.clone())
            .collect();
        let mut session = Self {
            session: QuerySession::new(snapshot)?,
            sources: HashMap::new(),
            function_names,
            contract_bodies,
        };
        let names: Vec<_> = session
            .snapshot()
            .semantic
            .symbols
            .iter()
            .filter(|s| s.kind == "method" && session.function_names.contains_key(&s.id))
            .map(|s| (s.id.clone(), session.selector(s)))
            .collect();
        session.function_names.extend(names);
        Ok(session)
    }
    pub fn workspace(&self) -> &str {
        &self.session.snapshot.workspace
    }
    pub fn snapshot(&self) -> &Snapshot {
        &self.session.snapshot
    }
    pub fn rename(
        mut self,
        entry: &Path,
        project: bool,
        selector: &str,
        filter: &SelectorFilter,
        name: &str,
        dry_run: bool,
    ) -> Result<Value> {
        self.session
            .snapshot
            .edit_context
            .as_ref()
            .context("edit requires live analysis inputs")?
            .verify_manifest_entry()?;
        let mut resolved = self.resolve(selector, filter)?;
        if resolved["status"] != "ok" {
            self.display_locations(&mut resolved, false)?;
            return Ok(resolved);
        }
        let id = resolved["symbol"]["id"]
            .as_str()
            .context("resolved symbol lacks identity")?
            .to_owned();
        if !self.function_names.contains_key(&id)
            && !matches!(
                resolved["symbol"]["kind"].as_str(),
                Some("field" | "static-field" | "variant" | "method" | "binding" | "parameter")
            )
        {
            self.display_locations(&mut resolved, false)?;
            let symbol = &resolved["symbol"];
            let loc = &symbol["location"];
            let message = format!(
                "unsupported rename target `{selector}` ({}): rename supports source functions, methods, fields, enum variants and local bindings; this symbol kind is not supported. No files changed",
                symbol["kind"].as_str().unwrap_or("symbol")
            );
            if let (Some(path), Some(start), Some(end), Some(line), Some(column)) = (
                loc["path"].as_str(),
                loc["start"].as_u64(),
                loc["end"].as_u64(),
                loc["line"].as_u64(),
                loc["column"].as_u64(),
            ) {
                return Err(edit::Rejection {
                    message,
                    location: edit::RejectionLocation {
                        path: path.into(),
                        start: start.try_into()?,
                        end: end.try_into()?,
                        line: line.try_into()?,
                        column: column.try_into()?,
                    },
                }
                .into());
            }
            anyhow::bail!(message);
        }
        let request = edit::Request {
            revision: self.session.revision().to_owned(),
            operations: vec![edit::Operation::Rename {
                function: id,
                name: name.to_owned(),
            }],
        };
        if dry_run {
            return edit::preview_analyzed(&self.session.snapshot, request);
        }
        let workspace = edit::Workspace::open(Path::new(self.workspace()))?;
        let preview = workspace.prepare_analyzed(
            entry,
            project,
            request,
            edit::ChangeFormat::Diff,
            self.session.snapshot,
        )?;
        let transaction = preview["transaction"]
            .as_str()
            .context("missing transaction identity")?;
        let mut emitter = crate::diagnostics::HumanEmitter;
        workspace.validate(transaction, &mut emitter)?;
        workspace.apply_with_rollback(transaction, &mut emitter)?;
        let changes = preview["changes"].as_array().map_or(0, Vec::len);
        let edits = preview["work"]["patches"].as_u64().unwrap_or(0);
        let declarations = preview["work"]["declarations"].as_u64().unwrap_or(0);
        let references = edits.saturating_sub(declarations);
        Ok(
            json!({"status":"ok","old":selector,"new":name,"files_changed":changes,"references_updated":references,"declarations_updated":declarations,"edits":edits,"validation":"passed","transaction":transaction}),
        )
    }
    fn source(&mut self, path: &str) -> Result<&SourceIndex> {
        if !self.sources.contains_key(path) {
            let expected = self
                .session
                .snapshot
                .sources
                .get(path)
                .context("invalid-location: file is outside the analyzed workspace")?;
            let text = std::fs::read_to_string(path)?;
            ensure!(
                hash(text.as_bytes()) == *expected,
                "stale: source changed after analysis"
            );
            self.sources.insert(path.to_owned(), SourceIndex::new(text));
        }
        Ok(&self.sources[path])
    }
    pub fn location(&mut self, selector: &str) -> Result<Option<(String, usize)>> {
        let Some((file, line, column)) = split_location(selector) else {
            return Ok(None);
        };
        let line = line.parse().context("invalid-location: invalid line")?;
        let column = column.parse().context("invalid-location: invalid column")?;
        let path = PathBuf::from(file);
        let path = if path.is_absolute() {
            path
        } else {
            Path::new(self.workspace()).join(path)
        };
        let path =
            std::fs::canonicalize(path).context("invalid-location: source file not found")?;
        let path = path
            .to_str()
            .context("invalid-location: non UTF-8 path")?
            .to_owned();
        let byte = self.source(&path)?.byte(line, column)?;
        Ok(Some((path, byte)))
    }
    fn selector(&self, symbol: &symbols::Symbol) -> String {
        if symbol.kind == "method"
            && let (Some(name), Some(location)) = (&symbol.source_name, &symbol.location)
        {
            let module = symbol
                .identity
                .as_ref()
                .map(|i| i.module.clone())
                .unwrap_or_else(|| source_module(&location.path, self.workspace()));
            return format!("{module}::{name}");
        }
        if let Some(identity) = &symbol.identity {
            let name = identity
                .symbol
                .split("::")
                .map(|s| s.split_once(':').map_or(s, |(_, name)| name))
                .collect::<Vec<_>>()
                .join("::");
            format!("{}::{name}", identity.module)
        } else if let Some(name) = self.function_names.get(&symbol.id) {
            name.clone()
        } else if let Some(location) = &symbol.location {
            let module = source_module(&location.path, self.workspace());
            format!(
                "{module}::{}",
                symbol.source_name.as_deref().unwrap_or(&symbol.name)
            )
        } else {
            symbol.name.clone()
        }
    }
    fn missing_symbol_reason(&mut self, path: &str, byte: usize) -> Result<String> {
        let source = self.source(path)?;
        let (line, _) = source.position(byte)?;
        let start = source.lines[line - 1];
        let end = source.lines.get(line).copied().unwrap_or(source.text.len());
        let line_text = source.text[start..end].trim_end_matches(['\r', '\n']);
        let line_end = start + line_text.len();
        let found = match source.text[byte..].chars().next() {
            _ if byte >= line_end => "end of line".to_owned(),
            Some(c) if c.is_whitespace() => "whitespace".to_owned(),
            Some(c) if c.is_alphanumeric() || c == '_' => {
                let word = |c: char| c.is_alphanumeric() || c == '_';
                let token_start = source.text[start..byte]
                    .char_indices()
                    .rev()
                    .find(|&(_, c)| !word(c))
                    .map_or(start, |(i, c)| start + i + c.len_utf8());
                let token_end = source.text[byte..line_end]
                    .char_indices()
                    .find(|&(_, c)| !word(c))
                    .map_or(line_end, |(i, _)| byte + i);
                format!("token `{}`", &source.text[token_start..token_end])
            }
            Some(c) => format!("token `{c}`"),
            None => "end of file".to_owned(),
        };
        let mut reason = format!(
            "No symbol at this position: found {found}. Place the cursor on an identifier."
        );
        let nearest = self
            .snapshot()
            .semantic
            .symbols
            .iter()
            .filter_map(|s| s.location.as_ref())
            .chain(
                self.snapshot()
                    .semantic
                    .references
                    .iter()
                    .map(|r| &r.location),
            )
            .filter(|loc| loc.path == path)
            .map(|loc| (loc.start, loc.end))
            .filter(|&(s, e)| s >= start && e <= line_end && s < e)
            .min_by_key(|&(s, e)| {
                (
                    if byte < s {
                        s - byte
                    } else {
                        byte.saturating_sub(e)
                    },
                    s,
                )
            });
        let source = self.source(path)?;
        if let Some((near_start, near_end)) = nearest
            && let Some(name) = source.text.get(near_start..near_end)
        {
            let (_, column) = source.position(near_start)?;
            reason.push_str(&format!(
                " Nearest symbol on this line: `{name}` at {line}:{column}."
            ));
        }
        Ok(reason)
    }
    pub fn resolve(&mut self, selector: &str, filter: &SelectorFilter) -> Result<Value> {
        let revision = self.session.revision().to_owned();
        let mut location_reason = None;
        let located = if let Some((file, byte)) = self.location(selector)? {
            let mut result = self.session.query(QueryRequest::SymbolAt {
                revision,
                file: file.clone(),
                byte,
            })["result"]
                .take();
            let symbols = result
                .get_mut("resolved_symbols")
                .map(Value::take)
                .or_else(|| result.get_mut("symbols").map(Value::take))
                .unwrap_or_else(|| json!([result["symbol"].take()]));
            let ids: std::collections::HashSet<String> = symbols
                .as_array()
                .into_iter()
                .flatten()
                .filter(|s| s["kind"] != "import" || result.get("resolved_symbols").is_none())
                .filter_map(|s| s["id"].as_str().map(str::to_owned))
                .collect();
            if ids.is_empty() {
                location_reason = Some(self.missing_symbol_reason(&file, byte)?);
            }
            Some(ids)
        } else {
            None
        };
        let mut candidates = Vec::new();
        let mut suggestions = Vec::new();
        let mut scope_fallback = Vec::new();
        let tail = selector.rsplit("::").next().unwrap_or(selector);
        let tail_chars: Vec<_> = tail.chars().collect();
        let selector_scope = selector.rsplit_once("::").map(|(scope, _)| scope);
        for symbol in &self.session.snapshot.semantic.symbols {
            if located
                .as_ref()
                .is_some_and(|ids| !ids.contains(&symbol.id))
            {
                continue;
            }
            if filter.kind.as_ref().is_some_and(|k| k != &symbol.kind)
                || filter.module.as_ref().is_some_and(|m| {
                    symbol
                        .identity
                        .as_ref()
                        .map(|i| i.module.clone())
                        .or_else(|| {
                            symbol
                                .location
                                .as_ref()
                                .map(|l| source_module(&l.path, self.workspace()))
                        })
                        .as_ref()
                        != Some(m)
                })
                || filter.package.as_ref().is_some_and(|p| {
                    symbol
                        .identity
                        .as_ref()
                        .is_none_or(|i| &i.package.name != p)
                })
            {
                continue;
            }
            let qualified = self.selector(symbol);
            let local_name = matches!(symbol.kind.as_str(), "binding" | "parameter")
                .then(|| qualified.rsplit_once('@'))
                .flatten()
                .filter(|(_, offset)| {
                    !offset.is_empty() && offset.bytes().all(|b| b.is_ascii_digit())
                })
                .map(|(name, _)| name);
            let matches_name = |name: &str| {
                selector == name
                    || name
                        .strip_suffix(selector)
                        .is_some_and(|p| p.ends_with("::"))
            };
            if located.is_some()
                || matches_name(&qualified)
                || local_name.is_some_and(matches_name)
                || selector == symbol.name
            {
                candidates.push(json!({"id":symbol.id,"selector":qualified,"name":symbol.name,"kind":symbol.kind,"location":symbol.location,"identity":symbol.identity}));
            } else {
                let name = local_name.unwrap_or(&qualified);
                let (owner, short) = name.rsplit_once("::").unwrap_or(("", name));
                let same_scope = selector_scope.is_none_or(|scope| {
                    owner == scope || owner.strip_suffix(scope).is_some_and(|p| p.ends_with("::"))
                });
                if !same_scope {
                    continue;
                }
                let score = if short.starts_with(tail) {
                    Some(0)
                } else if short.chars().count() > 1 && tail.starts_with(short) {
                    Some(1)
                } else if short.len() > 2 && tail_chars.len() > 2 {
                    selector_typo_distance(short, &tail_chars).map(|distance| distance + 2)
                } else {
                    None
                };
                if selector_scope.is_some() {
                    scope_fallback.push(qualified.clone());
                    scope_fallback.sort_unstable();
                    scope_fallback.dedup();
                    scope_fallback.truncate(5);
                }
                if let Some(score) = score {
                    suggestions.push((score, qualified));
                    suggestions.sort_unstable();
                    suggestions.dedup();
                    suggestions.truncate(5);
                }
            }
        }
        let suggestions: Vec<_> = if suggestions.is_empty() {
            scope_fallback
        } else {
            suggestions.into_iter().map(|(_, name)| name).collect()
        };
        let mut spellings = HashMap::new();
        for candidate in &candidates {
            *spellings
                .entry(candidate["selector"].as_str().unwrap().to_owned())
                .or_insert(0usize) += 1;
        }
        for candidate in &mut candidates {
            if spellings[candidate["selector"].as_str().unwrap()] > 1
                && let (Some(path), Some(start)) = (
                    candidate["location"]["path"].as_str(),
                    candidate["location"]["start"].as_u64(),
                )
            {
                let path = path.to_owned();
                let (line, column) = self.source(&path)?.position(start.try_into()?)?;
                let display = Path::new(&path)
                    .strip_prefix(self.workspace())
                    .unwrap_or(Path::new(&path))
                    .to_string_lossy()
                    .replace('\\', "/");
                candidate["qualified_name"] = candidate["selector"].take();
                candidate["selector"] = json!(format!("{display}:{line}:{column}"));
            }
        }
        Ok(match candidates.len() {
            0 => {
                let mut result = json!({"status":"unknown","suggestions":suggestions});
                if let Some(reason) = location_reason {
                    result["reason"] = json!(reason);
                } else if located.is_some() {
                    result["reason"] = json!(
                        "A symbol exists at this position, but it does not match the selected filters."
                    );
                }
                result
            }
            1 => json!({"status":"ok","symbol":candidates.remove(0)}),
            _ => json!({"status":"ambiguous","candidates":candidates}),
        })
    }
    pub fn query(
        &mut self,
        command: &str,
        selector: &str,
        filter: &SelectorFilter,
        all: bool,
    ) -> Result<Value> {
        let revision = self.session.revision().to_owned();
        if command == "type"
            && let Some((file, byte)) = self.location(selector)?
        {
            if filter.kind.is_some() || filter.module.is_some() || filter.package.is_some() {
                let resolved = self.resolve(selector, filter)?;
                if resolved["status"] != "ok" {
                    return Ok(resolved);
                }
            }
            let mut result = self.session.query(QueryRequest::TypeAt {
                revision,
                file: file.clone(),
                byte,
            })["result"]
                .take();
            if result["status"] == "unknown" {
                result["reason"] = json!(format!(
                    "No typed expression or declaration at this position. {}",
                    self.missing_symbol_reason(&file, byte)?
                ));
            }
            return Ok(result);
        }
        let resolved = self.resolve(selector, filter)?;
        if resolved["status"] != "ok" {
            return Ok(resolved);
        }
        let function = resolved["symbol"]["id"]
            .as_str()
            .context("resolved symbol lacks identity")?
            .to_owned();
        if command == "impact" {
            // Contract declarations have no executable body. Traverse the shared
            // semantic family once to seed all implementations, including defaults.
            let mut seeds = vec![function.clone()];
            if !self.function_names.contains_key(&function)
                || self.contract_bodies.contains(&function)
            {
                if resolved["symbol"]["kind"] != "method" {
                    return Ok(
                        json!({"status":"unanalyzed","reason":"Impact requires a function or method.","selected":resolved["symbol"]}),
                    );
                }
                let mut links = HashMap::<&str, Vec<&str>>::new();
                for (a, b) in &self.session.snapshot.semantic.rename_links {
                    links.entry(a).or_default().push(b);
                    links.entry(b).or_default().push(a);
                }
                let mut seen = std::collections::HashSet::new();
                let mut pending = vec![function.as_str()];
                seeds.clear();
                while let Some(id) = pending.pop() {
                    if seen.insert(id)
                        && let Some(neighbors) = links.get(id)
                    {
                        pending.extend(neighbors.iter().copied());
                    }
                }
                seeds.extend(
                    self.session
                        .snapshot
                        .functions
                        .iter()
                        .filter(|f| seen.contains(f.id.as_str()))
                        .map(|f| f.id.clone()),
                );
                if seeds.is_empty() {
                    return Ok(
                        json!({"status":"unanalyzed","reason":"This interface contract has no analyzed implementation in the selected source graph.","selected":resolved["symbol"]}),
                    );
                }
            }
            let impact = self.session.snapshot.impact(
                &seeds,
                Direction::Callers,
                Limits::default(),
                Some(&revision),
            )?;
            let mut impact = serde_json::to_value(impact)?;
            if let Some(nodes) = impact["nodes"].as_array_mut() {
                for node in nodes {
                    if let Some(name) = node["id"]
                        .as_str()
                        .and_then(|id| self.function_names.get(id))
                    {
                        node["selector"] = json!(name);
                    }
                }
            }
            return Ok(json!({"status":"ok","impact":impact,"symbol":resolved["symbol"]}));
        }
        let request = match command {
            "refs" | "references" => QueryRequest::References { revision, function },
            "effects" => QueryRequest::Effects { revision, function },
            "symbol" | "type" => QueryRequest::SymbolInfo { revision, function },
            _ => anyhow::bail!("unsupported direct command"),
        };
        let mut result = self.session.query(request)["result"].take();
        if command == "symbol" {
            let location = &resolved["symbol"]["location"];
            if let (Some(file), Some(byte)) =
                (location["path"].as_str(), location["start"].as_u64())
            {
                let typed = self.session.query(QueryRequest::TypeAt {
                    revision: self.session.revision().to_owned(),
                    file: file.to_owned(),
                    byte: byte.try_into()?,
                });
                if let Some(display) = typed["result"].get("type_display") {
                    result["type_display"] = display.clone();
                }
            }
        }
        if command == "type" {
            let location = &resolved["symbol"]["location"];
            if let (Some(file), Some(byte)) =
                (location["path"].as_str(), location["start"].as_u64())
            {
                result = self.session.query(QueryRequest::TypeAt {
                    revision: self.session.revision().to_owned(),
                    file: file.to_owned(),
                    byte: byte.try_into()?,
                })["result"]
                    .take();
            }
        }
        result["selected"] = resolved["symbol"].clone();
        if let Some(references) = result.get_mut("references").and_then(Value::as_array_mut) {
            let total = references.len();
            if !all {
                references.truncate(50);
            }
            let shown = references.len();
            result["total"] = json!(total);
            result["shown"] = json!(shown);
            result["truncated"] = json!(shown < total);
        }
        Ok(result)
    }
    /// Human-only labels; callers keep the query's structured IDs unchanged.
    pub fn display_effect_targets(&self, result: &mut Value) {
        for key in ["effect_evidence", "compiler_witnesses"] {
            if let Some(facts) = result[key].as_array_mut() {
                for fact in facts {
                    if let Some(name) = fact["via_function"]
                        .as_str()
                        .and_then(|id| self.function_names.get(id))
                    {
                        fact["via_function"] = json!(name);
                    }
                    if let Some(name) = fact["witness"]["owner"]
                        .as_str()
                        .and_then(|id| self.function_names.get(id))
                    {
                        fact["witness"]["owner"] = json!(name);
                    }
                }
            }
        }
    }
    pub fn display_locations(&mut self, value: &mut Value, absolute: bool) -> Result<()> {
        match value {
            Value::Array(items) => {
                for item in items {
                    self.display_locations(item, absolute)?;
                }
            }
            Value::Object(fields) => {
                if let (Some(path), Some(start)) = (
                    fields.get("path").and_then(Value::as_str),
                    fields.get("start").and_then(Value::as_u64),
                ) {
                    let path = path.to_owned();
                    let (line, column) = self.source(&path)?.position(start.try_into()?)?;
                    fields.insert("line".into(), json!(line));
                    fields.insert("column".into(), json!(column));
                    if !absolute {
                        let path = Path::new(&path)
                            .strip_prefix(self.workspace())
                            .map(|p| p.to_string_lossy().replace('\\', "/"))
                            .unwrap_or(path);
                        fields.insert("path".into(), json!(path));
                    }
                }
                if !absolute {
                    for key in ["path", "module"] {
                        if let Some(Value::String(path)) = fields.get_mut(key)
                            && let Ok(relative) = Path::new(path).strip_prefix(self.workspace())
                        {
                            let relative = relative.to_string_lossy().replace('\\', "/");
                            *path = if relative.is_empty() {
                                ".".into()
                            } else {
                                relative
                            };
                        }
                    }
                }
                for item in fields.values_mut() {
                    self.display_locations(item, absolute)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn repeated_positions_have_linear_indexes_and_logarithmic_lookup_work() {
        for size in [16usize, 64, 256, 1024] {
            for fragment in ["日", "日\r\n"] {
                let index = SourceIndex::new(fragment.repeat(size));
                let positions = index.characters.len();
                assert!(positions <= index.text.len() + 1);
                assert_eq!(index.lines.len(), index.line_characters.len());
                for &byte in &index.characters {
                    index.position(byte).unwrap();
                }
                let bound = positions * 2 * (positions.ilog2() as usize + 2);
                assert!(index.search_steps.get() <= bound);
                println!(
                    "source-index chars={positions} lines={} lookups={positions} search_steps={} bound={bound}",
                    index.lines.len(),
                    index.search_steps.get()
                );
            }
        }
    }
    #[test]
    fn source_positions_are_strict_and_unicode_aware() {
        let source = SourceIndex::new("a日本😀\r\n\nlast\n".into());
        for (line, column, byte) in [
            (1, 1, 0),
            (1, 2, 1),
            (1, 3, 4),
            (1, 4, 7),
            (1, 5, 11),
            (2, 1, 13),
            (3, 1, 14),
            (3, 5, 18),
            (4, 1, 19),
        ] {
            assert_eq!(source.byte(line, column).unwrap(), byte);
            assert_eq!(source.position(byte).unwrap(), (line, column));
        }
        for (line, column) in [(0, 1), (1, 0), (1, 6), (2, 2), (4, 2), (5, 1)] {
            assert!(source.byte(line, column).is_err());
        }
        for byte in [2, 3, 5, 6, 8, 9, 10, 20] {
            assert!(source.position(byte).is_err());
        }
        assert_eq!(
            split_location("C:\\work\\main.wi:12:3"),
            Some(("C:\\work\\main.wi", "12", "3"))
        );
        assert_eq!(split_location("order::Order::qty"), None);
        assert_eq!(SourceIndex::new(String::new()).byte(1, 1).unwrap(), 0);
    }
}

/// Banded edit distance, bounded to two edits. O(a+b) work and O(1)
/// distance-band storage, plus O(a+b) Unicode scalar storage.
fn selector_typo_distance(a: &str, b: &[char]) -> Option<usize> {
    let a: Vec<_> = a.chars().collect();
    let limit = if a.len().min(b.len()) < 4 { 1 } else { 2 };
    if a.len().abs_diff(b.len()) > limit {
        return None;
    }
    // Only the diagonal band can participate in a distance <= limit.
    let mut previous = [3usize; 5];
    for j in 0..=limit.min(b.len()) {
        previous[j + limit] = j;
    }
    for i in 1..=a.len() {
        let mut current = [3usize; 5];
        for j in i.saturating_sub(limit)..=b.len().min(i + limit) {
            #[cfg(test)]
            typo_tests::CELLS.with(|count| count.set(count.get() + 1));
            let k = j + limit - i;
            if j == 0 {
                current[k] = i;
                continue;
            }
            let diagonal = previous[k] + usize::from(a[i - 1] != b[j - 1]);
            let deletion = if k + 1 < 5 { previous[k + 1] + 1 } else { 3 };
            let insertion = if k > 0 { current[k - 1] + 1 } else { 3 };
            current[k] = diagonal.min(deletion).min(insertion);
        }
        previous = current;
    }
    let distance = previous[b.len() + limit - a.len()];
    (distance <= limit).then_some(distance)
}

#[cfg(test)]
mod typo_tests {
    use super::selector_typo_distance;
    thread_local! { pub(super) static CELLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
    #[test]
    fn band_work_scales_linearly_with_identifier_length() {
        for n in [16, 64, 256, 1024] {
            let a = "a".repeat(n);
            let mut b: Vec<_> = a.chars().collect();
            b[n - 1] = 'b';
            CELLS.with(|count| count.set(0));
            assert_eq!(selector_typo_distance(&a, &b), Some(1));
            assert_eq!(CELLS.with(|count| count.get()), 5 * n - 4);
        }
    }
    #[test]
    fn bounded_distance_matches_full_distance() {
        fn full(a: &str, b: &str) -> usize {
            let b: Vec<_> = b.chars().collect();
            let mut row: Vec<_> = (0..=b.len()).collect();
            for (i, a) in a.chars().enumerate() {
                let mut diagonal = row[0];
                row[0] = i + 1;
                for (j, &b) in b.iter().enumerate() {
                    let old = row[j + 1];
                    row[j + 1] = (diagonal + usize::from(a != b))
                        .min(row[j] + 1)
                        .min(old + 1);
                    diagonal = old;
                }
            }
            row[b.len()]
        }
        let words = [
            "",
            "a",
            "abc",
            "slot",
            "slto",
            "transfer",
            "tranfer",
            "running",
            "runing",
            "関数名",
            "関数",
            "abcdefghijk",
            "xabcdefghijkz",
        ];
        for a in words {
            for b in words {
                let d = full(a, b);
                let limit = if a.chars().count().min(b.chars().count()) < 4 {
                    1
                } else {
                    2
                };
                assert_eq!(
                    selector_typo_distance(a, &b.chars().collect::<Vec<_>>()),
                    (d <= limit).then_some(d),
                    "{a}/{b}"
                );
            }
        }
    }
}
