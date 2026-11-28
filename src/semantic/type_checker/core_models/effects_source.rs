//! Source-level E2604 differential. No production effect/dispatch/indirect
//! representation or solver is used by the oracle or generator.
use super::{check, next};
use crate::diagnostics::ErrorCode;
use std::fmt::Write;

mod projects;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Direct,
    Virtual,
    Interface,
    Indirect,
    Imported,
}

const LOCAL_KINDS: [Kind; 4] = [Kind::Direct, Kind::Virtual, Kind::Interface, Kind::Indirect];
const ALL_KINDS: [Kind; 5] = [
    Kind::Direct,
    Kind::Virtual,
    Kind::Interface,
    Kind::Indirect,
    Kind::Imported,
];

#[derive(Clone, Copy, Debug)]
struct Edge {
    kind: Kind,
    to: usize,
}

#[derive(Debug)]
struct Model {
    edges: Vec<Vec<Edge>>,
    waits: Vec<bool>,
    arity: Vec<usize>,
    modules: Vec<usize>,
    // An uncalled waiting lambda value, placed in module zero. It participates
    // in the same arity candidate pool as named values, even when unused.
    lambda: Option<usize>,
    async_virtual: bool,
}

impl Model {
    fn empty(n: usize) -> Self {
        Self {
            edges: vec![vec![]; n],
            waits: vec![false; n],
            arity: vec![1; n],
            modules: vec![0; n],
            lambda: None,
            async_virtual: false,
        }
    }

    // Each generated indirect call loads exactly the named target into its
    // local. The oracle follows that value, independently of compiler flow
    // constraints. Unused function values cannot introduce edges. Virtual's
    // pure base contributes no wait; its override forwards to `to`.
    fn reachable_wait(&self) -> (bool, usize) {
        let n = self.edges.len();
        let mut expanded = vec![Vec::new(); n + 2];
        let mut work = 0;
        for (from, row) in self.edges.iter().enumerate() {
            work += 1;
            for edge in row {
                work += 1;
                match edge.kind {
                    Kind::Indirect => expanded[from].push(edge.to),
                    Kind::Virtual if self.async_virtual => {}
                    Kind::Direct | Kind::Virtual | Kind::Interface | Kind::Imported => {
                        expanded[from].push(edge.to);
                    }
                }
            }
        }
        let mut seen = vec![false; n + 2];
        let mut pending = vec![0];
        seen[0] = true;
        while let Some(node) = pending.pop() {
            work += 1;
            if node < n && self.waits[node] {
                return (true, work);
            }
            for &to in &expanded[node] {
                work += 1;
                if !std::mem::replace(&mut seen[to], true) {
                    pending.push(to);
                }
            }
        }
        (false, work)
    }

    fn args(&self, to: usize) -> &'static str {
        if self.arity[to] == 1 { "ch" } else { "ch, 0" }
    }

    fn params(&self, to: usize) -> &'static str {
        if self.arity[to] == 1 {
            "ch: Channel<i64>"
        } else {
            "ch: Channel<i64>, extra: i64"
        }
    }

    fn call(&self, source: &mut String, from: usize, edge: Edge, serial: usize) {
        let to = edge.to;
        let prefix = if self.modules[from] == self.modules[to] {
            String::new()
        } else {
            format!("m{}::", self.modules[to])
        };
        let args = self.args(to);
        match edge.kind {
            Kind::Direct | Kind::Imported => writeln!(source, "{prefix}f{to}({args});"),
            Kind::Virtual => writeln!(
                source,
                "let o{serial}: {prefix}B{to} = new {prefix}D{to}(); o{serial}.run({args});"
            ),
            Kind::Interface => writeln!(
                source,
                "let r{serial}: {prefix}I{to} = new {prefix}C{to}(); r{serial}.run({args});"
            ),
            // Imported function values use item imports, not static property syntax.
            Kind::Indirect => writeln!(source, "let g{serial} = f{to}; g{serial}({args});"),
        }
        .unwrap();
    }

    // Bucket declarations once by module and collect imports once per edge;
    // do not rescan all functions for each module or wrappers for each call.
    fn sources(&self, label: &str, locked: bool) -> Vec<String> {
        let count = self.modules.iter().max().unwrap() + 1;
        assert!(count <= 3);
        let mut sources = vec![format!("// {label}\n"); count];
        let mut imports = vec![[false; 3]; count];
        let mut value_imports = vec![vec![false; self.edges.len()]; count];
        let mut virtuals = vec![false; self.edges.len()];
        let mut interfaces = virtuals.clone();
        for (from, row) in self.edges.iter().enumerate() {
            for edge in row {
                virtuals[edge.to] |= edge.kind == Kind::Virtual;
                interfaces[edge.to] |= edge.kind == Kind::Interface;
                if self.modules[from] != self.modules[edge.to] {
                    imports[self.modules[from]][self.modules[edge.to]] = true;
                    value_imports[self.modules[from]][edge.to] |= edge.kind == Kind::Indirect;
                }
            }
        }
        // Ensure disconnected modules (and their address-taken values) are
        // checked too. Module zero is the entry; imports always point upward.
        imports[0][1..count].fill(true);
        for (source, imports) in sources.iter_mut().zip(imports) {
            for (module, needed) in imports.into_iter().enumerate() {
                if needed {
                    writeln!(source, "import m{module};").unwrap();
                }
            }
        }
        for (source, imports) in sources.iter_mut().zip(value_imports) {
            for (to, needed) in imports.into_iter().enumerate() {
                if needed {
                    writeln!(source, "import m{}::f{to};", self.modules[to]).unwrap();
                }
            }
        }
        for (i, row) in self.edges.iter().enumerate() {
            let source = &mut sources[self.modules[i]];
            let params = self.params(i);
            let args = self.args(i);
            writeln!(source, "pub fn f{i}({params}) {{").unwrap();
            if self.waits[i] {
                source.push_str("ch.recv();\n");
            }
            for (serial, &edge) in row.iter().enumerate() {
                self.call(source, i, edge, serial);
            }
            source.push_str("}\n");
            if virtuals[i] {
                let async_ = if self.async_virtual { "async " } else { "" };
                writeln!(source,
                    "pub open class B{i} {{ pub open {async_}fn run(self, {params}) {{}} }}\n\
                     pub class D{i} extends B{i} {{ pub override {async_}fn run(self, {params}) {{ f{i}({args}); }} }}").unwrap();
            }
            if interfaces[i] {
                writeln!(source,
                    "pub interface I{i} extends Send {{ fn run(self, {params}); }}\n\
                     pub class C{i} implements I{i} {{ pub fn run(self, {params}) {{ f{i}({args}); }} }}").unwrap();
            }
        }
        if let Some(arity) = self.lambda {
            let extra = if arity == 1 { "" } else { ", extra: i64" };
            writeln!(
                sources[0],
                "fn make_value() {{ let unused = |ch: Channel<i64>{extra}| {{ ch.recv(); }}; }}"
            )
            .unwrap();
        }
        let (open, close) = if locked {
            ("lock m as x {", "}")
        } else {
            ("", "")
        };
        writeln!(sources[0], "async fn main() {{ let ch = Channel<i64>::new(); let m = Mutex::new(0); {open} f0({}); {close} }}", self.args(0)).unwrap();
        sources
    }

    fn compare(&self, label: &str, locked: bool) -> bool {
        let sources = self.sources(label, locked);
        assert_eq!(sources.len(), 1);
        let errors = check(&sources[0], &[ErrorCode::E2604]);
        let rejected = !errors.is_empty();
        assert_eq!(
            rejected,
            locked && self.reachable_wait().0,
            "{label}\n{self:?}\n{}",
            sources[0]
        );
        rejected
    }
}

#[derive(Default, Debug)]
struct Counts([usize; 2]);

impl Counts {
    fn add(&mut self, rejected: bool) {
        self.0[usize::from(rejected)] += 1;
    }

    fn assert(&self, total: usize, minimum: usize) {
        assert_eq!(self.0.iter().sum::<usize>(), total, "{self:?}");
        assert!(self.0.iter().all(|&n| n >= minimum), "{self:?}");
    }
}

#[test]
fn core_model_effects_source_exhaustive_kinds() {
    let mut total = 0;
    for kind in LOCAL_KINDS {
        let mut counts = Counts::default();
        for bits in 0..512 {
            for seeds in 0..8 {
                let mut model = Model::empty(3);
                for (from, row) in model.edges.iter_mut().enumerate() {
                    for to in 0..3 {
                        if bits & (1 << (from * 3 + to)) != 0 {
                            row.push(Edge { kind, to });
                        }
                    }
                    model.waits[from] = seeds & (1 << from) != 0;
                }
                let label = format!("kind={kind:?} graph={bits} seeds={seeds}");
                counts.add(model.compare(&label, true));
                total += 1;
            }
        }
        counts.assert(4096, 512);
        println!("exhaustive {kind:?}: {counts:?}");
    }
    assert_eq!(total, 16_384);
}

#[test]
fn core_model_effects_source_mixed_kinds() {
    let mut seed = 0x6479_6e61_6d69_6373;
    let mut counts: [Counts; 4] = std::array::from_fn(|_| Counts::default());
    let mut cases = 0;
    let mut sizes = [0; 3];
    let mut lambdas = [0; 3];
    for case in 0..256 {
        let replay = seed;
        let n = 4 + (next(&mut seed) % 3) as usize;
        sizes[n - 4] += 1;
        let mut model = Model::empty(n);
        for (from, row) in model.edges.iter_mut().enumerate() {
            model.arity[from] = 1 + (next(&mut seed) % 2) as usize;
            model.waits[from] = case % 4 != 0 && next(&mut seed).is_multiple_of(3);
            for to in 0..n {
                if next(&mut seed).is_multiple_of(3) {
                    row.push(Edge {
                        kind: LOCAL_KINDS[(next(&mut seed) % 4) as usize],
                        to,
                    });
                }
            }
        }
        // Each case exercises all four kinds, including an indirect call
        // reachable from the entry. Random edges add cycles and fan-out.
        for (i, kind) in LOCAL_KINDS.into_iter().enumerate() {
            model.edges[i].push(Edge {
                kind,
                to: (i + 1) % n,
            });
        }
        model.lambda = (case % 4 == 1).then(|| 1 + (next(&mut seed) % 2) as usize);
        lambdas[model.lambda.unwrap_or(0)] += 1;
        let label = format!("mixed kinds={LOCAL_KINDS:?} case={case} seed={replay:#x}");
        let rejected = model.compare(&label, true);
        for count in &mut counts {
            count.add(rejected);
        }
        assert!(!model.compare(&label, false));
        cases += 1;
    }
    assert_eq!(cases, 256);
    assert!(sizes.iter().all(|&n| n >= 32), "{sizes:?}");
    assert!(lambdas.iter().all(|&n| n >= 16), "{lambdas:?}");
    for (kind, count) in LOCAL_KINDS.into_iter().zip(counts) {
        count.assert(256, 64);
        println!("mixed {kind:?}: {count:?}");
    }
}

#[test]
fn core_model_effects_source_masks_and_arity() {
    let mut cases = 0;
    for kind in LOCAL_KINDS {
        for waiting in [false, true] {
            for asynchronous in [false, true] {
                let mut model = Model::empty(2);
                model.edges[0].push(Edge { kind, to: 1 });
                model.waits[1] = waiting;
                model.async_virtual = asynchronous;
                let label =
                    format!("mask kind={kind:?} waiting={waiting} async_virtual={asynchronous}");
                let expected = waiting && !(kind == Kind::Virtual && asynchronous);
                assert_eq!(model.compare(&label, true), expected);
                assert!(!model.compare(&label, false));
                cases += 1;
            }
        }
    }
    // f2 and the lambda are disconnected. Matching arity must not create a
    // path from the entry after function-value flow refinement.
    for value_arity in [1, 2] {
        for lambda in [None, Some(1), Some(2)] {
            let mut model = Model::empty(4);
            model.edges[0].push(Edge {
                kind: Kind::Indirect,
                to: 1,
            });
            model.edges[3].push(Edge {
                kind: Kind::Indirect,
                to: 2,
            });
            model.arity[2] = value_arity;
            model.waits[2] = true;
            model.lambda = lambda;
            let label = format!("arity value={value_arity} lambda={lambda:?}");
            assert!(!model.compare(&label, true));
            assert!(!model.compare(&label, false));
            cases += 1;
        }
    }
    assert_eq!(cases, 22);
}

#[test]
fn core_model_effects_source_linear_work() {
    // Count operations and emitted call sites, not elapsed time. Dense
    // indirect calls must share candidate hubs rather than expand N squared
    // candidates for each of N squared edges.
    for n in [8, 32, 128] {
        for shape in ["chain", "fan", "dense"] {
            for kind in [Kind::Indirect, Kind::Virtual, Kind::Interface] {
                let mut model = Model::empty(n);
                for (i, row) in model.edges.iter_mut().enumerate() {
                    match shape {
                        "chain" if i + 1 < n => row.push(Edge { kind, to: i + 1 }),
                        "fan" => row.push(Edge { kind, to: 0 }),
                        "dense" => row.extend((0..n).map(|to| Edge { kind, to })),
                        _ => {}
                    }
                }
                let edges: usize = model.edges.iter().map(Vec::len).sum();
                let (waits, work) = model.reachable_wait();
                assert!(!waits);
                assert!(work <= 4 * (n + edges + 2), "{shape} n={n} work={work}");
                let source = model.sources("scaling", true).remove(0);
                let (call, wrapper) = match kind {
                    Kind::Indirect => ("let g", None),
                    Kind::Virtual => ("let o", Some("pub open class B")),
                    Kind::Interface => ("let r", Some("pub interface I")),
                    _ => unreachable!(),
                };
                assert_eq!(source.matches(call).count(), edges);
                if let Some(wrapper) = wrapper {
                    let targets = match shape {
                        "chain" => n - 1,
                        "fan" => 1,
                        _ => n,
                    };
                    assert_eq!(source.matches(wrapper).count(), targets);
                }
                assert_eq!(source.matches("pub fn f").count(), n);
                println!(
                    "{kind:?} {shape} n={n} edges={edges} work={work} bytes={}",
                    source.len()
                );
            }
        }
    }
}
