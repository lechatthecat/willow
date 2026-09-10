use std::collections::{BTreeMap, BTreeSet};
use syn::visit_mut::{self, VisitMut};
struct Calls(BTreeSet<String>);
impl VisitMut for Calls {
    fn visit_expr_call_mut(&mut self, node: &mut syn::ExprCall) {
        if let syn::Expr::Path(p) = &*node.func
            && let Some(s) = p.path.segments.last()
        {
            self.0.insert(s.ident.to_string());
        }
        visit_mut::visit_expr_call_mut(self, node);
    }
}
fn main() {
    for path in std::env::args().skip(1) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(file) = syn::parse_file(&text) else {
            continue;
        };
        let mut graph = BTreeMap::new();
        for item in file.items {
            if let syn::Item::Fn(mut f) = item {
                let mut calls = Calls(BTreeSet::new());
                calls.visit_block_mut(&mut f.block);
                graph.insert(f.sig.ident.to_string(), calls.0);
            }
        }
        for name in graph.keys() {
            let mut work: Vec<_> = graph[name].iter().cloned().collect();
            let mut seen = BTreeSet::new();
            while let Some(n) = work.pop() {
                if n == *name {
                    println!("{path}: {name}");
                    break;
                }
                if seen.insert(n.clone())
                    && let Some(edges) = graph.get(&n)
                {
                    work.extend(edges.iter().cloned());
                }
            }
        }
    }
}
