//! Bounded source structure projected from a single compiler query session.
use super::{direct::DirectSession, symbols::Symbol};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{self, Write},
    path::Path,
};

pub const HARD_LIMIT: usize = 1024 * 1024;
pub const KINDS: &[&str] = &[
    "function",
    "class",
    "interface",
    "enum",
    "field",
    "static-field",
    "method",
    "constructor",
    "variant",
];
#[derive(Clone, Serialize)]
pub struct Entry {
    pub name: String,
    pub kind: String,
    pub selector: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_async: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub type_params: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_display: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_count: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<Entry>,
}
#[derive(Default, Clone, Serialize)]
pub struct Counts {
    pub files: usize,
    pub top_level_symbols: usize,
    pub members: usize,
    pub hidden_members: usize,
    pub kinds: BTreeMap<String, usize>,
}
#[derive(Clone, Serialize)]
pub struct File {
    pub path: String,
    pub module: String,
    pub symbols: Vec<Entry>,
    pub counts: Counts,
}
#[derive(Serialize)]
pub struct Overview {
    pub schema: u32,
    pub kind: &'static str,
    pub status: &'static str,
    pub target: String,
    pub requested_depth: u8,
    pub effective_depth: Option<u8>,
    pub truncated: bool,
    pub fallback: &'static str,
    pub max_chars: usize,
    pub files: Vec<File>,
    pub counts: Counts,
    pub root_errors: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<&'static str>,
    #[serde(skip)]
    pub symbol_visits: usize,
}
impl Overview {
    pub fn empty(target: String, depth: u8, max_chars: usize, errors: usize) -> Self {
        Self {
            schema: 1,
            kind: "overview",
            status: if errors == 0 { "ok" } else { "incomplete" },
            target,
            requested_depth: depth,
            effective_depth: Some(depth),
            truncated: false,
            fallback: "none",
            max_chars,
            files: Vec::new(),
            counts: Counts::default(),
            root_errors: errors,
            hint: (errors > 0).then_some("Run willow check for diagnostics."),
            symbol_visits: 0,
        }
    }
    pub fn project(
        session: &DirectSession,
        target: &Path,
        depth: u8,
        max_chars: usize,
        kind: Option<&str>,
        absolute: bool,
    ) -> Self {
        let snapshot = session.snapshot();
        let display = |path: &Path| {
            let path = if absolute {
                path
            } else {
                path.strip_prefix(session.workspace()).unwrap_or(path)
            };
            if path.as_os_str().is_empty() {
                ".".into()
            } else {
                path.to_string_lossy().replace('\\', "/")
            }
        };
        let mut output = Self::empty(display(target), depth, max_chars, 0);
        let directory = target.is_dir();
        let root_package = snapshot
            .semantic
            .modules
            .first()
            .map(|m| &m.identity.package);
        let project_paths: HashSet<_> = snapshot
            .semantic
            .modules
            .iter()
            .filter(|m| Some(&m.identity.package) == root_package)
            .map(|m| m.path.as_str())
            .collect();
        let paths: Vec<_> = snapshot
            .sources
            .keys()
            .filter(|p| {
                if root_package.is_some() && !project_paths.contains(p.as_str()) {
                    return false;
                }
                let p = Path::new(p);
                if directory {
                    p.starts_with(target)
                } else {
                    p == target
                }
            })
            .collect();
        let mut files = HashMap::new();
        for path in paths {
            files.insert(path.as_str(), output.files.len());
            let module = Path::new(path)
                .strip_prefix(session.workspace())
                .unwrap_or(Path::new(path))
                .with_extension("");
            output.files.push(File {
                path: display(Path::new(path)),
                module: module.to_string_lossy().replace(['/', '\\'], "::"),
                symbols: Vec::new(),
                counts: Counts::default(),
            });
        }
        for module in &snapshot.semantic.modules {
            if let Some(&file) = files.get(module.path.as_str()) {
                output.files[file].module = module.identity.module.clone();
            }
        }
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        for symbol in &snapshot.semantic.symbols {
            output.symbol_visits += 1;
            let Some(loc) = &symbol.location else {
                continue;
            };
            let Some(&file) = files.get(loc.path.as_str()) else {
                continue;
            };
            if symbol.kind == "module" {
                output.files[file].module = symbol.name.clone();
            }
            if !KINDS.contains(&symbol.kind.as_str()) || symbol.name.contains('$') {
                continue;
            }
            if !seen.insert((loc.path.as_str(), loc.start, loc.end, symbol.kind.as_str())) {
                continue;
            }
            candidates.push((file, symbol));
        }
        // Fixed-width radix ordering is linear even for generated files with many declarations.
        output.symbol_visits += radix_source_order(&mut candidates);
        let mut owners = HashMap::new();
        let mut top = Vec::new();
        let mut children: Vec<Vec<&Symbol>> = Vec::new();
        for &(file, symbol) in &candidates {
            output.symbol_visits += 1;
            if is_top(symbol) {
                let name = symbol.source_name.as_deref().unwrap_or(&symbol.name);
                owners.insert((file, name), top.len());
                top.push((file, symbol));
                children.push(Vec::new());
            }
        }
        for &(file, symbol) in &candidates {
            output.symbol_visits += 1;
            if is_top(symbol) {
                continue;
            }
            let owner = symbol
                .source_name
                .as_deref()
                .and_then(|s| s.rsplit_once("::"))
                .map(|(o, _)| o);
            if let Some(&index) = owner.and_then(|o| owners.get(&(file, o))) {
                children[index].push(symbol);
            }
        }
        for ((file, symbol), mut members) in top.into_iter().zip(children) {
            output.symbol_visits += 1;
            if let Some(kind) = kind {
                members.retain(|m| m.kind == kind);
                if symbol.kind != kind && members.is_empty() {
                    continue;
                }
            }
            let file = &mut output.files[file];
            let mut projected = entry(session, symbol);
            if matches!(symbol.kind.as_str(), "class" | "interface" | "enum") {
                projected.member_count = Some(members.len());
            }
            file.counts.top_level_symbols += 1;
            *file.counts.kinds.entry(symbol.kind.clone()).or_default() += 1;
            file.counts.members += members.len();
            for member in members {
                output.symbol_visits += 1;
                *file.counts.kinds.entry(member.kind.clone()).or_default() += 1;
                if depth == 1 {
                    projected.members.push(entry(session, member));
                }
            }
            file.symbols.push(projected);
        }
        for file in &mut output.files {
            file.counts.files = 1;
            file.counts.hidden_members = if depth == 0 { file.counts.members } else { 0 };
            output.counts.files += 1;
            output.counts.top_level_symbols += file.counts.top_level_symbols;
            output.counts.members += file.counts.members;
            output.counts.hidden_members += file.counts.hidden_members;
            for (kind, count) in &file.counts.kinds {
                *output.counts.kinds.entry(kind.clone()).or_default() += count;
            }
        }
        output
    }
    /// Each failed attempt owns only a bounded buffer, dropped before retrying.
    pub fn render(mut self, format: &str, all: bool) -> anyhow::Result<String> {
        anyhow::ensure!(
            matches!(format, "human" | "json" | "ndjson"),
            "invalid overview format"
        );
        let limit = if all { usize::MAX } else { self.max_chars };
        loop {
            if let Ok(text) = self.attempt(format, limit) {
                return Ok(text);
            }
            self.truncated = true;
            self.hint = Some(if self.root_errors > 0 {
                "Narrow the path or use --depth 0. Run willow check for diagnostics."
            } else {
                "Narrow the path or use --depth 0."
            });
            match self.effective_depth {
                Some(1) => {
                    self.effective_depth = Some(0);
                    self.fallback = "depth0";
                    for file in &mut self.files {
                        for symbol in &mut file.symbols {
                            symbol.members.clear();
                        }
                        file.counts.hidden_members = file.counts.members;
                    }
                    self.counts.hidden_members = self.counts.members;
                }
                Some(_) => {
                    self.effective_depth = None;
                    self.fallback = "counts-only";
                    for file in &mut self.files {
                        file.symbols.clear();
                    }
                }
                None => {
                    // Whole-project totals remain when even per-file counts cannot fit.
                    // This indivisible metadata may exceed an impossibly small soft budget.
                    self.files.clear();
                    self.fallback = "counts-only";
                    return self.attempt(format, usize::MAX).map_err(Into::into);
                }
            }
        }
    }
    fn attempt(&self, format: &str, limit: usize) -> io::Result<String> {
        let mut writer = BoundedWriter {
            output: Vec::new(),
            chars: 0,
            limit,
        };
        if format == "human" {
            self.human(&mut writer)?;
        } else {
            serde_json::to_writer(&mut writer, self).map_err(io::Error::other)?;
            writer.write_all(b"\n")?;
        }
        String::from_utf8(writer.output).map_err(io::Error::other)
    }
    fn human(&self, out: &mut impl Write) -> io::Result<()> {
        writeln!(out, "overview {}: {}", self.target, self.status)?;
        for file in &self.files {
            writeln!(out, "{} ({})", file.path, file.module)?;
            for symbol in &file.symbols {
                human_entry(out, symbol, "  ")?;
                for member in &symbol.members {
                    human_entry(out, member, "    ")?;
                }
            }
            if self.effective_depth.is_none() {
                writeln!(out, "  {:?}", file.counts.kinds)?;
            }
        }
        writeln!(
            out,
            "{} files, {} top-level symbols, {} hidden members",
            self.counts.files, self.counts.top_level_symbols, self.counts.hidden_members
        )?;
        writeln!(
            out,
            "requested_depth={} effective_depth={} truncated={} fallback={} max_chars={} root_errors={}",
            self.requested_depth,
            self.effective_depth
                .map_or("counts".into(), |d| d.to_string()),
            self.truncated,
            self.fallback,
            self.max_chars,
            self.root_errors
        )?;
        if let Some(hint) = self.hint {
            writeln!(out, "{hint}")?;
        }
        Ok(())
    }
}
fn is_top(s: &Symbol) -> bool {
    matches!(s.kind.as_str(), "function" | "class" | "enum" | "interface")
}
fn entry(session: &DirectSession, symbol: &Symbol) -> Entry {
    Entry {
        name: symbol
            .name
            .rsplit("::")
            .next()
            .unwrap_or(&symbol.name)
            .into(),
        kind: symbol.kind.clone(),
        selector: session.selector(symbol),
        is_async: symbol.details.is_async,
        type_params: symbol.details.type_params.clone(),
        type_display: symbol.ty.as_ref().map(|ty| {
            session
                .session
                .names
                .render(ty, symbol.identity.as_ref().map(|i| i.module.as_str()))
        }),
        member_count: None,
        members: Vec::new(),
    }
}
fn human_entry(out: &mut impl Write, s: &Entry, indent: &str) -> io::Result<()> {
    write!(out, "{indent}")?;
    if s.is_async {
        write!(out, "async ")?;
    }
    write!(out, "{} {}", s.kind, s.name)?;
    if !s.type_params.is_empty() {
        write!(out, "<{}>", s.type_params.join(", "))?;
    }
    write!(out, " [{}]", s.selector)?;
    if let Some(ty) = &s.type_display {
        write!(out, ": {ty}")?;
    }
    if let Some(n) = s.member_count {
        write!(out, " ({n} members)")?;
    }
    writeln!(out)
}
fn radix_source_order(items: &mut Vec<(usize, &Symbol)>) -> usize {
    let mut visits = 0;
    let mut buffer = items.clone();
    for byte in 0..std::mem::size_of::<usize>() {
        let mut counts = [0; 256];
        let key = |s: &Symbol| (s.location.as_ref().unwrap().start >> (byte * 8)) & 255;
        for &(_, s) in items.iter() {
            visits += 1;
            counts[key(s)] += 1;
        }
        let mut start = 0;
        for count in &mut counts {
            let n = *count;
            *count = start;
            start += n;
        }
        for &item in items.iter() {
            visits += 1;
            let k = key(item.1);
            buffer[counts[k]] = item;
            counts[k] += 1;
        }
        std::mem::swap(items, &mut buffer);
    }
    visits
}
struct BoundedWriter {
    output: Vec<u8>,
    chars: usize,
    limit: usize,
}
impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Count UTF-8 leading bytes; serde may split writes at arbitrary boundaries.
        let chars = bytes.iter().filter(|&&b| b & 0xc0 != 0x80).count();
        if self.output.len().saturating_add(bytes.len()) > HARD_LIMIT
            || self.chars.saturating_add(chars) > self.limit
        {
            return Err(io::Error::other("overview budget exceeded"));
        }
        self.output.extend_from_slice(bytes);
        self.chars += chars;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{Location, SemanticFacts, Snapshot};
    use crate::parser::ast::Type;
    fn session(n: usize, file_count: usize) -> DirectSession {
        let root = std::env::temp_dir();
        let mut snapshot = Snapshot {
            edit_context: None,
            version: 1,
            compiler: crate::ai::storage::compiler_stamp(),
            compatibility: String::new(),
            workspace: root.to_string_lossy().into(),
            revision: String::new(),
            sources: BTreeMap::new(),
            functions: vec![],
            semantic: SemanticFacts::default(),
        };
        for i in 0..file_count {
            snapshot.sources.insert(
                root.join(format!("overview-{i}.wi"))
                    .to_string_lossy()
                    .into(),
                String::new(),
            );
        }
        for i in (0..n).rev() {
            snapshot.semantic.symbols.push(Symbol {
                identity: None,
                id: format!("symbol:{i}"),
                name: format!("f{i}"),
                source_name: Some(format!("f{i}")),
                details: Default::default(),
                kind: "function".into(),
                location: Some(Location {
                    path: root
                        .join(format!("overview-{}.wi", i % file_count))
                        .to_string_lossy()
                        .into(),
                    start: i * 10,
                    end: i * 10 + 1,
                }),
                ty: Some(Type::Fn(vec![], Box::new(Type::Void))),
            });
        }
        snapshot.revision = snapshot.digest().unwrap();
        DirectSession::new(snapshot).unwrap()
    }
    #[test]
    fn projection_scales_linearly_across_files_and_declarations() {
        for n in [100, 1000, 10000] {
            for files in [1, 10, 100] {
                let session = session(n, files);
                let target = if files == 1 {
                    std::env::temp_dir().join("overview-0.wi")
                } else {
                    std::env::temp_dir()
                };
                let result = Overview::project(&session, &target, 1, 12000, None, false);
                assert_eq!(result.counts.top_level_symbols, n);
                assert_eq!(
                    result.symbol_visits,
                    n * (4 + 2 * std::mem::size_of::<usize>())
                );
                assert_eq!(result.counts.files, files);
                for file in &result.files {
                    let indices: Vec<usize> = file
                        .symbols
                        .iter()
                        .map(|s| s.name[1..].parse().unwrap())
                        .collect();
                    assert!(indices.windows(2).all(|w| w[0] < w[1]));
                }
                let visits = result.symbol_visits;
                let text = result.render("json", false).unwrap();
                assert!(text.chars().count() <= 12000);
                println!(
                    "symbols={n} files={files} projection_visits={visits} output_chars={}",
                    text.chars().count()
                );
                assert_eq!(session.session.queries, 0);
            }
        }
    }
    #[test]
    fn owner_fanout_and_many_owners_keep_linear_visits() {
        for n in [100, 1000, 10000] {
            for fanout in [10, n] {
                let mut s = session(n + n / fanout, 1);
                let mut ordinal = 0;
                for owner in 0..n / fanout {
                    for child in 0..=fanout {
                        let symbol = &mut s.session.snapshot.semantic.symbols[ordinal];
                        symbol.kind = if child == 0 { "class" } else { "field" }.into();
                        symbol.name = if child == 0 {
                            format!("Owner{owner}")
                        } else {
                            format!("field{child}")
                        };
                        symbol.source_name = Some(if child == 0 {
                            symbol.name.clone()
                        } else {
                            format!("Owner{owner}::{}", symbol.name)
                        });
                        symbol.location.as_mut().unwrap().start = ordinal * 10;
                        symbol.location.as_mut().unwrap().end = ordinal * 10 + 1;
                        ordinal += 1;
                    }
                }
                let result = Overview::project(
                    &s,
                    &std::env::temp_dir().join("overview-0.wi"),
                    1,
                    12000,
                    None,
                    false,
                );
                assert_eq!(result.counts.members, n);
                assert_eq!(result.counts.top_level_symbols, n / fanout);
                assert_eq!(
                    result.symbol_visits,
                    ordinal * (4 + 2 * std::mem::size_of::<usize>())
                );
                assert!(
                    result.files[0]
                        .symbols
                        .iter()
                        .all(|s| s.members.len() == fanout)
                );
            }
        }
    }
    #[test]
    fn bounded_writer_counts_unicode_and_includes_final_newline() {
        let mut writer = BoundedWriter {
            output: vec![],
            chars: 0,
            limit: 3,
        };
        writer.write_all("日本".as_bytes()).unwrap();
        writer.write_all(b"\n").unwrap();
        assert!(writer.write_all(b"x").is_err());
        assert_eq!(writer.output.len(), 7);
    }
    #[test]
    fn all_obeys_hard_limit_and_keeps_complete_json() {
        let mut s = session(10000, 1);
        for sym in &mut s.session.snapshot.semantic.symbols {
            sym.name.push_str(&"long".repeat(100));
        }
        for format in ["json", "ndjson", "human"] {
            let result = Overview::project(
                &s,
                &std::env::temp_dir().join("overview-0.wi"),
                1,
                1,
                None,
                false,
            );
            let text = result.render(format, true).unwrap();
            assert!(text.len() <= HARD_LIMIT);
            if format != "human" {
                let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(v["truncated"], true);
                assert!(v["effective_depth"].is_null());
            } else {
                assert!(text.contains("truncated=true"));
            }
        }
    }
    #[test]
    fn absent_types_duplicates_and_non_source_symbols() {
        let mut s = session(1, 1);
        s.session.snapshot.semantic.symbols[0].ty = None;
        let mut duplicate = s.session.snapshot.semantic.symbols[0].clone();
        duplicate.id = "duplicate".into();
        s.session.snapshot.semantic.symbols.push(duplicate);
        let mut builtin = s.session.snapshot.semantic.symbols[0].clone();
        builtin.location = None;
        s.session.snapshot.semantic.symbols.push(builtin);
        let result = Overview::project(
            &s,
            &std::env::temp_dir().join("overview-0.wi"),
            1,
            12000,
            None,
            false,
        );
        assert_eq!(result.counts.top_level_symbols, 1);
        assert!(result.files[0].symbols[0].type_display.is_none());
    }
    #[test]
    fn semantic_container_types_use_existing_display() {
        let mut s = session(1, 1);
        for ty in [
            Type::Array(Box::new(Type::I64)),
            Type::Generic("Option".into(), vec![Type::String]),
            Type::Generic("Result".into(), vec![Type::I64, Type::String]),
            Type::Generic("Map".into(), vec![Type::String, Type::I64]),
            Type::Generic("Channel".into(), vec![Type::I64]),
        ] {
            s.session.snapshot.semantic.symbols[0].ty = Some(ty);
            let result = Overview::project(
                &s,
                &std::env::temp_dir().join("overview-0.wi"),
                1,
                12000,
                None,
                false,
            );
            let display = result.files[0].symbols[0].type_display.as_ref().unwrap();
            assert!(display.contains('<'));
            assert!(!display.contains('$'));
        }
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;
    #[test]
    fn unicode_and_windows_source_metadata_survives_projection() {
        for path in [r"C:\project\日本語.wi", r"\\server\share\日本語.wi"] {
            let mut snapshot = crate::ai::Snapshot {
                edit_context: None,
                version: 1,
                compiler: crate::ai::storage::compiler_stamp(),
                compatibility: String::new(),
                workspace: String::new(),
                revision: String::new(),
                sources: BTreeMap::from([(path.into(), String::new())]),
                functions: vec![],
                semantic: crate::ai::SemanticFacts::default(),
            };
            snapshot.semantic.symbols.push(Symbol {
                identity: None,
                id: "unicode".into(),
                name: "計算".into(),
                source_name: Some("計算".into()),
                details: Default::default(),
                kind: "function".into(),
                location: Some(crate::ai::Location {
                    path: path.into(),
                    start: 0,
                    end: 6,
                }),
                ty: None,
            });
            snapshot.revision = snapshot.digest().unwrap();
            let session = DirectSession::new(snapshot).unwrap();
            let overview = Overview::project(&session, Path::new(path), 1, 12000, None, false);
            assert_eq!(overview.files[0].symbols[0].name, "計算");
            assert_eq!(overview.files[0].path, path.replace('\\', "/"));
            let text = overview.render("json", false).unwrap();
            assert!(text.contains("計算"));
        }
    }
}
