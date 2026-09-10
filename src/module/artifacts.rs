//! Session-private backing for bodies. Global declaration shells retain source
//! coordinates and syntax identities, but no statements or initializer trees.
//! Desugaring can therefore compose default signatures without retaining their
//! bodies; a copied default shell refers to the same immutable body artifact.
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};

use crate::diagnostics::{FileId, Span};
use crate::parser::ast::{Block, Expr, ExprId, Item, Program};

#[derive(Clone, Copy)]
pub(crate) enum UnitKind {
    Ast,
    Checker,
    Declared,
    Lir,
}

#[derive(Debug, Default)]
struct UnitMetrics {
    live: [usize; 4],
    peak: [usize; 4],
}

pub(crate) struct LiveUnit<T> {
    value: T,
    _lease: UnitLease,
}
impl<T> std::ops::Deref for LiveUnit<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}
pub(crate) struct UnitLease {
    metrics: Rc<RefCell<UnitMetrics>>,
    kind: UnitKind,
}
impl Drop for UnitLease {
    fn drop(&mut self) {
        self.metrics.borrow_mut().live[self.kind as usize] -= 1;
    }
}

#[derive(Debug)]
pub(crate) struct UnitArtifacts {
    directory: PathBuf,
    metrics: Rc<RefCell<UnitMetrics>>,
    next: usize,
    blocks: HashMap<Span, usize>,
    initializers: HashMap<ExprId, usize>,
    sources: HashMap<FileId, usize>,
}

impl UnitArtifacts {
    pub(crate) fn new() -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let directory = std::env::temp_dir().join(format!(
                "willow-units-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&directory) {
                Ok(()) => {
                    return Ok(Self {
                        directory,
                        metrics: Rc::default(),
                        next: 0,
                        blocks: HashMap::new(),
                        initializers: HashMap::new(),
                        sources: HashMap::new(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("create compiler unit storage"),
            }
        }
    }

    pub(crate) fn live(&self, kind: UnitKind) -> UnitLease {
        let mut metrics = self.metrics.borrow_mut();
        let slot = kind as usize;
        metrics.live[slot] += 1;
        metrics.peak[slot] = metrics.peak[slot].max(metrics.live[slot]);
        UnitLease {
            metrics: Rc::clone(&self.metrics),
            kind,
        }
    }

    pub(crate) fn track<T>(&self, kind: UnitKind, value: T) -> LiveUnit<T> {
        LiveUnit {
            value,
            _lease: self.live(kind),
        }
    }

    #[cfg(test)]
    pub(crate) fn peaks(&self) -> [usize; 4] {
        self.metrics.borrow().peak
    }

    pub(crate) fn write<T: Serialize>(&mut self, value: &T) -> Result<usize> {
        let id = self.next;
        self.next += 1;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.directory.join(id.to_string()))?;
        let mut writer = BufWriter::new(file);
        serde_json::to_writer(&mut writer, value).context("write compiler unit artifact")?;
        writer.flush()?;
        Ok(id)
    }

    pub(crate) fn read<T: DeserializeOwned>(&self, id: usize) -> Result<T> {
        let file = File::open(self.directory.join(id.to_string()))
            .context("open compiler unit artifact")?;
        serde_json::from_reader(BufReader::new(file)).context("read compiler unit artifact")
    }

    pub(crate) fn snapshot_source(&mut self, id: FileId, source: &str) -> Result<()> {
        if !self.sources.contains_key(&id) {
            let artifact = self.write(&source)?;
            self.sources.insert(id, artifact);
        }
        Ok(())
    }

    pub(crate) fn source(&self, id: FileId) -> Result<String> {
        self.read(*self.sources.get(&id).context("missing source snapshot")?)
    }

    fn offload_block(&mut self, block: &mut Block) -> Result<()> {
        if !self.blocks.contains_key(&block.span) {
            let id = self.write(block)?;
            self.blocks.insert(block.span, id);
        }
        block.stmts = Vec::new();
        Ok(())
    }

    /// All body-bearing top-level slots, including default methods and static
    /// initializers. Nested syntax is serialized with its owning slot.
    pub(crate) fn offload(&mut self, program: &mut Program) -> Result<()> {
        let _live = self.live(UnitKind::Ast);
        for item in &mut program.items {
            match item {
                Item::Function(function) => self.offload_block(&mut function.body)?,
                Item::Class(class) => {
                    for method in &mut class.methods {
                        self.offload_block(&mut method.body)?;
                    }
                    for constructor in &mut class.constructors {
                        self.offload_block(&mut constructor.body)?;
                    }
                    for field in &mut class.fields {
                        if let Some(expr) = &mut field.initializer {
                            let id = expr.id();
                            if !self.initializers.contains_key(&id) {
                                let artifact = self.write(expr)?;
                                self.initializers.insert(id, artifact);
                            }
                            *expr = Expr::Bool(false, expr.span(), id);
                        }
                    }
                }
                Item::Interface(interface) => {
                    for method in &mut interface.methods {
                        if let Some(body) = &mut method.default_body {
                            self.offload_block(body)?;
                        }
                    }
                }
                Item::Enum(_) => {}
            }
        }
        Ok(())
    }

    pub(crate) fn hydrate(&self, summary: &Program) -> Result<LiveUnit<Program>> {
        let mut program = summary.clone();
        let hydrate_block = |block: &mut Block| -> Result<()> {
            *block = self.read(
                *self
                    .blocks
                    .get(&block.span)
                    .context("missing body artifact")?,
            )?;
            Ok(())
        };
        for item in &mut program.items {
            match item {
                Item::Function(function) => hydrate_block(&mut function.body)?,
                Item::Class(class) => {
                    for method in &mut class.methods {
                        hydrate_block(&mut method.body)?;
                    }
                    for constructor in &mut class.constructors {
                        hydrate_block(&mut constructor.body)?;
                    }
                    for field in &mut class.fields {
                        if let Some(expr) = &mut field.initializer {
                            *expr = self.read(
                                *self
                                    .initializers
                                    .get(&expr.id())
                                    .context("missing initializer artifact")?,
                            )?;
                        }
                    }
                }
                Item::Interface(interface) => {
                    for method in &mut interface.methods {
                        if let Some(body) = &mut method.default_body {
                            hydrate_block(body)?;
                        }
                    }
                }
                Item::Enum(_) => {}
            }
        }
        Ok(self.track(UnitKind::Ast, program))
    }
}

impl Drop for UnitArtifacts {
    fn drop(&mut self) {
        if std::env::var_os("WILLOW_UNIT_MEMORY_LOG").is_some_and(|value| value != "0") {
            let peak = self.metrics.borrow().peak;
            eprintln!(
                "[unit-memory] peak_ast={} peak_checker={} peak_declared={} peak_lir={}",
                peak[0], peak[1], peak[2], peak[3]
            );
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(source: &str, file: FileId) -> Program {
        let tokens = crate::lexer::Lexer::with_file_id(source, file)
            .tokenize()
            .unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}: {source}");
        program
    }

    #[test]
    fn body_artifact_roundtrips_twenty_four_syntax_perspectives() {
        // Explicit perspectives cover every owning top-level slot and the
        // expression/statement crossings relevant to checker-table identity.
        let cases = [
            ("empty function", "fn f() {}"),
            ("scalar return", "fn f() -> i64 { return 4; }"),
            ("binary and unary", "fn f() { let x = -(1 + 2) * 3; }"),
            ("local mutation", "fn f() { let mut x = 1; x = 2; }"),
            (
                "if else",
                "fn f() { if true { println(1); } else { println(2); } }",
            ),
            (
                "while break continue",
                "fn f() { while true { if false { break; } continue; } }",
            ),
            ("for range", "fn f() { for x in 0..3 { println(x); } }"),
            ("defer call", "fn f() { defer println(1); }"),
            ("defer block", "fn f() { defer { println(1); } }"),
            (
                "array literal index",
                "fn f() { let a = [1, 2]; println(a[0]); }",
            ),
            (
                "array assignment",
                "fn f() { let mut a = [1, 2]; a[0] = 3; }",
            ),
            ("lambda expression", "fn f() { let x = |n: i64| n + 1; }"),
            (
                "lambda block",
                "fn f() { let x = |n: i64| { return n + 1; }; }",
            ),
            (
                "nested lambda",
                "fn f() { let x = |n: i64| { let y = |m: i64| m + n; return y(2); }; }",
            ),
            (
                "method body",
                "class C { pub fn value(self) -> i64 { return 4; } }",
            ),
            (
                "constructor body",
                "class C { x: i64; pub init(self, x: i64) { self.x = x; } }",
            ),
            (
                "static initializer",
                "class C { pub static x: i64 = 1 + 2; }",
            ),
            (
                "static callable initializer",
                "class C { pub static x: i64 = add(1, 2); }",
            ),
            (
                "static method",
                "class C { pub static fn x() -> i64 { return 3; } }",
            ),
            (
                "interface default",
                "interface I { fn x(self) -> i64 { return 3; } }",
            ),
            ("required interface", "interface I { fn x(self) -> i64; }"),
            ("async await", "async fn f() { await run(); }"),
            (
                "object and receiver call",
                "fn f() { let c = new C(3); println(c.value()); }",
            ),
            (
                "match expression",
                "fn f() { let x = match true { true => 1, false => 2 }; }",
            ),
        ];
        for (name, source) in cases {
            let mut program = parse(source, FileId(9));
            let expected = serde_json::to_vec(&program).unwrap();
            let mut artifacts = UnitArtifacts::new().unwrap();
            artifacts.offload(&mut program).unwrap();
            let restored = artifacts.hydrate(&program).unwrap();
            assert_eq!(serde_json::to_vec(&*restored).unwrap(), expected, "{name}");
        }
    }

    #[test]
    fn source_snapshots_ignore_later_input_changes() {
        let mut artifacts = UnitArtifacts::new().unwrap();
        artifacts.snapshot_source(FileId(2), "original").unwrap();
        artifacts.snapshot_source(FileId(2), "changed").unwrap();
        assert_eq!(artifacts.source(FileId(2)).unwrap(), "original");
    }

    #[test]
    fn artifact_directory_is_removed_on_success_and_error() {
        let directory;
        {
            let mut artifacts = UnitArtifacts::new().unwrap();
            directory = artifacts.directory.clone();
            artifacts.write(&"payload").unwrap();
            assert!(artifacts.read::<String>(999).is_err());
            assert!(directory.exists());
        }
        assert!(!directory.exists());
    }

    #[test]
    fn current_ast_peak_is_independent_of_module_count() {
        for count in [1, 16, 128] {
            let mut artifacts = UnitArtifacts::new().unwrap();
            let mut summaries = Vec::new();
            for index in 0..count {
                let mut program = parse("fn body() -> i64 { return 40 + 2; }", FileId(index));
                artifacts.offload(&mut program).unwrap();
                summaries.push(program);
            }
            for summary in &summaries {
                let restored = artifacts.hydrate(summary).unwrap();
                assert!(!restored.items.is_empty());
            }
            assert_eq!(artifacts.peaks(), [1, 0, 0, 0]);
        }
    }

    #[test]
    fn spooled_resolver_handles_a_thousand_imports_on_one_mib_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let mut artifacts = UnitArtifacts::new().unwrap();
                let root = artifacts.directory.clone();
                for index in 0..1024 {
                    let source = if index == 1023 {
                        "fn body() {}".to_string()
                    } else {
                        format!("import unit{}; fn body() {{}}", index + 1)
                    };
                    std::fs::write(root.join(format!("unit{index}.wi")), source).unwrap();
                }
                let mut entry = parse("import unit0; fn main() {}", FileId::ENTRY);
                artifacts.offload(&mut entry).unwrap();
                let resolution =
                    crate::module::resolver::resolve_imports_spooled(&entry, &root, artifacts);
                assert!(
                    resolution.diagnostics.is_empty(),
                    "{:?}",
                    resolution.diagnostics
                );
                assert_eq!(resolution.graph.files.len(), 1024);
                assert_eq!(
                    resolution.graph.artifacts.as_ref().unwrap().peaks(),
                    [1, 0, 0, 0]
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn default_injection_copies_body_reference_and_restores_original_identity() {
        let mut artifacts = UnitArtifacts::new().unwrap();
        let mut dependency = parse(
            "pub interface I { fn answer(self) -> i64 { return 42; } }",
            FileId(1),
        );
        let mut entry = parse(
            "import base; class C implements base::I {} fn main() {}",
            FileId::ENTRY,
        );
        artifacts.offload(&mut dependency).unwrap();
        artifacts.offload(&mut entry).unwrap();
        let mut modules = vec![crate::module::ResolvedModule {
            id: crate::module::ModuleId(0),
            name: "base".into(),
            canonical_path: "base".into(),
            path: PathBuf::from("base.wi"),
            source: String::new(),
            program: dependency,
        }];
        let result = crate::desugar::DesugarPass::run(&mut entry, &mut modules);
        assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
        let class = entry
            .items
            .iter()
            .find_map(|item| {
                if let Item::Class(c) = item {
                    Some(c)
                } else {
                    None
                }
            })
            .unwrap();
        assert!(class.methods[0].body.stmts.is_empty());
        let restored = artifacts.hydrate(&entry).unwrap();
        let class = restored
            .items
            .iter()
            .find_map(|item| {
                if let Item::Class(c) = item {
                    Some(c)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(class.methods[0].body.stmts.len(), 1);
        assert_eq!(class.methods[0].body.span.file_id, FileId(1));
    }
}
