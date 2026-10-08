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
    /// Sorted lexer token spans, built on the first position-coverage query.
    /// `None` when the text does not lex (no coverage filtering then).
    tokens: std::cell::OnceCell<Option<Vec<(usize, usize)>>>,
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
            tokens: std::cell::OnceCell::new(),
            #[cfg(test)]
            search_steps: std::cell::Cell::new(0),
        }
    }
    /// Whether `byte` lies outside every source token (whitespace, comments,
    /// end of line). Lexing is linear once per file; each lookup is logarithmic.
    pub fn is_between_tokens(&self, byte: usize) -> bool {
        let Some(tokens) = self.tokens.get_or_init(|| {
            crate::lexer::Lexer::new(&self.text)
                .tokenize()
                .ok()
                .map(|tokens| {
                    tokens
                        .iter()
                        .filter(|t| t.span.start < t.span.end)
                        .map(|t| (t.span.start, t.span.end))
                        .collect()
                })
        }) else {
            return false;
        };
        let p = tokens.partition_point(|t| t.0 <= byte);
        p.checked_sub(1).is_none_or(|i| byte >= tokens[i].1)
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
    lambda_symbols: HashMap<String, symbols::Symbol>,
    constructor_bodies: HashMap<String, String>,
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
        // Constructor overloads have distinct declaration IDs, while the
        // compiler analyzes their bodies under one class `init` callable ID.
        let mut callable_by_owner = HashMap::new();
        let mut function_names = HashMap::new();
        let mut lambda_symbols = HashMap::new();
        let mut contract_bodies = std::collections::HashSet::new();
        for f in &snapshot.functions {
            if f.name.starts_with("<lambda ")
                && let Some(location) = f.locations.first()
            {
                lambda_symbols.insert(
                    f.id.clone(),
                    symbols::Symbol {
                        identity: f.identity.clone(),
                        id: f.id.clone(),
                        name: f.name.clone(),
                        source_name: None,
                        details: Default::default(),
                        kind: "function".into(),
                        location: Some(location.clone()),
                        ty: None,
                    },
                );
            }
            if f.name.ends_with("::init") {
                callable_by_owner.insert((f.module.as_str(), f.name.as_str()), f.id.as_str());
            }
            let module = f
                .identity
                .as_ref()
                .map(|i| i.module.clone())
                .unwrap_or_else(|| source_module(&f.module, &snapshot.workspace));
            function_names.insert(f.id.clone(), format!("{module}::{}", f.name));
            if f.name
                .rsplit("::")
                .next()
                .is_some_and(|n| n.starts_with("$default$"))
            {
                contract_bodies.insert(f.id.clone());
            }
        }
        let constructor_bodies = snapshot
            .semantic
            .symbols
            .iter()
            .filter(|s| s.kind == "constructor")
            .filter_map(|s| {
                let owner = s.source_name.as_deref()?.rsplit_once('#')?.0;
                let path = s.location.as_ref()?.path.as_str();
                callable_by_owner
                    .get(&(path, owner))
                    .map(|id| (s.id.clone(), (*id).to_owned()))
            })
            .collect();
        let mut session = Self {
            session: QuerySession::new(snapshot)?,
            sources: HashMap::new(),
            function_names,
            lambda_symbols,
            constructor_bodies,
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
            let mut preview = edit::preview_analyzed(&self.session.snapshot, request)?;
            let summary = rename_summary(&preview);
            preview
                .as_object_mut()
                .context("rename preview is not an object")?
                .extend(summary);
            return Ok(preview);
        }
        let local_hint = matches!(
            resolved["symbol"]["kind"].as_str(),
            Some("binding" | "parameter")
        )
        .then(|| -> Result<_> {
            Ok((
                resolved["symbol"]["selector"]
                    .as_str()
                    .context("local selector")?
                    .to_owned(),
                serde_json::from_value::<super::Location>(resolved["symbol"]["location"].clone())?,
            ))
        })
        .transpose()?;
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
        if let Some((selector, location)) = local_hint {
            workspace.remember_local_rename(transaction, &selector, &location)?;
        }
        let mut emitter = crate::diagnostics::HumanEmitter;
        workspace.validate(transaction, &mut emitter)?;
        workspace.apply_with_rollback(transaction, &mut emitter)?;
        let mut result = json!({"status":"ok","old":selector,"new":name,"validation":"passed","transaction":transaction});
        result
            .as_object_mut()
            .unwrap()
            .extend(rename_summary(&preview));
        Ok(result)
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
    pub(super) fn selector(&self, symbol: &symbols::Symbol) -> String {
        // Lambda names contain ':' as an offset separator, not an identity tag.
        if self.lambda_symbols.contains_key(&symbol.id) {
            return self.function_names[&symbol.id].clone();
        }
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
            Some(_) if source.is_between_tokens(byte) => "a comment".to_owned(),
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
            let mut ids: std::collections::HashSet<String> = symbols
                .as_array()
                .into_iter()
                .flatten()
                .filter(|s| s["kind"] != "import" || result.get("resolved_symbols").is_none())
                .filter_map(|s| s["id"].as_str().map(str::to_owned))
                .collect();
            // Lambda opening tokens have callable locations but no declaration
            // symbols. Match only the opening byte, never shadow inner symbols.
            ids.extend(
                self.lambda_symbols
                    .values()
                    .filter(|s| {
                        s.location
                            .as_ref()
                            .is_some_and(|loc| loc.path == file && loc.start == byte)
                    })
                    .map(|s| s.id.clone()),
            );
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
        let mut sole_local = None;
        let mut scoped_locals = 0;
        let hint = edit::LocalRenameHint::read(&self.session.snapshot).filter(|hint| {
            let plain = hint
                .selector
                .rsplit_once('@')
                .map_or(hint.selector.as_str(), |(name, _)| name);
            [hint.selector.as_str(), plain].iter().any(|name| {
                *name == selector
                    || name
                        .strip_suffix(selector)
                        .is_some_and(|p| p.ends_with("::"))
            })
        });
        let segments: Vec<_> = selector
            .split("::")
            .map(|s| (s, s.chars().collect::<Vec<_>>()))
            .collect();
        let selector_scope = selector.rsplit_once("::").map(|(scope, _)| scope);
        for symbol in self
            .session
            .snapshot
            .semantic
            .symbols
            .iter()
            .chain(self.lambda_symbols.values())
        {
            if located
                .as_ref()
                .is_some_and(|ids| !ids.contains(&symbol.id))
            {
                continue;
            }
            // Builtin member calls are rename evidence, not name-selectable
            // symbols: `len` must not become ambiguous with `Array::len`.
            if located.is_none() && symbol.kind == "builtin-method" {
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
                let (owner, _) = name.rsplit_once("::").unwrap_or(("", name));
                let same_scope = selector_scope.is_none_or(|scope| {
                    owner == scope || owner.strip_suffix(scope).is_some_and(|p| p.ends_with("::"))
                });
                let score = selector_suggestion_score(name, &segments);
                if local_name.is_some() && same_scope && selector_scope.is_some() {
                    scoped_locals += 1;
                    sole_local = (scoped_locals == 1).then(|| qualified.clone());
                }
                if local_name.is_none() && same_scope && selector_scope.is_some() {
                    scope_fallback.push(qualified.clone());
                    scope_fallback.sort_unstable();
                    scope_fallback.dedup();
                    scope_fallback.truncate(5);
                }
                let previous = hint.as_ref().is_some_and(|hint| {
                    symbol
                        .location
                        .as_ref()
                        .is_some_and(|loc| loc.path == hint.path && loc.start == hint.start)
                }) && local_name.is_some();
                if previous || score.is_some() {
                    // A matching declaration beats spelling similarity. Position
                    // breaks equal-score ties deterministically within a file.
                    let position = symbol.location.as_ref().map_or(usize::MAX, |loc| loc.start);
                    suggestions.push((!previous, score.unwrap_or(usize::MAX), position, qualified));
                    suggestions.sort_unstable();
                    suggestions.dedup();
                    suggestions.truncate(5);
                }
            }
        }
        // Preserve a useful unambiguous same-scope fallback, without choosing
        // arbitrary locals when several declarations could have been renamed.
        if let Some(local) = sole_local {
            scope_fallback.insert(0, local);
            scope_fallback.truncate(5);
        }
        let mut suggestions: Vec<_> = if suggestions.is_empty() {
            scope_fallback
        } else {
            suggestions
                .into_iter()
                .map(|(_, _, _, name)| name)
                .collect()
        };
        normalize_local_suggestions(
            &mut suggestions,
            self.session
                .snapshot
                .semantic
                .symbols
                .iter()
                .filter(|symbol| matches!(symbol.kind.as_str(), "binding" | "parameter"))
                .map(|symbol| self.selector(symbol)),
        );
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
                let mut result = json!({"status":"unknown","reason":"No symbol matches this selector and its filters.","suggestions":suggestions});
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
            let mut result = if self.source(&file)?.is_between_tokens(byte) {
                json!({"status":"unknown"})
            } else {
                self.session.query(QueryRequest::TypeAt {
                    revision,
                    file: file.clone(),
                    byte,
                })["result"]
                    .take()
            };
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
        let declaration = resolved["symbol"]["id"]
            .as_str()
            .context("resolved symbol lacks identity")?;
        let function = if matches!(command, "effects" | "impact") {
            self.constructor_bodies
                .get(declaration)
                .map(String::as_str)
                .unwrap_or(declaration)
        } else {
            declaration
        }
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
        if command == "symbol" && result["symbol"].get("type_display").is_none() {
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
    /// Human-only labels. Persisted names (and their IDs, fingerprints and legacy
    /// selectors) retain byte offsets; displayed lambda anchors are selectors too.
    pub fn display_function_targets(&mut self, result: &mut Value, absolute: bool) -> Result<()> {
        // Only index locations needed by this response, once per distinct lambda.
        let mut ids = std::collections::HashSet::new();
        for key in ["selected", "symbol"] {
            if let Some(id) = result[key]["id"].as_str() {
                ids.insert(id);
            }
        }
        if let Some(nodes) = result["impact"]["nodes"].as_array() {
            ids.extend(nodes.iter().filter_map(|node| node["id"].as_str()));
        }
        for key in ["effect_evidence", "compiler_witnesses"] {
            if let Some(facts) = result[key].as_array() {
                for fact in facts {
                    ids.extend(fact["via_function"].as_str());
                    ids.extend(fact["witness"]["owner"].as_str());
                }
            }
        }
        let lambdas: Vec<_> = ids
            .into_iter()
            .filter_map(|id| {
                self.lambda_symbols
                    .get(id)
                    .and_then(|s| s.location.clone())
                    .map(|loc| (id.to_owned(), loc))
            })
            .collect();
        let mut labels = HashMap::with_capacity(lambdas.len());
        for (id, loc) in lambdas {
            let (line, column) = self.source(&loc.path)?.position(loc.start)?;
            let path = if absolute {
                loc.path.clone()
            } else {
                Path::new(&loc.path)
                    .strip_prefix(self.workspace())
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or(loc.path)
            };
            labels.insert(id, format!("{path}:{line}:{column}"));
        }
        for key in ["selected", "symbol"] {
            if let Some(label) = result[key]["id"].as_str().and_then(|id| labels.get(id)) {
                result[key]["selector"] = json!(label);
            }
        }
        if let Some(nodes) = result["impact"]["nodes"].as_array_mut() {
            for node in nodes {
                if let Some(label) = node["id"].as_str().and_then(|id| labels.get(id)) {
                    node["selector"] = json!(label);
                }
            }
        }
        let name = |id: &str| labels.get(id).or_else(|| self.function_names.get(id));
        for key in ["effect_evidence", "compiler_witnesses"] {
            if let Some(facts) = result[key].as_array_mut() {
                for fact in facts {
                    if let Some(name) = fact["via_function"].as_str().and_then(name) {
                        fact["via_function"] = json!(name);
                    }
                    if let Some(name) = fact["witness"]["owner"].as_str().and_then(name) {
                        fact["witness"]["owner"] = json!(name);
                    }
                }
            }
        }
        Ok(())
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
    fn lambda_projection_deduplicates_positions_and_preserves_snapshot() {
        for n in [16usize, 64, 256, 1024] {
            let path = "/virtual/main.wi";
            let text = "日|| 1;\r\n".repeat(n);
            let mut snapshot = Snapshot {
                edit_context: None,
                version: 1,
                compiler: storage::compiler_stamp(),
                compatibility: String::new(),
                workspace: "/virtual".into(),
                revision: String::new(),
                sources: BTreeMap::from([(path.into(), hash(&text))]),
                semantic: Default::default(),
                functions: (0..n)
                    .map(|i| {
                        let start = i * "日|| 1;\r\n".len() + "日".len();
                        Function {
                            identity: None,
                            body_id: None,
                            body_location: None,
                            rename_calls: vec![],
                            rename_imports: vec![],
                            id: format!("f{i}"),
                            module: path.into(),
                            name: format!("<lambda main@{start}:{}>", start + 4),
                            locations: vec![Location {
                                path: path.into(),
                                start,
                                end: start + 4,
                            }],
                            synthetic: false,
                            fingerprint: String::new(),
                            body_fingerprint: String::new(),
                            callees: vec![],
                            runtime_effects: 0,
                            unknown: false,
                            unresolved: vec![],
                        }
                    })
                    .collect(),
            };
            snapshot.revision = snapshot.digest().unwrap();
            let before = snapshot.revision.clone();
            let mut session = DirectSession::new(snapshot).unwrap();
            session.sources.insert(path.into(), SourceIndex::new(text));
            let mut once = 0;
            for repeats in [1, 8] {
                session.sources[path].search_steps.set(0);
                let facts: Vec<_> = (0..n * repeats)
                    .map(|i| {
                        json!({
                            "via_function": format!("f{}", i % n),
                            "witness": {"owner": format!("f{}", i % n)}
                        })
                    })
                    .collect();
                let mut result = json!({"effect_evidence":facts});
                session
                    .display_function_targets(&mut result, false)
                    .unwrap();
                for (i, fact) in result["effect_evidence"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                {
                    let anchor = format!("main.wi:{}:2", i % n + 1);
                    assert_eq!(fact["via_function"], anchor);
                    assert_eq!(fact["witness"]["owner"], anchor);
                }
                let steps = session.sources[path].search_steps.get();
                if repeats == 1 {
                    once = steps;
                } else {
                    assert_eq!(steps, once);
                }
                let bound = n * 2 * ((n * 10).ilog2() as usize + 2);
                assert!(steps <= bound);
                println!(
                    "lambda-projection lambdas={n} rows={} position_search_steps={steps} bound={bound}",
                    n * repeats
                );
            }
            assert_eq!(session.snapshot().digest().unwrap(), before);
            assert_eq!(session.sources.len(), 1);
        }
    }
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

/// Only the bounded final candidates need ambiguity counts. Keep offsets for
/// shadowed names; a second streaming pass needs no workspace-sized index.
fn normalize_local_suggestions(suggestions: &mut [String], locals: impl Iterator<Item = String>) {
    let mut counts = [0usize; 5];
    if !suggestions.iter().any(|name| name.contains('@')) {
        return;
    }
    for name in locals {
        for (i, suggestion) in suggestions.iter().enumerate() {
            if let (Some((plain, _)), Some((wanted, _))) =
                (name.rsplit_once('@'), suggestion.rsplit_once('@'))
                && plain == wanted
            {
                counts[i] += 1;
            }
        }
    }
    for (i, suggestion) in suggestions.iter_mut().enumerate() {
        if counts[i] == 1 {
            suggestion.truncate(suggestion.rfind('@').unwrap());
        }
    }
}

/// Compare aligned path segments from the leaf, permitting the same omitted
/// module prefix as exact resolution. Each segment is visited at most once.
fn selector_suggestion_score(name: &str, segments: &[(&str, Vec<char>)]) -> Option<usize> {
    let mut names = name.rsplit("::");
    let mut score = 0;
    for (wanted, chars) in segments.iter().rev() {
        #[cfg(test)]
        typo_tests::SEGMENTS.with(|count| count.set(count.get() + 1));
        let actual = names.next()?;
        if actual == *wanted {
            continue;
        }
        score += if !wanted.is_empty() && actual.starts_with(wanted) {
            1
        } else if actual.chars().count() > 1 && wanted.starts_with(actual) {
            2
        } else {
            selector_typo_distance(actual, chars)? + 2
        };
    }
    Some(score)
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

/// File and edit counts shared by rename dry-run previews and applied renames.
fn rename_summary(preview: &Value) -> serde_json::Map<String, Value> {
    let changes = preview["changes"].as_array().map_or(0, Vec::len);
    let edits = preview["work"]["patches"].as_u64().unwrap_or(0);
    let declarations = preview["work"]["declarations"].as_u64().unwrap_or(0);
    let references = edits.saturating_sub(declarations);
    serde_json::Map::from_iter([
        ("files_changed".to_owned(), json!(changes)),
        ("edits".to_owned(), json!(edits)),
        ("declarations_updated".to_owned(), json!(declarations)),
        ("references_updated".to_owned(), json!(references)),
    ])
}

#[cfg(test)]
mod typo_tests {
    use super::{selector_suggestion_score, selector_typo_distance};

    #[test]
    fn local_suggestion_normalization_streams_once_with_bounded_candidates() {
        for n in [16, 32, 64, 128] {
            let visits = std::cell::Cell::new(0);
            let mut suggestions = vec![
                "m::f::unique@0".into(),
                "m::f::shadow@1".into(),
                "m::f::other@2".into(),
            ];
            super::normalize_local_suggestions(
                &mut suggestions,
                (0..n).map(|i| {
                    visits.set(visits.get() + 1);
                    match i {
                        0 => "m::f::unique@0".into(),
                        1 => "m::f::other@2".into(),
                        _ => format!("m::f::shadow@{i}"),
                    }
                }),
            );
            assert_eq!(visits.get(), n);
            assert_eq!(
                suggestions,
                ["m::f::unique", "m::f::shadow@1", "m::f::other"]
            );
            println!(
                "locals={n} visits={} comparison_bound={}",
                visits.get(),
                3 * n
            );
        }
    }

    thread_local! { pub(super) static SEGMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
    thread_local! { pub(super) static CELLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
    fn score(name: &str, selector: &str) -> Option<usize> {
        selector_suggestion_score(
            name,
            &selector
                .split("::")
                .map(|s| (s, s.chars().collect()))
                .collect::<Vec<_>>(),
        )
    }
    #[test]
    fn segment_scores_cover_edits_suffixes_and_ranking() {
        for (name, selector, expected) in [
            ("eval::Recursive::cell", "eval::Recursive::cell", Some(0)),
            ("eval::Recursive::cell", "Recursive::cell", Some(0)),
            ("eval::Recursive::cell", "cell", Some(0)),
            ("eval::Recursive::cell", "evl::Recursive::cell", Some(3)),
            ("eval::Recursive::cell", "eval::Recursiv::cell", Some(1)),
            ("eval::Recursive::cell", "eval::Recursive::cel", Some(1)),
            ("eval::Recursive::cell", "eval::Recursive::celll", Some(2)),
            ("eval::Recursive::cell", "eval::Recursive::clle", Some(4)),
            ("eval::Recursive::cell", "evl::Recursiv::cell", Some(4)),
            ("eval::Recursive::cell", "different::Recursive::cell", None),
            ("Recursive::cell", "eval::Recursive::cel", None),
            ("eval::Recursive::cell", "eval::cell", None),
            ("eval::Recursive::cell", "eval::Recursive::unrelated", None),
            ("a::B::c", "a::C::c", Some(3)),
            ("日本語::計算式", "日本::計算式", Some(1)),
            ("日本語::計算式", "日本語::計算値", Some(3)),
        ] {
            assert_eq!(score(name, selector), expected, "{name}/{selector}");
        }
        assert!(
            score("eval::Recursive::cell", "eval::Recursiv::cell")
                < score("eval::Recursive::cells", "eval::Recursiv::cell")
        );
    }
    #[test]
    fn segment_work_scales_with_candidates_and_depth_without_suffix_rescans() {
        for depth in [1, 4, 16, 64] {
            let selector = vec!["Recursive"; depth].join("::");
            let name = format!("{}x", selector);
            let parts: Vec<_> = selector
                .split("::")
                .map(|s| (s, s.chars().collect()))
                .collect();
            for candidates in [1, 8, 64] {
                SEGMENTS.with(|count| count.set(0));
                for _ in 0..candidates {
                    assert_eq!(selector_suggestion_score(&name, &parts), Some(1));
                }
                let visits = SEGMENTS.with(|count| count.get());
                assert_eq!(visits, candidates * depth);
                println!("candidates={candidates} depth={depth} segment_visits={visits}");
            }
        }
    }
    #[test]
    fn band_work_scales_linearly_with_identifier_length() {
        for n in [16, 64, 256, 1024] {
            let a = "a".repeat(n);
            let mut b: Vec<_> = a.chars().collect();
            b[n - 1] = 'b';
            CELLS.with(|count| count.set(0));
            assert_eq!(selector_typo_distance(&a, &b), Some(1));
            let cells = CELLS.with(|count| count.get());
            assert_eq!(cells, 5 * n - 4);
            println!("identifier_scalars={n} distance_cells={cells}");
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
