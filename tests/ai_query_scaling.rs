//! Reproducible retained-allocation and deterministic graph-count audit.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering};
use willow_compiler::{
    CompilerOptions, CompilerSession,
    ai::{QueryRequest, QuerySession},
    diagnostics::HumanEmitter,
};
struct MeasuredAllocator;
static LIVE: AtomicIsize = AtomicIsize::new(0);
unsafe impl GlobalAlloc for MeasuredAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(p, layout) }
    }
    unsafe fn realloc(&self, p: *mut u8, old: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(p, old, size) };
        if !next.is_null() {
            LIVE.fetch_add(size as isize - old.size() as isize, Ordering::Relaxed);
        }
        next
    }
}
#[global_allocator]
static ALLOCATOR: MeasuredAllocator = MeasuredAllocator;
#[test]
fn increasing_graphs_and_repeated_queries_retain_no_query_history() {
    let directory = std::env::temp_dir().join(format!("willow-query-audit-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    // Snapshot locations are canonical; temp_dir can traverse symlinks on macOS
    // and lacks the canonical verbatim prefix on Windows.
    let directory = std::fs::canonicalize(directory).unwrap();
    let path = directory.join("main.wi");
    for n in [16, 64, 256] {
        let mut source =
            String::from("fn main() { let mut n = 0; while n < 2 { n = n + 1; worker0(); } } ");
        for i in 0..n {
            source.push_str(&format!("fn worker{i}() -> i64 {{ "));
            if i + 1 < n {
                source.push_str(&format!("worker{}(); ", i + 1));
            }
            source.push_str("return 42; } ");
        }
        std::fs::write(&path, &source).unwrap();
        let snapshot =
            CompilerSession::new(path.to_str().unwrap(), "", &CompilerOptions::debug(), None)
                .analysis_with_emitter(&mut HumanEmitter)
                .unwrap();
        let risk = snapshot.risk(&snapshot).unwrap();
        let nodes: usize = snapshot.semantic.flows.iter().map(|f| f.nodes.len()).sum();
        let edges: usize = snapshot
            .semantic
            .flows
            .iter()
            .flat_map(|f| &f.nodes)
            .map(|n| n.successors.len())
            .sum();
        let visits = risk["edge_visits"].as_u64().unwrap() as usize;
        assert!(visits <= 6 * edges);
        assert!(risk["call_edge_visits"].as_u64().unwrap() <= n as u64);
        let effect_propagation_visits = risk["effect_edge_visits"].as_u64().unwrap();
        assert!(effect_propagation_visits <= n as u64);
        let revision = snapshot.revision.clone();
        let id = snapshot
            .functions
            .iter()
            .find(|f| f.name == "worker0")
            .unwrap()
            .id
            .clone();
        if n == 16 {
            let saved = directory.join("query-baseline.json");
            snapshot.save(&saved).unwrap();
            let mut from_disk =
                QuerySession::new(willow_compiler::ai::Snapshot::load(&saved).unwrap()).unwrap();
            let mut from_frontend = QuerySession::new(snapshot.clone()).unwrap();
            let request = || QueryRequest::SymbolInfo {
                revision: revision.clone(),
                function: id.clone(),
            };
            assert_eq!(from_disk.query(request()), from_frontend.query(request()));
        }
        let expression_count = snapshot.semantic.expressions.len();
        let snapshot_bytes = serde_json::to_vec(&snapshot).unwrap().len();
        let heap_before = LIVE.load(Ordering::Relaxed);
        let owned = snapshot.clone();
        let snapshot_heap_bytes = LIVE.load(Ordering::Relaxed) - heap_before;
        let before = LIVE.load(Ordering::Relaxed);
        let mut session = QuerySession::new(owned).unwrap();
        let index_bytes = LIVE.load(Ordering::Relaxed) - before;
        for queries in [1, 10, 1000] {
            let comparisons_before = session.position_comparisons;
            let effect_visits_before = session.effect_edge_visits;
            let retained_before = LIVE.load(Ordering::Relaxed);
            for _ in 0..queries {
                let result = session.query(QueryRequest::TypeAt {
                    revision: revision.clone(),
                    file: path.to_string_lossy().into_owned(),
                    byte: source.find("42").unwrap(),
                });
                assert_eq!(result["result"]["status"], "ok");
                drop(result);
                let assignment = session.query(QueryRequest::TypeAt {
                    revision: revision.clone(),
                    file: path.to_string_lossy().into_owned(),
                    byte: source.find("n = n").unwrap(),
                });
                assert_eq!(assignment["result"]["status"], "ok");
                assert_eq!(assignment["result"]["type"], serde_json::json!(["I64"]));
                drop(assignment);
                drop(session.query(QueryRequest::Effects {
                    revision: revision.clone(),
                    function: id.clone(),
                }));
            }
            let retained_delta = LIVE.load(Ordering::Relaxed) - retained_before;
            assert!(
                retained_delta.abs() < 4096,
                "unexpected query history: {retained_delta}"
            );
            let position_comparisons = session.position_comparisons - comparisons_before;
            let effect_edge_visits = session.effect_edge_visits - effect_visits_before;
            assert!(position_comparisons <= 2 * queries * (expression_count.ilog2() as usize + 3));
            assert_eq!(effect_edge_visits, queries * (n - 1));
            println!(
                "MEASURE {}",
                serde_json::json!({"functions":n+1,"expressions":expression_count,"cfg_nodes":nodes,"cfg_edges":edges,"edge_visits":visits,"effect_propagation_visits":effect_propagation_visits,"query_pairs":queries,"snapshot_json_bytes":snapshot_bytes,"snapshot_heap_bytes":snapshot_heap_bytes,"position_comparisons":position_comparisons,"effect_edge_visits":effect_edge_visits,"index_heap_delta":index_bytes,"retained_query_heap_delta":retained_delta})
            );
        }
    }
    std::fs::remove_dir_all(directory).unwrap();
}
