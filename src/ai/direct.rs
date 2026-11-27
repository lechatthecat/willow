//! Source-facing selectors over one immutable compiler query session.
use super::*;
use crate::ai::SymbolKind;
use crate::qualified_name::QualifiedName;
use serde_json::{Value, json};
use std::path::PathBuf;

/// Source-facing command names. Keep both reference spellings on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DirectCommand {
    Refs,
    References,
    Symbol,
    Type,
    Effects,
    Impact,
    Rename,
}
impl DirectCommand {
    pub fn parse(command: &str) -> Option<Self> {
        Some(match command {
            "refs" => Self::Refs,
            "references" => Self::References,
            "symbol" => Self::Symbol,
            "type" => Self::Type,
            "effects" => Self::Effects,
            "impact" => Self::Impact,
            "rename" => Self::Rename,
            _ => return None,
        })
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Refs => "refs",
            Self::References => "references",
            Self::Symbol => "symbol",
            Self::Type => "type",
            Self::Effects => "effects",
            Self::Impact => "impact",
            Self::Rename => "rename",
        }
    }
}
impl std::fmt::Display for DirectCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A source location before or after direct-command presentation conversion.
#[derive(Debug, Clone, Serialize)]
pub struct SelectedLocation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    pub end: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub path: String,
    pub start: usize,
}
impl From<Location> for SelectedLocation {
    fn from(location: Location) -> Self {
        Self {
            column: None,
            end: location.end,
            line: None,
            path: location.path,
            start: location.start,
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct SelectedSymbol {
    pub id: String,
    pub identity: Option<SymbolIdentity>,
    pub kind: SymbolKind,
    pub location: Option<SelectedLocation>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    pub selector: String,
}
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Resolution {
    Ok {
        symbol: Box<SelectedSymbol>,
    },
    Unknown {
        reason: String,
        suggestions: Vec<String>,
    },
    Ambiguous {
        candidates: Vec<SelectedSymbol>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct DirectQueryResult {
    pub status: super::query_result::Status,
    #[serde(flatten)]
    pub data: DirectQueryData,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<SelectedSymbol>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_display: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shown: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<&'static str>,
}
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum DirectQueryData {
    Semantic(super::query_result::QueryData),
    Unknown {
        suggestions: Vec<String>,
    },
    Ambiguous {
        candidates: Vec<SelectedSymbol>,
    },
    Resolved {
        symbol: SelectedSymbol,
    },
    Impact {
        impact: Impact,
        symbol: SelectedSymbol,
    },
    Empty {},
}
impl DirectQueryResult {
    pub fn new(status: super::query_result::Status, data: DirectQueryData) -> Self {
        Self {
            status,
            data,
            selected: None,
            reason: None,
            type_display: None,
            total: None,
            shown: None,
            truncated: None,
            schema: None,
        }
    }
    pub fn effects(&self) -> Option<&super::query_result::EffectResult> {
        match &self.data {
            DirectQueryData::Semantic(super::query_result::QueryData::Effects(effects)) => {
                Some(effects)
            }
            _ => None,
        }
    }
    pub fn effects_mut(&mut self) -> Option<&mut super::query_result::EffectResult> {
        match &mut self.data {
            DirectQueryData::Semantic(super::query_result::QueryData::Effects(effects)) => {
                Some(effects)
            }
            _ => None,
        }
    }
}
impl From<super::query_result::QueryResult> for DirectQueryResult {
    fn from(result: super::query_result::QueryResult) -> Self {
        Self::new(result.status, DirectQueryData::Semantic(result.data))
    }
}
impl From<Resolution> for DirectQueryResult {
    fn from(resolution: Resolution) -> Self {
        use super::query_result::Status;
        match resolution {
            Resolution::Ok { symbol } => {
                Self::new(Status::Ok, DirectQueryData::Resolved { symbol: *symbol })
            }
            Resolution::Unknown {
                reason,
                suggestions,
            } => {
                let mut result =
                    Self::new(Status::Unknown, DirectQueryData::Unknown { suggestions });
                result.reason = Some(reason);
                result
            }
            Resolution::Ambiguous { candidates } => {
                Self::new(Status::Ambiguous, DirectQueryData::Ambiguous { candidates })
            }
        }
    }
}

#[derive(Default)]
pub struct SelectorFilter {
    pub kind: Option<SymbolKind>,
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
    QualifiedName::from_segments(parts)
}
/// Presentation mode for direct rename. Boolean callers retain legacy previews.
pub enum RenameMode {
    Apply,
    Summary,
    Diff,
}
impl From<bool> for RenameMode {
    fn from(dry_run: bool) -> Self {
        if dry_run { Self::Diff } else { Self::Apply }
    }
}

impl DirectSession {
    pub fn new(snapshot: Snapshot) -> Result<Self> {
        // Constructor overloads have distinct declaration IDs, while the
        // compiler analyzes their bodies under one class `init` callable ID.
        let mut callable_by_owner = HashMap::new();
        let mut function_names = HashMap::new();
        let mut lambda_symbols = HashMap::new();
        for f in &snapshot.functions {
            if f.name.starts_with("<lambda ")
                && let Some(location) = f.locations.first()
            {
                lambda_symbols.insert(
                    f.id.clone(),
                    symbols::Symbol {
                        origin: Default::default(),
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
            if QualifiedName::new(&f.name).has_item("init") {
                callable_by_owner.insert((f.module.as_str(), f.name.as_str()), f.id.as_str());
            }
            let module = f
                .identity
                .as_ref()
                .map(|i| i.module.clone())
                .unwrap_or_else(|| source_module(&f.module, &snapshot.workspace));
            function_names.insert(f.id.clone(), QualifiedName::join(&module, &f.name));
        }
        let constructor_bodies = snapshot
            .semantic
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Constructor)
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
        };
        let names: Vec<_> = session
            .snapshot()
            .semantic
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method && session.function_names.contains_key(&s.id))
            .map(|s| (s.id.clone(), session.selector(s)))
            .collect();
        session.function_names.extend(names);
        // Copies keep their source declaration's range, including imported
        // defaults. Display an addressable source selector for each copy.
        let methods: HashMap<_, _> = session
            .snapshot()
            .semantic
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .map(|s| (s.id.as_str(), session.selector(s)))
            .collect();
        let declarations: HashMap<_, _> = session
            .snapshot()
            .functions
            .iter()
            .filter_map(|f| methods.get(f.id.as_str()).map(|name| (f, name)))
            .flat_map(|(f, name)| {
                f.locations
                    .iter()
                    .map(move |l| ((l.path.as_str(), l.start, l.end), name))
            })
            .collect();
        let copies: Vec<_> = session
            .snapshot()
            .functions
            .iter()
            .filter_map(|f| {
                f.locations
                    .iter()
                    .find_map(|l| declarations.get(&(l.path.as_str(), l.start, l.end)))
                    .map(|name| (f.id.clone(), (*name).clone()))
            })
            .collect();
        session.function_names.extend(copies);
        Ok(session)
    }
    pub fn workspace(&self) -> &str {
        &self.session.snapshot.workspace
    }
    pub fn snapshot(&self) -> &Snapshot {
        &self.session.snapshot
    }
    pub fn rename(
        self,
        entry: &Path,
        project: bool,
        selector: &str,
        filter: &SelectorFilter,
        name: &str,
        mode: impl Into<RenameMode>,
    ) -> Result<Value> {
        self.rename_with_emitter(
            entry,
            project,
            selector,
            filter,
            name,
            mode,
            &mut crate::diagnostics::HumanEmitter,
        )
    }
    /// Preserve the caller's diagnostic policy through validation and publication.
    #[allow(
        clippy::too_many_arguments,
        reason = "Preserve the exported rename_with_emitter API"
    )]
    pub fn rename_with_emitter(
        self,
        entry: &Path,
        project: bool,
        selector: &str,
        filter: &SelectorFilter,
        name: &str,
        mode: impl Into<RenameMode>,
        emitter: &mut dyn crate::diagnostics::DiagnosticEmitter,
    ) -> Result<Value> {
        Ok(serde_json::to_value(self.rename_typed_with_emitter(
            entry, project, selector, filter, name, mode, emitter,
        )?)?)
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "Preserve the exported rename_with_emitter API"
    )]
    pub fn rename_typed_with_emitter(
        mut self,
        entry: &Path,
        project: bool,
        selector: &str,
        filter: &SelectorFilter,
        name: &str,
        mode: impl Into<RenameMode>,
        emitter: &mut dyn crate::diagnostics::DiagnosticEmitter,
    ) -> Result<RenameResult> {
        let mode = mode.into();
        self.session
            .snapshot
            .edit_context
            .as_ref()
            .context("edit requires live analysis inputs")?
            .verify_manifest_entry()?;
        let mut resolved = self.resolve_typed(selector, filter)?;
        if let Resolution::Ambiguous { candidates } = &resolved {
            let mut editable = candidates.iter().filter(|candidate| {
                candidate
                    .location
                    .as_ref()
                    .is_none_or(|location| bundled_source(&location.path).is_none())
            });
            if let (Some(only), None) = (editable.next(), editable.next()) {
                resolved = Resolution::Ok {
                    symbol: Box::new(only.clone()),
                };
            }
        }
        let Resolution::Ok { symbol } = resolved else {
            let mut result = DirectQueryResult::from(resolved);
            self.display_query_locations(&mut result, false)?;
            return Ok(RenameResult::Resolution(Box::new(result)));
        };
        let symbol = *symbol;
        let id = symbol.id.clone();
        let mut selected = symbol.clone();
        self.display_selected(&mut selected, false)?;
        if !self.function_names.contains_key(&id)
            && !matches!(
                symbol.kind,
                SymbolKind::Field
                    | SymbolKind::StaticField
                    | SymbolKind::Variant
                    | SymbolKind::Method
                    | SymbolKind::Binding
                    | SymbolKind::Parameter
            )
        {
            let message = format!(
                "unsupported rename target `{selector}` ({}): rename supports source functions, methods, fields, enum variants and local bindings; this symbol kind is not supported. No files changed",
                symbol.kind
            );
            if let Some(location) = &selected.location
                && let (Some(line), Some(column)) = (location.line, location.column)
            {
                return Err(edit::Rejection {
                    message,
                    location: edit::RejectionLocation {
                        path: location.path.clone(),
                        start: location.start,
                        end: location.end,
                        line,
                        column,
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
        if !matches!(mode, RenameMode::Apply) {
            let preview = edit::preview_analyzed(
                &self.session.snapshot,
                request,
                matches!(mode, RenameMode::Diff),
            )?;
            let edits = preview.work.patches;
            let declarations_updated = preview.work.declarations;
            let references_updated = edits.saturating_sub(declarations_updated);
            return Ok(RenameResult::Preview(RenamePreviewResult {
                preview,
                edits,
                declarations_updated,
                references_updated,
                selected,
            }));
        }
        let rename_selector = self
            .session
            .snapshot
            .semantic
            .symbols
            .iter()
            .chain(self.lambda_symbols.values())
            .find(|candidate| symbol.id == candidate.id)
            .map(|symbol| self.selector(symbol))
            .unwrap_or_else(|| symbol.selector.clone());
        let rename_location = symbol.location.context("rename selector")?;
        let rename_location = Location {
            path: rename_location.path,
            start: rename_location.start,
            end: rename_location.end,
            display: None,
        };
        let workspace = edit::Workspace::open(Path::new(self.workspace()))?;
        let preview = workspace.prepare_analyzed_typed(
            entry,
            project,
            request,
            edit::ChangeFormat::Diff,
            self.session.snapshot,
        )?;
        let transaction = &preview.transaction;
        workspace.validate(transaction, emitter)?;
        workspace.apply_with_rollback(transaction, emitter)?;
        let warnings = workspace.remember_rename(transaction, &rename_selector, &rename_location).err().map(|error| vec![format!(
            "Rename succeeded, but rename history could not be saved; stale-selector suggestions may be unavailable: {error:#}"
        )]);
        Ok(RenameResult::Applied(RenameApplied {
            status: query_result::Status::Ok,
            old: selector.to_owned(),
            new: name.to_owned(),
            validation: "passed",
            transaction: Some(preview.transaction),
            selected,
            warnings,
            files_changed: preview.changes.len(),
            edits: preview.work.patches,
            declarations_updated: preview.work.declarations,
            references_updated: preview
                .work
                .patches
                .saturating_sub(preview.work.declarations),
        }))
    }
    fn source(&mut self, path: &str) -> Result<&SourceIndex> {
        if !self.sources.contains_key(path) {
            let expected = self
                .session
                .snapshot
                .sources
                .get(path)
                .context("invalid-location: file is outside the analyzed workspace")?;
            let text = bundled_source(path)
                .map(str::to_owned)
                .map_or_else(|| std::fs::read_to_string(path), Ok)?;
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
        let path = if path.is_absolute() || bundled_source(file).is_some() {
            path
        } else {
            Path::new(self.workspace()).join(path)
        };
        let path = source_path(&path).context("invalid-location: source file not found")?;
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
        // A module names its namespace, not a member within that namespace.
        // Prefixing it again makes `main::main` collide with the entry function.
        if symbol.kind == SymbolKind::Module {
            return symbol.name.clone();
        }
        if symbol.kind == SymbolKind::Method
            && let (Some(name), Some(location)) = (&symbol.source_name, &symbol.location)
        {
            let module = symbol
                .identity
                .as_ref()
                .map(|i| i.module.clone())
                .unwrap_or_else(|| source_module(&location.path, self.workspace()));
            return QualifiedName::join(&module, name);
        }
        if let Some(identity) = &symbol.identity {
            let name = QualifiedName::from_segments(
                QualifiedName::new(&identity.symbol)
                    .segments()
                    .map(|s| s.split_once(':').map_or(s, |(_, name)| name)),
            );
            QualifiedName::join(&identity.module, &name)
        } else if let Some(name) = self.function_names.get(&symbol.id) {
            name.clone()
        } else if let Some(location) = &symbol.location {
            let module = source_module(&location.path, self.workspace());
            QualifiedName::join(
                &module,
                symbol.source_name.as_deref().unwrap_or(&symbol.name),
            )
        } else {
            symbol.name.clone()
        }
    }
    /// Workspace-relative `path:line:column`, itself a valid position selector.
    fn source_label(&mut self, path: &str, byte: usize) -> Result<String> {
        let (line, column) = self.source(path)?.position(byte)?;
        let display = Path::new(path)
            .strip_prefix(self.workspace())
            .unwrap_or(Path::new(path))
            .to_string_lossy()
            .replace('\\', "/");
        Ok(format!("{display}:{line}:{column}"))
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
        Ok(serde_json::to_value(self.resolve_typed(selector, filter)?)?)
    }
    #[allow(
        clippy::too_many_lines,
        reason = "selector resolution keeps location precedence, exact candidates and ranked fallback suggestions in one decision flow so ambiguous locals and renamed symbols use consistent tie-breaking"
    )]
    pub fn resolve_typed(&mut self, selector: &str, filter: &SelectorFilter) -> Result<Resolution> {
        let mut location_reason = None;
        let located = if let Some((file, byte)) = self.location(selector)? {
            use super::query_result::{QueryData, SymbolInfo};
            let result = self.session.symbol_at_query(&file, byte);
            let mut ids: std::collections::HashSet<String> = match result.data {
                QueryData::Symbol {
                    resolved_symbols: Some(symbols),
                    ..
                } => symbols
                    .into_iter()
                    .filter(|symbol| symbol.kind != SymbolKind::Import)
                    .map(|symbol| symbol.id)
                    .collect(),
                QueryData::Symbol {
                    symbol,
                    resolved_symbols: None,
                } => {
                    let id = match symbol {
                        SymbolInfo::Function(function) => function.id,
                        SymbolInfo::Declaration(symbol) => symbol.id,
                    };
                    std::collections::HashSet::from([id])
                }
                QueryData::Symbols { symbols, .. } => {
                    symbols.into_iter().map(|symbol| symbol.id).collect()
                }
                _ => std::collections::HashSet::new(),
            };
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
        let mut hints: HashMap<String, std::collections::HashSet<usize>> = HashMap::new();
        for hint in edit::RenameHint::read(&self.session.snapshot)
            .into_iter()
            .filter(|hint| {
                let plain = hint
                    .selector
                    .rsplit_once('@')
                    .map_or(hint.selector.as_str(), |(name, _)| name);
                [hint.selector.as_str(), plain].iter().any(|name| {
                    *name == selector || QualifiedName::new(*name).strip_tail(selector).is_some()
                })
            })
        {
            hints.entry(hint.path).or_default().insert(hint.start);
        }
        // Selectors separate members only with `::`. A `Type.member` spelling
        // never resolves, but its suggestions rank the `Type::member` path.
        let suggestion_selector = if located.is_none() && selector.contains('.') {
            std::borrow::Cow::Owned(QualifiedName::from_segments(selector.split('.')))
        } else {
            std::borrow::Cow::Borrowed(selector)
        };
        let segments: Vec<_> = QualifiedName::new(&*suggestion_selector)
            .segments()
            .map(|s| (s, s.chars().collect::<Vec<_>>()))
            .collect();
        let selector_scope = QualifiedName::new(&*suggestion_selector).qualifier();
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
            if located.is_none() && symbol.kind == SymbolKind::BuiltinMethod {
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
            // Preserve reusable overload selectors, but accept and suggest the
            // source constructor spelling rather than its internal ordinal.
            let source_selector = if symbol.kind == SymbolKind::Constructor {
                qualified
                    .rsplit_once('#')
                    .map_or(qualified.as_str(), |(name, _)| name)
            } else {
                &qualified
            };
            let local_name = local_plain(&symbol.kind, &qualified);
            let matches_name = |name: &str| {
                selector == name || QualifiedName::new(name).strip_tail(selector).is_some()
            };
            if located.is_some()
                || matches_name(&qualified)
                || (symbol.kind == SymbolKind::Constructor && matches_name(source_selector))
                || local_name.is_some_and(matches_name)
                || selector == symbol.name
            {
                candidates.push(SelectedSymbol {
                    id: symbol.id.clone(),
                    selector: qualified,
                    name: symbol.name.clone(),
                    kind: symbol.kind.clone(),
                    location: symbol.location.clone().map(SelectedLocation::from),
                    identity: symbol.identity.clone(),
                    qualified_name: None,
                });
            } else if symbol.kind != SymbolKind::Import {
                // Imports alias declarations owned elsewhere; suggest those
                // declarations rather than the importing module's alias.
                let qualified = source_selector;
                let name = local_name.unwrap_or(qualified);
                // Lambda pseudo-names expose byte offsets; rank them after
                // declarations and label them by source position below.
                let lambda = self.lambda_symbols.contains_key(&symbol.id);
                // Lambdas and shadowed locals display a source position, never
                // their internal byte-offset pseudo-names.
                let anchor = (lambda || local_name.is_some())
                    .then(|| {
                        symbol
                            .location
                            .as_ref()
                            .map(|loc| (loc.path.clone(), loc.start))
                    })
                    .flatten();
                let (owner, _) = QualifiedName::new(&name).split_last().unwrap_or(("", name));
                let same_scope = selector_scope.is_none_or(|scope| {
                    owner == scope || QualifiedName::new(owner).strip_tail(scope).is_some()
                });
                let score = selector_suggestion_score(name, &segments).or_else(|| {
                    // An exact declaration path under a different module is a
                    // useful fallback, even when the module is not a typo.
                    // Keep locals scoped and rank spelling matches ahead of it.
                    if local_name.is_some() || lambda {
                        return None;
                    }
                    let module = symbol
                        .identity
                        .as_ref()
                        .map(|i| std::borrow::Cow::Borrowed(i.module.as_str()))
                        .or_else(|| {
                            symbol.location.as_ref().map(|loc| {
                                std::borrow::Cow::Owned(source_module(&loc.path, self.workspace()))
                            })
                        })?;
                    wrong_module_suggestion(name, &module, &suggestion_selector)
                        .then_some(usize::MAX - 1)
                });
                if local_name.is_some() && same_scope && selector_scope.is_some() {
                    scoped_locals += 1;
                    sole_local = (scoped_locals == 1).then(|| qualified.to_owned());
                }
                if local_name.is_none() && same_scope && selector_scope.is_some() {
                    // Declarations sort by name; lambdas follow in source order.
                    let order = anchor.clone().filter(|_| lambda);
                    scope_fallback.push((order, qualified.to_owned(), anchor.clone()));
                    scope_fallback.sort_unstable();
                    scope_fallback.dedup();
                    scope_fallback.truncate(5);
                }
                let previous = symbol.location.as_ref().is_some_and(|loc| {
                    hints
                        .get(&loc.path)
                        .is_some_and(|starts| starts.contains(&loc.start))
                });
                if previous || score.is_some() {
                    // A matching declaration beats spelling similarity. Position
                    // breaks equal-score ties deterministically within a file.
                    let position = symbol.location.as_ref().map_or(usize::MAX, |loc| loc.start);
                    suggestions.push((
                        !previous,
                        lambda,
                        score.unwrap_or(usize::MAX),
                        position,
                        qualified.to_owned(),
                        anchor,
                    ));
                    suggestions.sort_unstable();
                    suggestions.dedup();
                    suggestions.truncate(5);
                }
            }
        }
        // Preserve a useful unambiguous same-scope fallback, without choosing
        // arbitrary locals when several declarations could have been renamed.
        if let Some(local) = sole_local {
            scope_fallback.insert(0, (None, local, None));
            scope_fallback.truncate(5);
        }
        let (mut suggestions, anchors): (Vec<_>, Vec<_>) = if suggestions.is_empty() {
            scope_fallback
                .into_iter()
                .map(|(_, name, anchor)| (name, anchor))
                .unzip()
        } else {
            suggestions
                .into_iter()
                .map(|(_, _, _, _, name, anchor)| (name, anchor))
                .unzip()
        };
        normalize_local_suggestions(
            &mut suggestions,
            self.session
                .snapshot
                .semantic
                .symbols
                .iter()
                .filter(|symbol| matches!(symbol.kind, SymbolKind::Binding | SymbolKind::Parameter))
                .map(|symbol| self.selector(symbol)),
        );
        // At most five labels are resolved, against cached sources. Lambda
        // names and shadowed locals still carry a byte-offset anchor here.
        for (suggestion, anchor) in suggestions.iter_mut().zip(anchors) {
            if let Some((path, start)) = anchor.filter(|_| suggestion.contains('@')) {
                *suggestion = self.source_label(&path, start)?;
            }
        }
        self.display_local_candidates(&mut candidates)?;
        let mut spellings = HashMap::new();
        for candidate in &candidates {
            *spellings
                .entry(candidate.selector.clone())
                .or_insert(0usize) += 1;
        }
        for candidate in &mut candidates {
            if spellings[&candidate.selector] > 1
                && let Some(location) = &candidate.location
            {
                let label = self.source_label(&location.path, location.start)?;
                candidate.qualified_name = Some(std::mem::replace(&mut candidate.selector, label));
            }
        }
        Ok(match candidates.len() {
            0 => Resolution::Unknown {
                reason: location_reason.unwrap_or_else(|| {
                    if located.is_some() {
                        "A symbol exists at this position, but it does not match the selected filters."
                    } else {
                        "No symbol matches this selector and its filters."
                    }.to_owned()
                }),
                suggestions,
            },
            1 => Resolution::Ok { symbol: Box::new(candidates.remove(0)) },
            _ => Resolution::Ambiguous { candidates },
        })
    }
    /// Byte offsets anchor local identities but are not source spellings. A
    /// unique local is selected by its scoped name; a shadowed one by position.
    fn display_local_candidates(&mut self, candidates: &mut [SelectedSymbol]) -> Result<()> {
        let mut counts: HashMap<String, usize> = candidates
            .iter()
            .filter_map(|candidate| local_plain(&candidate.kind, &candidate.selector))
            .map(|plain| (plain.to_owned(), 0))
            .collect();
        if counts.is_empty() {
            return Ok(());
        }
        for symbol in self
            .session
            .snapshot
            .semantic
            .symbols
            .iter()
            .filter(|s| matches!(s.kind, SymbolKind::Binding | SymbolKind::Parameter))
        {
            if let Some(count) = local_plain(&symbol.kind, &self.selector(symbol))
                .and_then(|plain| counts.get_mut(plain))
            {
                *count += 1;
            }
        }
        for candidate in candidates {
            let Some(plain) = local_plain(&candidate.kind, &candidate.selector).map(str::to_owned)
            else {
                continue;
            };
            if counts[&plain] == 1 {
                candidate.selector = plain;
            } else if let Some(location) = &candidate.location {
                let label = self.source_label(&location.path, location.start)?;
                candidate.qualified_name = Some(plain);
                candidate.selector = label;
            }
        }
        Ok(())
    }
    pub fn query(
        &mut self,
        command: &str,
        selector: &str,
        filter: &SelectorFilter,
        all: bool,
    ) -> Result<Value> {
        let command = DirectCommand::parse(command).context("unsupported direct command")?;
        Ok(serde_json::to_value(
            self.query_typed(command, selector, filter, all)?,
        )?)
    }
    pub fn query_typed(
        &mut self,
        command: DirectCommand,
        selector: &str,
        filter: &SelectorFilter,
        all: bool,
    ) -> Result<DirectQueryResult> {
        use super::query_result::{QueryData, QueryResult, Status, SymbolInfo};
        let revision = self.session.revision().to_owned();
        if command == DirectCommand::Type
            && let Some((file, byte)) = self.location(selector)?
        {
            if filter.kind.is_some() || filter.module.is_some() || filter.package.is_some() {
                let resolved = self.resolve_typed(selector, filter)?;
                if !matches!(resolved, Resolution::Ok { .. }) {
                    return Ok(resolved.into());
                }
            }
            let mut result: DirectQueryResult = if self.source(&file)?.is_between_tokens(byte) {
                QueryResult::empty(Status::Unknown)
            } else {
                self.session.type_query(file.clone(), byte)
            }
            .into();
            if result.status == Status::Unknown {
                result.reason = Some(format!(
                    "No typed expression or declaration at this position. {}",
                    self.missing_symbol_reason(&file, byte)?
                ));
            }
            return Ok(result);
        }
        let resolved = self.resolve_typed(selector, filter)?;
        let Resolution::Ok { symbol } = resolved else {
            let mut result: DirectQueryResult = resolved.into();
            if command == DirectCommand::Effects {
                result.schema = Some(super::effect_states::SCHEMA);
            }
            return Ok(result);
        };
        let symbol = *symbol;
        let function = if matches!(command, DirectCommand::Effects | DirectCommand::Impact) {
            self.constructor_bodies
                .get(&symbol.id)
                .unwrap_or(&symbol.id)
        } else {
            &symbol.id
        }
        .clone();
        if command == DirectCommand::Impact {
            let mut seeds = vec![function.clone()];
            if !self.function_names.contains_key(&function) || symbol.kind == SymbolKind::Method {
                if symbol.kind != SymbolKind::Method {
                    let mut result =
                        DirectQueryResult::new(Status::Unanalyzed, DirectQueryData::Empty {});
                    result.reason = Some("Impact requires a function or method.".into());
                    result.selected = Some(symbol);
                    return Ok(result);
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
                    let mut result =
                        DirectQueryResult::new(Status::Unanalyzed, DirectQueryData::Empty {});
                    result.reason = Some("This interface contract has no analyzed implementation in the selected source graph.".into());
                    result.selected = Some(symbol);
                    return Ok(result);
                }
            }
            let mut impact = self.session.snapshot.impact(
                &seeds,
                Direction::Callers,
                Limits::default(),
                Some(&revision),
            )?;
            for node in &mut impact.nodes {
                node.selector = self.function_names.get(&node.id).cloned();
                if self.lambda_symbols.contains_key(&node.id) {
                    node.kind = Some(super::graph::ImpactKind::Lambda);
                }
                node.relation = Some(match node.distance {
                    0 => super::graph::ImpactRelation::Definition,
                    1 => super::graph::ImpactRelation::DirectCaller,
                    _ => super::graph::ImpactRelation::IndirectCaller,
                });
            }
            impact.nodes.sort_by_cached_key(|node| {
                let lambda = self
                    .lambda_symbols
                    .get(&node.id)
                    .and_then(|s| s.location.as_ref());
                (
                    node.distance,
                    lambda.is_some(),
                    lambda.map_or_else(
                        || node.selector.as_deref().unwrap_or_default().to_owned(),
                        |l| l.path.clone(),
                    ),
                    lambda.map_or(0, |l| l.start),
                )
            });
            return Ok(DirectQueryResult::new(
                Status::Ok,
                DirectQueryData::Impact { impact, symbol },
            ));
        }
        let mut queried = match command {
            DirectCommand::Refs | DirectCommand::References => {
                self.session.references_query(&function)
            }
            DirectCommand::Effects => self.session.effects_query(&function),
            DirectCommand::Symbol | DirectCommand::Type => self.session.symbol_query(&function),
            _ => anyhow::bail!("unsupported direct command"),
        };
        if matches!(command, DirectCommand::Refs | DirectCommand::References)
            && symbol.kind == SymbolKind::Method
            && let QueryData::References { coverage, .. } = &mut queried.data
        {
            *coverage = Some(format!(
                "{}; override and implementation declarations are excluded; impact lists them with direct and indirect callers",
                coverage.as_deref().unwrap_or("compiler-resolved")
            ));
        }
        let mut type_display = None;
        let has_display = matches!(&queried.data,
            QueryData::Symbol { symbol: SymbolInfo::Declaration(declaration), .. }
                if declaration.type_display.is_some());
        if command == DirectCommand::Symbol
            && !has_display
            && let Some(location) = &symbol.location
        {
            let typed = self
                .session
                .type_query(location.path.clone(), location.start);
            if let QueryData::Type(typed) = typed.data {
                type_display = typed.type_display;
            }
        }
        if command == DirectCommand::Type
            && let Some(location) = &symbol.location
        {
            queried = self
                .session
                .type_query(location.path.clone(), location.start);
        }
        let mut result: DirectQueryResult = queried.into();
        result.type_display = type_display;
        result.selected = Some(symbol);
        if let DirectQueryData::Semantic(QueryData::References { references, .. }) =
            &mut result.data
        {
            let total = references.len();
            if !all {
                references.truncate(50);
            }
            let shown = references.len();
            result.total = Some(total);
            result.shown = Some(shown);
            result.truncated = Some(shown < total);
        }
        if let Some(effects) = result.effects_mut() {
            self.spell_lock_targets(effects)?;
        }
        Ok(result)
    }
    /// Name each acquired lock by its operand's source text, so evidence reads
    /// `locked=table: Mutex<..>` without a source lookup. Whitespace collapses
    /// and long operands are shortened; the location keeps the exact range.
    /// Linear in the operand text of the reported witnesses.
    fn spell_lock_targets(&mut self, result: &mut super::query_result::EffectResult) -> Result<()> {
        use super::evidence::SourcePoint;
        const LIMIT: usize = 80;
        for fact in result
            .effect_evidence
            .iter_mut()
            .flatten()
            .chain(result.compiler_witnesses.iter_mut().flatten().flatten())
        {
            let Some(lock) = fact
                .witness
                .as_mut()
                .and_then(|w| w.cause.as_mut())
                .and_then(|cause| cause.lock.as_mut())
            else {
                continue;
            };
            let Some(SourcePoint::Located(target)) = &lock.target else {
                continue;
            };
            let text = self
                .source(&target.path)?
                .text
                .get(target.start..target.end)
                .context("invalid-location: lock operand is outside its source")?;
            let mut expression = String::with_capacity(text.len());
            for piece in text.split_whitespace() {
                if !expression.is_empty()
                    && !expression.ends_with('.')
                    && !piece.starts_with(['.', '?'])
                {
                    expression.push(' ');
                }
                expression.push_str(piece);
            }
            if let Some((cut, _)) = expression.char_indices().nth(LIMIT) {
                expression.truncate(cut);
                expression.push('…');
            }
            lock.expression = Some(expression);
        }
        Ok(())
    }
    /// Human-only labels. Persisted names and snapshot identities are untouched.
    pub fn display_function_targets(
        &mut self,
        result: &mut DirectQueryResult,
        absolute: bool,
    ) -> Result<()> {
        let mut ids = std::collections::HashSet::new();
        if let Some(selected) = &result.selected {
            ids.insert(selected.id.as_str());
        }
        if let DirectQueryData::Impact { impact, symbol } = &result.data {
            ids.insert(symbol.id.as_str());
            ids.extend(impact.nodes.iter().map(|node| node.id.as_str()));
        }
        if let Some(effects) = result.effects() {
            for fact in effects
                .effect_evidence
                .iter()
                .flatten()
                .chain(effects.compiler_witnesses.iter().flatten().flatten())
            {
                ids.extend(fact.via_function.as_deref());
                ids.extend(fact.witness.as_ref().and_then(|w| w.owner.as_deref()));
                ids.extend(fact.call_chain.iter().flatten().map(String::as_str));
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
        if let Some(selected) = &mut result.selected
            && let Some(label) = labels.get(&selected.id)
        {
            selected.selector.clone_from(label);
        }
        if let DirectQueryData::Impact { impact, symbol } = &mut result.data {
            if let Some(label) = labels.get(&symbol.id) {
                symbol.selector.clone_from(label);
            }
            for node in &mut impact.nodes {
                if let Some(label) = labels.get(&node.id) {
                    node.selector = Some(label.clone());
                }
            }
        }
        let name = |id: &str| labels.get(id).or_else(|| self.function_names.get(id));
        if let Some(effects) = result.effects_mut() {
            for cause in effects.unknown_causes.iter_mut().flatten() {
                if let Some(name) = name(&cause.function) {
                    cause.name.clone_from(name);
                }
            }
            for fact in effects
                .effect_evidence
                .iter_mut()
                .flatten()
                .chain(effects.compiler_witnesses.iter_mut().flatten().flatten())
            {
                for step in fact.call_chain.iter_mut().flatten() {
                    if let Some(label) = name(step) {
                        step.clone_from(label);
                    }
                }
                if let Some(via) = &mut fact.via_function
                    && let Some(label) = name(via)
                {
                    via.clone_from(label);
                }
                if let Some(owner) = fact.witness.as_mut().and_then(|w| w.owner.as_mut())
                    && let Some(label) = name(owner)
                {
                    owner.clone_from(label);
                }
            }
        }
        Ok(())
    }
    fn display_path(&self, path: &mut String, absolute: bool) {
        if !absolute && let Ok(relative) = Path::new(path).strip_prefix(self.workspace()) {
            let relative = relative.to_string_lossy().replace('\\', "/");
            *path = if relative.is_empty() {
                ".".into()
            } else {
                relative
            };
        }
    }
    fn display_identity(&self, identity: &mut Option<SymbolIdentity>, absolute: bool) {
        if let Some(identity) = identity {
            self.display_symbol_identity(identity, absolute);
        }
    }
    fn display_symbol_identity(&self, identity: &mut SymbolIdentity, absolute: bool) {
        use crate::package::PackageSourceIdentity;
        self.display_path(&mut identity.module, absolute);
        match &mut identity.package.source {
            PackageSourceIdentity::Path { path } => {
                if !absolute && let Ok(relative) = path.strip_prefix(self.workspace()) {
                    let relative = relative.to_string_lossy().replace('\\', "/");
                    *path = if relative.is_empty() {
                        PathBuf::from(".")
                    } else {
                        PathBuf::from(relative)
                    };
                }
            }
            PackageSourceIdentity::GitSubdirectory { path, .. } => {
                self.display_path(path, absolute)
            }
            PackageSourceIdentity::Git { .. } => {}
        }
    }
    fn display_rust_crate(
        &mut self,
        dependency: &mut interop::RustCrateIdentity,
        absolute: bool,
    ) -> Result<()> {
        use interop::RustCrateSource;
        match &mut dependency.source {
            RustCrateSource::Project(source) => {
                if let Some(path) = &mut source.path {
                    self.display_path(path, absolute);
                }
            }
            RustCrateSource::Local { path, .. } => self.display_path(path, absolute),
            RustCrateSource::Remote(_) => {}
            RustCrateSource::Other(value) => self.display_locations(value, absolute)?,
        }
        Ok(())
    }
    fn display_location(&mut self, location: &mut Location, absolute: bool) -> Result<()> {
        let (line, column) = self.source(&location.path)?.position(location.start)?;
        location.display = Some(Box::new(super::DisplayPosition { line, column }));
        if !absolute {
            location.path = Path::new(&location.path)
                .strip_prefix(self.workspace())
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| location.path.clone());
        }
        Ok(())
    }
    fn display_selected(&mut self, symbol: &mut SelectedSymbol, absolute: bool) -> Result<()> {
        self.display_identity(&mut symbol.identity, absolute);
        if let Some(location) = &mut symbol.location {
            let (line, column) = self.source(&location.path)?.position(location.start)?;
            location.line = Some(line);
            location.column = Some(column);
            if !absolute {
                location.path = Path::new(&location.path)
                    .strip_prefix(self.workspace())
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| location.path.clone());
            }
        }
        Ok(())
    }
    fn display_assumption(
        &mut self,
        assumption: &mut super::assumptions::Assumption,
        absolute: bool,
    ) -> Result<()> {
        if let Some(location) = &mut assumption.location
            && let Some(path) = &mut location.path
        {
            let (line, column) = self.source(path)?.position(location.start)?;
            location.display = Some(Box::new(super::DisplayPosition { line, column }));
            if !absolute {
                *path = Path::new(path)
                    .strip_prefix(self.workspace())
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| path.clone());
            }
        }
        Ok(())
    }
    fn display_effect_state(
        &mut self,
        state: &mut super::effect_states::EffectState,
        absolute: bool,
    ) -> Result<()> {
        if let Some(assumption) = &mut state.assumption {
            self.display_assumption(assumption, absolute)?;
        }
        if let Some(witness) = &mut state.witness {
            self.display_location(&mut witness.location, absolute)?;
        }
        for reason in &mut state.unknown_reasons {
            if let Some(location) = &mut reason.location {
                self.display_location(location, absolute)?;
            }
        }
        Ok(())
    }
    fn display_effect(
        &mut self,
        fact: &mut super::evidence::ExplainedEffect,
        absolute: bool,
    ) -> Result<()> {
        use super::evidence::SourcePoint;
        if let Some(witness) = &mut fact.witness
            && let Some(cause) = &mut witness.cause
        {
            if let Some(location) = cause.location_mut() {
                self.display_location(location, absolute)?;
            }
            if let Some(lock) = &mut cause.lock
                && let Some(SourcePoint::Located(location)) = &mut lock.target
            {
                self.display_location(location, absolute)?;
            }
        }
        if let Some(paths) = &mut fact.paths
            && let Some(Some(location)) = &mut paths.location
        {
            self.display_location(location, absolute)?;
        }
        if let Some(task) = &mut fact.task_relationship
            && let Some(location) = &mut task.location
        {
            self.display_location(location, absolute)?;
        }
        Ok(())
    }
    fn display_effects(
        &mut self,
        effects: &mut super::query_result::EffectResult,
        absolute: bool,
    ) -> Result<()> {
        for state in &mut effects.effects {
            self.display_effect_state(state, absolute)?;
        }
        if let Some(assumptions) = &mut effects.assumptions {
            for assumption in [
                &mut assumptions.no_block,
                &mut assumptions.no_io,
                &mut assumptions.no_wait,
            ]
            .into_iter()
            .flatten()
            {
                self.display_assumption(assumption, absolute)?;
            }
        }
        for cause in effects.unknown_causes.iter_mut().flatten() {
            self.display_path(&mut cause.module, absolute);
            for location in &mut cause.locations {
                self.display_location(location, absolute)?;
            }
            for site in cause.call_sites.iter_mut().flatten() {
                self.display_location(&mut site.location, absolute)?;
            }
            if let Some(location) = &mut cause.caller_location {
                self.display_location(location, absolute)?;
            }
        }
        for fact in effects
            .effect_evidence
            .iter_mut()
            .flatten()
            .chain(effects.compiler_witnesses.iter_mut().flatten().flatten())
        {
            self.display_effect(fact, absolute)?;
        }
        Ok(())
    }
    fn display_symbol_record(
        &mut self,
        symbol: &mut super::query_result::SymbolRecord,
        absolute: bool,
    ) -> Result<()> {
        self.display_identity(&mut symbol.identity, absolute);
        if let Some(location) = &mut symbol.location {
            self.display_location(location, absolute)?;
        }
        Ok(())
    }
    fn display_reference(
        &mut self,
        reference: &mut super::query_result::ReferenceRecord,
        absolute: bool,
    ) -> Result<()> {
        self.display_identity(&mut reference.identity, absolute);
        self.display_location(&mut reference.location, absolute)
    }
    pub fn display_query_locations(
        &mut self,
        result: &mut DirectQueryResult,
        absolute: bool,
    ) -> Result<()> {
        use super::query_result::{QueryData, SymbolInfo};
        if let Some(selected) = &mut result.selected {
            self.display_selected(selected, absolute)?;
        }
        match &mut result.data {
            DirectQueryData::Resolved { symbol } => self.display_selected(symbol, absolute)?,
            DirectQueryData::Ambiguous { candidates } => {
                for candidate in candidates {
                    self.display_selected(candidate, absolute)?;
                }
            }
            DirectQueryData::Impact { impact, symbol } => {
                self.display_selected(symbol, absolute)?;
                for node in &mut impact.nodes {
                    self.display_identity(&mut node.identity, absolute);
                    for state in &mut node.effects {
                        self.display_effect_state(state, absolute)?;
                    }
                }
                for dependency in &mut impact.rust_dependencies {
                    self.display_rust_crate(dependency, absolute)?;
                }
                for boundary in &mut impact.dependency_boundary {
                    if let Some(dependency) = &mut boundary.crate_identity {
                        self.display_rust_crate(dependency, absolute)?;
                    }
                    self.display_identity(&mut boundary.identity, absolute);
                }
                for dependency in &mut impact.external_dependencies {
                    self.display_symbol_identity(&mut dependency.from, absolute);
                    self.display_symbol_identity(&mut dependency.target, absolute);
                }
            }
            DirectQueryData::Semantic(data) => match data {
                QueryData::Symbol {
                    symbol,
                    resolved_symbols,
                } => {
                    match symbol {
                        SymbolInfo::Declaration(symbol) => {
                            self.display_symbol_record(symbol, absolute)?
                        }
                        SymbolInfo::Function(function) => {
                            self.display_identity(&mut function.identity, absolute);
                            self.display_path(&mut function.module, absolute);
                            for location in &mut function.locations {
                                self.display_location(location, absolute)?;
                            }
                        }
                    }
                    for symbol in resolved_symbols.iter_mut().flatten() {
                        self.display_symbol_record(symbol, absolute)?;
                    }
                }
                QueryData::Symbols { symbols, .. } => {
                    for symbol in symbols {
                        self.display_symbol_record(symbol, absolute)?;
                    }
                }
                QueryData::Type(typed) => {
                    if let Some(location) = &mut typed.location {
                        self.display_location(location, absolute)?;
                    }
                }
                QueryData::References {
                    references,
                    unresolved_candidates,
                    ..
                } => {
                    for reference in references
                        .iter_mut()
                        .chain(unresolved_candidates.iter_mut().flatten())
                    {
                        self.display_reference(reference, absolute)?;
                    }
                }
                QueryData::Effects(effects) => self.display_effects(effects, absolute)?,
                QueryData::Empty {} => {}
            },
            DirectQueryData::Unknown { .. } | DirectQueryData::Empty {} => {}
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
    fn bundled_source_locations_use_embedded_bytes_and_verify_hash() {
        let path = "<std>/collections.wi";
        let mut snapshot = Snapshot {
            edit_context: None,
            version: 1,
            compiler: storage::compiler_stamp(),
            compatibility: String::new(),
            workspace: "/virtual".into(),
            revision: String::new(),
            sources: BTreeMap::from([(path.into(), hash(bundled_source(path).unwrap()))]),
            semantic: Default::default(),
            functions: vec![],
        };
        snapshot.revision = snapshot.digest().unwrap();
        let mut session = DirectSession::new(snapshot).unwrap();
        assert_eq!(
            session.location(&format!("{path}:1:1")).unwrap(),
            Some((path.into(), 0))
        );
        assert!(session.location("<std>/missing.wi:1:1").is_err());
        session.sources.clear();
        session
            .session
            .snapshot
            .sources
            .insert(path.into(), "stale".into());
        let error = session.location(&format!("{path}:1:1")).unwrap_err();
        assert!(error.to_string().contains("stale: source changed"));
    }

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
                            synthetic_kind: crate::qualified_name::SyntheticKind::Source,
                            identity: None,
                            source_name: None,
                            body_id: None,
                            body_location: None,
                            rename_calls: vec![],
                            rename_imports: vec![],
                            id: format!("f{i}"),
                            module: path.into(),
                            name: format!("<lambda main@{start}:{}>", start + 4),
                            locations: vec![Location {
                                display: None,
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
                use super::super::{
                    evidence::{ExplainedEffect, Witness, WitnessKind},
                    query_result::{EffectResult, QueryData, Status},
                };
                let facts = (0..n * repeats)
                    .map(|i| {
                        let id = format!("f{}", i % n);
                        let mut witness = Witness::new(WitnessKind::RuntimeCapability);
                        witness.owner = Some(id.clone());
                        ExplainedEffect::new(1, witness, id)
                    })
                    .collect();
                let effects = EffectResult {
                    schema: super::super::effect_states::SCHEMA,
                    effects: vec![],
                    reason: None,
                    assumptions: None,
                    unknown_causes: None,
                    bounds: None,
                    effect_evidence: Some(facts),
                    no_panic: None,
                    witness: None,
                    compiler_witnesses: None,
                    meaning: None,
                };
                let mut result = DirectQueryResult::new(
                    Status::Ok,
                    DirectQueryData::Semantic(QueryData::Effects(effects)),
                );
                session
                    .display_function_targets(&mut result, false)
                    .unwrap();
                for (i, fact) in result
                    .effects()
                    .unwrap()
                    .effect_evidence
                    .as_ref()
                    .unwrap()
                    .iter()
                    .enumerate()
                {
                    let anchor = format!("main.wi:{}:2", i % n + 1);
                    assert_eq!(fact.via_function.as_deref(), Some(anchor.as_str()));
                    assert_eq!(
                        fact.witness.as_ref().unwrap().owner.as_deref(),
                        Some(anchor.as_str())
                    );
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

/// A local's scoped name without its internal `@byte` identity anchor.
fn local_plain<'a>(kind: &SymbolKind, selector: &'a str) -> Option<&'a str> {
    matches!(kind, SymbolKind::Binding | SymbolKind::Parameter)
        .then(|| selector.rsplit_once('@'))
        .flatten()
        .filter(|(_, offset)| !offset.is_empty() && offset.bytes().all(|b| b.is_ascii_digit()))
        .map(|(name, _)| name)
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

/// Compare only the declaration suffix, without allocating or rescanning paths.
fn wrong_module_suggestion(name: &str, module: &str, selector: &str) -> bool {
    QualifiedName::new(name)
        .strip_qualifier(module)
        .and_then(|item| QualifiedName::new(selector).strip_tail(item))
        .is_some_and(|prefix| !prefix.is_empty() && prefix != module)
}

/// Compare aligned path segments from the leaf, permitting the same omitted
/// module prefix as exact resolution. Each segment is visited at most once.
fn selector_suggestion_score(name: &str, segments: &[(&str, Vec<char>)]) -> Option<usize> {
    let mut names = QualifiedName::new(name).rev_segments();
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

/// Banded optimal-string-alignment distance (Levenshtein plus adjacent
/// transpositions), bounded to two edits. O(a+b) work and O(1) distance-band
/// storage, plus O(a+b) Unicode scalar storage.
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
    // Row i-2 shares band index k with cell (i-2, j-2).
    let mut before = [3usize; 5];
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
            let transposition = if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                before[k] + 1
            } else {
                3
            };
            current[k] = diagonal.min(deletion).min(insertion).min(transposition);
        }
        before = previous;
        previous = current;
    }
    let distance = previous[b.len() + limit - a.len()];
    (distance <= limit).then_some(distance)
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
    fn wrong_module_suggestions_preserve_the_entire_declaration_path() {
        for (name, module, selector, expected) in [
            ("ow::Base::count", "ow", "main::Base::count", true),
            ("ow::Base::count", "ow", "other::nested::Base::count", true),
            ("pkg::ow::Base::count", "pkg::ow", "main::Base::count", true),
            ("ow::count", "ow", "main::count", true),
            ("ow::Base::count", "ow", "main::Other::count", false),
            ("ow::Base::count", "ow", "main::Base::coun", false),
            ("ow::Base::count", "ow", "Base::count", false),
            ("ow::Base::count", "ow", "::Base::count", false),
            ("ow::Base::count", "ow", "ow::Base::count", false),
        ] {
            assert_eq!(
                super::wrong_module_suggestion(name, module, selector),
                expected
            );
        }
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
            ("sim::run", "sim::rnu", Some(3)),
            ("sim::run", "sim::urn", Some(3)),
            ("sim::run", "smi::run", Some(3)),
            ("sim::run", "sim::nur", None),
            ("eval::Recursive::cell", "eval::Recursive::clel", Some(3)),
            ("eval::Recursive::cell", "eval::Rceursive::cell", Some(3)),
            ("eval::Recursive::cell", "eval::Rceursive::clel", Some(6)),
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
        // Full-matrix optimal string alignment distance.
        fn full(a: &str, b: &str) -> usize {
            let (a, b): (Vec<_>, Vec<_>) = (a.chars().collect(), b.chars().collect());
            let mut d = vec![vec![0; b.len() + 1]; a.len() + 1];
            for i in 0..=a.len() {
                for j in 0..=b.len() {
                    d[i][j] = if i == 0 || j == 0 {
                        i + j
                    } else {
                        let mut v = (d[i - 1][j - 1] + usize::from(a[i - 1] != b[j - 1]))
                            .min(d[i - 1][j] + 1)
                            .min(d[i][j - 1] + 1);
                        if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                            v = v.min(d[i - 2][j - 2] + 1);
                        }
                        v
                    };
                }
            }
            d[a.len()][b.len()]
        }
        let words = [
            "",
            "a",
            "abc",
            "acb",
            "bac",
            "run",
            "rnu",
            "urn",
            "ca",
            "abcd",
            "badc",
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

#[derive(Serialize)]
#[serde(untagged)]
pub enum RenameResult {
    Resolution(Box<DirectQueryResult>),
    Preview(RenamePreviewResult),
    Applied(RenameApplied),
}
impl RenameResult {
    pub fn status(&self) -> query_result::Status {
        match self {
            Self::Resolution(r) => r.status,
            Self::Preview(r) => r.preview.status,
            Self::Applied(r) => r.status,
        }
    }
}
#[derive(Serialize)]
pub struct RenamePreviewResult {
    #[serde(flatten)]
    pub preview: edit::RenamePreview,
    pub edits: usize,
    pub declarations_updated: usize,
    pub references_updated: usize,
    pub selected: SelectedSymbol,
}
#[derive(Serialize)]
pub struct RenameApplied {
    pub status: query_result::Status,
    pub old: String,
    pub new: String,
    pub validation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction: Option<String>,
    pub selected: SelectedSymbol,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<String>>,
    pub files_changed: usize,
    pub edits: usize,
    pub declarations_updated: usize,
    pub references_updated: usize,
}
