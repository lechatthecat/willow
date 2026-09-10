use std::collections::{BTreeMap, BTreeSet};
use syn::visit_mut::{self, VisitMut};
struct Calls(BTreeSet<String>);
impl VisitMut for Calls {
    fn visit_expr_method_call_mut(&mut self, node: &mut syn::ExprMethodCall) {
        if matches!(&*node.receiver, syn::Expr::Path(p) if p.path.is_ident("self") || p.path.is_ident("this") || p.path.is_ident("checker"))
        {
            self.0.insert(node.method.to_string());
        }
        visit_mut::visit_expr_method_call_mut(self, node);
    }
}
fn main() {
    let mut graph = BTreeMap::new();
    for path in std::env::args().skip(1) {
        let file = syn::parse_file(&std::fs::read_to_string(path).unwrap()).unwrap();
        for item in file.items {
            let syn::Item::Impl(implementation) = item else {
                continue;
            };
            if !matches!(&*implementation.self_ty, syn::Type::Path(p) if p.path.segments.last().unwrap().ident == "TypeChecker")
            {
                continue;
            }
            for item in implementation.items {
                let syn::ImplItem::Fn(mut method) = item else {
                    continue;
                };
                let mut calls = Calls(BTreeSet::new());
                calls.visit_block_mut(&mut method.block);
                graph.insert(method.sig.ident.to_string(), calls.0);
            }
        }
    }
    let mut recursive = BTreeSet::new();
    for name in graph.keys() {
        let mut todo: Vec<_> = graph[name].iter().cloned().collect();
        let mut seen = BTreeSet::new();
        while let Some(call) = todo.pop() {
            if call == *name {
                recursive.insert(name.clone());
                break;
            }
            if seen.insert(call.clone())
                && let Some(calls) = graph.get(&call)
            {
                todo.extend(calls.iter().cloned());
            }
        }
    }
    loop {
        let before = recursive.len();
        for (name, calls) in &graph {
            if calls.iter().any(|call| recursive.contains(call)) {
                recursive.insert(name.clone());
            }
        }
        if recursive.len() == before {
            break;
        }
    }
    println!("{}", recursive.into_iter().collect::<Vec<_>>().join(", "));
}
