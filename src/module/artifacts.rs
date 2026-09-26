//! Session-private backing for bodies. Global declaration shells retain source
//! coordinates and syntax identities, but no statements or initializer trees.
//! Desugaring can therefore compose default signatures without retaining their
//! bodies; a copied default shell refers to the same immutable body artifact.
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};

use crate::diagnostics::FileId;
use crate::parser::ast::{Block, BodyId, Expr, Item, Program};

#[cfg(test)]
#[path = "artifact_property_tests.rs"]
mod property_tests;

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
impl<T> LiveUnit<T> {
    pub(crate) fn into_inner(self) -> T {
        self.value
    }
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

pub(crate) type ParsedRecords = HashMap<FileId, (String, usize)>;

#[derive(Debug)]
pub(crate) struct UnitArtifacts {
    pub(crate) store: Rc<ArtifactStore>,
    metrics: Rc<RefCell<UnitMetrics>>,
    blocks: HashMap<BodyId, usize>,
    initializers: HashMap<crate::parser::ast::BodyId, usize>,
    sources: HashMap<FileId, usize>,
    pub(crate) bodies: Rc<crate::compiler_db::ids::BodyIndex>,
    pub(crate) revision_enabled: bool,
    pub(crate) parsed: ParsedRecords,
    pub(crate) previous_parsed: Option<(Rc<ArtifactStore>, ParsedRecords)>,
}

/// Shared session pack. Query handles can retain it independently of declaration
/// shells; the final owner closes the file and removes its private directory.
#[derive(Debug)]
pub(crate) struct ArtifactStore {
    directory: PathBuf,
    pack: RefCell<Option<ArtifactPack>>,
    entries: RefCell<Vec<(u64, u64)>>,
    read_windows: RefCell<ReadWindows>,
}

/// Read-ahead copies of flushed pack ranges, most recently used first. The
/// pack is append-only, so a filled range never goes stale; nearby small
/// records decode from memory without a seek/read pair each. A few windows let
/// a unit's records and a hot shared record (read once per unit) coexist.
#[derive(Debug, Default)]
struct ReadWindows {
    windows: Vec<ReadWindow>,
    /// Pack reads issued to fill windows, for deterministic I/O counts.
    #[cfg(test)]
    fills: usize,
}

#[derive(Debug, Default)]
struct ReadWindow {
    start: u64,
    bytes: Vec<u8>,
}

/// Small query records decode from memory; larger syntax artifacts stream.
const READ_WINDOW_BYTES: u64 = 64 * 1024;
/// Retained read scratch is at most `READ_WINDOWS * READ_WINDOW_BYTES`.
const READ_WINDOWS: usize = 4;
/// Keep appends buffered across a unit's many small declaration records.
const WRITE_BUFFER_BYTES: usize = 64 * 1024;

/// Appends and reads have independent file offsets. Keeping the writer alive
/// amortizes allocation and flushing across consecutive small query records.
#[derive(Debug)]
struct ArtifactPack {
    writer: BufWriter<File>,
    reader: File,
    written: u64,
}

/// Count bytes accepted by the buffered writer without flushing or seeking.
/// Failed serialization still advances the append offset for its partial data.
struct CountedWriter<'a> {
    writer: &'a mut BufWriter<File>,
    written: &'a mut u64,
}

impl Write for CountedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let count = self.writer.write(bytes)?;
        *self.written += count as u64;
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

impl ArtifactStore {
    pub(crate) fn write<T: Serialize>(&self, value: &T) -> Result<usize> {
        let mut borrowed = self.pack.borrow_mut();
        let pack = borrowed.as_mut().expect("open artifact pack");
        let start = pack.written;
        let mut writer = CountedWriter {
            writer: &mut pack.writer,
            written: &mut pack.written,
        };
        serde_json::to_writer(&mut writer, value).context("write compiler unit artifact")?;
        let end = pack.written;
        let mut entries = self.entries.borrow_mut();
        let id = entries.len();
        entries.push((start, end - start));
        Ok(id)
    }

    /// Bytes written to the pack so far, for deterministic size measurements.
    pub(crate) fn written(&self) -> u64 {
        self.pack.borrow().as_ref().map_or(0, |pack| pack.written)
    }

    pub(crate) fn read<T: DeserializeOwned>(&self, id: usize) -> Result<T> {
        let &(start, length) = self
            .entries
            .borrow()
            .get(id)
            .context("missing compiler unit artifact")?;
        let end = start + length;
        let mut borrowed = self.pack.borrow_mut();
        let pack = borrowed.as_mut().expect("open artifact pack");
        // Records still in the append buffer decode in place: no flush, no I/O.
        let flushed = pack.written - pack.writer.buffer().len() as u64;
        if start >= flushed {
            let buffered =
                &pack.writer.buffer()[(start - flushed) as usize..(end - flushed) as usize];
            return serde_json::from_slice(buffered).context("decode compiler unit artifact");
        }
        if end > flushed {
            pack.writer
                .flush()
                .context("flush compiler unit artifacts")?;
        }
        let reader = &mut pack.reader;
        if length > READ_WINDOW_BYTES {
            reader.seek(SeekFrom::Start(start))?;
            return serde_json::from_reader(BufReader::new(reader.take(length)))
                .context("read compiler unit artifact");
        }
        let mut cache = self.read_windows.borrow_mut();
        let hit = cache.windows.iter().position(|window| {
            window.start <= start && end <= window.start + window.bytes.len() as u64
        });
        let index = match hit {
            Some(index) => index,
            None => {
                // Fill the aligned block holding the record, so reads that walk
                // back to a unit's earlier records hit too; a record crossing a
                // block boundary starts its own window. Windows never cover
                // bytes still in the writer (flushing cannot change them anyway).
                let offset = start % READ_WINDOW_BYTES;
                let fill_start = if offset + length <= READ_WINDOW_BYTES {
                    start - offset
                } else {
                    start
                };
                let available = (pack.written - pack.writer.buffer().len() as u64) - fill_start;
                let fill = READ_WINDOW_BYTES.min(available) as usize;
                let mut window = if cache.windows.len() < READ_WINDOWS {
                    ReadWindow::default()
                } else {
                    cache.windows.pop().expect("full window cache")
                };
                window.bytes.clear();
                if window.bytes.capacity() < fill {
                    window.bytes.reserve_exact(fill);
                }
                window.bytes.resize(fill, 0);
                window.start = fill_start;
                reader.seek(SeekFrom::Start(fill_start))?;
                reader
                    .read_exact(&mut window.bytes)
                    .context("read compiler unit artifact")?;
                cache.windows.push(window);
                #[cfg(test)]
                {
                    cache.fills += 1;
                }
                cache.windows.len() - 1
            }
        };
        // Keep most recently used first; eviction pops the last.
        cache.windows[..=index].rotate_right(1);
        let window = &cache.windows[0];
        let offset = (start - window.start) as usize;
        serde_json::from_slice(&window.bytes[offset..offset + length as usize])
            .context("decode compiler unit artifact")
    }
}

impl Drop for ArtifactStore {
    fn drop(&mut self) {
        self.pack.get_mut().take();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

impl std::ops::Deref for UnitArtifacts {
    type Target = ArtifactStore;
    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

impl UnitArtifacts {
    pub(crate) fn body_index_mut(&mut self) -> &mut crate::compiler_db::ids::BodyIndex {
        Rc::get_mut(&mut self.bodies).expect("body inputs are frozen after CompilerDb creation")
    }

    pub(crate) fn new() -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let directory = std::env::temp_dir().join(format!(
                "willow-units-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            let builder = {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = builder;
                builder.mode(0o700);
                builder
            };
            match builder.create(&directory) {
                Ok(()) => {
                    // Own the directory before opening its pack, so an open
                    // failure follows the same cleanup path as every later error.
                    let mut store = ArtifactStore {
                        pack: RefCell::new(None),
                        directory,
                        entries: RefCell::default(),
                        read_windows: RefCell::default(),
                    };
                    let path = store.directory.join("artifacts.pack");
                    let writer = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                        .context("open compiler unit pack")?;
                    // try_clone shares a seek position on Unix; reopen instead.
                    let reader = File::open(&path).context("open compiler unit reader")?;
                    *store.pack.get_mut() = Some(ArtifactPack {
                        writer: BufWriter::with_capacity(WRITE_BUFFER_BYTES, writer),
                        reader,
                        written: 0,
                    });
                    let artifacts = Self {
                        store: Rc::new(store),
                        metrics: Rc::default(),
                        blocks: HashMap::new(),
                        initializers: HashMap::new(),
                        sources: HashMap::new(),
                        bodies: Default::default(),
                        revision_enabled: false,
                        parsed: HashMap::new(),
                        previous_parsed: None,
                    };
                    return Ok(artifacts);
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

    pub(crate) fn cached_parse(&self, file: FileId, source: &str) -> Result<Option<Program>> {
        use sha2::{Digest, Sha256};
        let Some((store, records)) = &self.previous_parsed else {
            return Ok(None);
        };
        let fingerprint = format!("{:x}", Sha256::digest(source.as_bytes()));
        records
            .get(&file)
            .filter(|(hash, _)| hash == &fingerprint)
            .map(|(_, record)| store.read(*record))
            .transpose()
    }

    pub(crate) fn retain_parse(
        &mut self,
        file: FileId,
        source: &str,
        program: &Program,
    ) -> Result<()> {
        use sha2::{Digest, Sha256};
        let fingerprint = format!("{:x}", Sha256::digest(source.as_bytes()));
        let record = self.write(program)?;
        self.parsed.insert(file, (fingerprint, record));
        Ok(())
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
        if !self.blocks.contains_key(&block.id) {
            let id = self.write(block)?;
            self.blocks.insert(block.id, id);
        }
        block.stmts = Vec::new();
        Ok(())
    }

    /// All body-bearing top-level slots, including default methods and static
    /// initializers. Nested syntax is serialized with its owning slot.
    pub(crate) fn offload(&mut self, program: &mut Program) -> Result<()> {
        let _live = self.live(UnitKind::Ast);
        self.body_index_mut().register_program(program);
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
                            let id = self
                                .bodies
                                .initializer_body(expr.id())
                                .expect("indexed initializer");
                            if !self.initializers.contains_key(&id) {
                                let artifact = self.write(expr)?;
                                self.initializers.insert(id, artifact);
                            }
                            *expr = Expr::Bool(false, expr.span(), expr.id());
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

    pub(crate) fn hydrate(&self, summary: &Program, file: FileId) -> Result<LiveUnit<Program>> {
        crate::query_stats::hydrate(file);
        let mut program = summary.clone();
        let hydrate_block = |block: &mut Block| -> Result<()> {
            let id = block.id;
            *block = self.read(
                *self
                    .blocks
                    .get(&self.bodies.source_body(id))
                    .context("missing body artifact")?,
            )?;
            block.id = id;
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
                                    .get(
                                        &self
                                            .bodies
                                            .initializer_body(expr.id())
                                            .context("missing static identity")?,
                                    )
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
        crate::query_stats::peaks(self.metrics.borrow().peak);
        if std::env::var_os("WILLOW_UNIT_MEMORY_LOG").is_some_and(|value| value != "0") {
            let peak = self.metrics.borrow().peak;
            eprintln!(
                "[unit-memory] peak_ast={} peak_checker={} peak_declared={} peak_lir={}",
                peak[0], peak[1], peak[2], peak[3]
            );
        }
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
            let restored = artifacts.hydrate(&program, FileId(9)).unwrap();
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
    fn shared_store_survives_shells_and_cleans_up_after_last_owner() {
        let artifacts = UnitArtifacts::new().unwrap();
        let directory = artifacts.directory.clone();
        let payload = artifacts.write(&"retained source").unwrap();
        let store = Rc::clone(&artifacts.store);
        drop(artifacts);
        assert!(directory.exists());
        assert_eq!(store.read::<String>(payload).unwrap(), "retained source");
        let later = store.write(&vec![1, 2, 3]).unwrap();
        assert_eq!(store.read::<Vec<u32>>(later).unwrap(), [1, 2, 3]);
        let final_owner = Rc::clone(&store);
        drop(store);
        assert!(directory.exists());
        assert_eq!(
            final_owner.read::<String>(payload).unwrap(),
            "retained source"
        );
        drop(final_owner);
        assert!(!directory.exists());
    }

    #[test]
    fn artifact_directory_is_removed_on_success_and_error() {
        let directory;
        {
            let artifacts = UnitArtifacts::new().unwrap();
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
            for (index, summary) in summaries.iter().enumerate() {
                let restored = artifacts.hydrate(summary, FileId(index as u32)).unwrap();
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
            package: crate::package::PackageId(0),
            symbol_module: None,
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
        let restored = artifacts.hydrate(&entry, FileId::ENTRY).unwrap();
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
