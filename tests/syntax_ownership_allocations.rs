//! Deterministic allocation evidence for syntax ownership.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use willow_compiler::diagnostics::Span;
use willow_compiler::parser::ast::{Expr, ExprId};

struct CountingAllocator;
thread_local! {
    static COUNT: Cell<Option<usize>> = const { Cell::new(None) };
}
fn record() {
    let _ = COUNT.try_with(|count| {
        if let Some(value) = count.get() {
            count.set(Some(value + 1));
        }
    });
}
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record();
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record();
        unsafe { System.alloc_zeroed(layout) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn measured<T>(f: impl FnOnce() -> T) -> (T, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            COUNT.with(|count| count.set(None));
        }
    }
    COUNT.with(|count| count.set(Some(0)));
    let _reset = Reset;
    let value = f();
    (value, COUNT.with(|count| count.get().unwrap()))
}

#[test]
fn syntax_ownership_allocation_scaling() {
    for size in [64usize, 256, 1024, 4096] {
        for shape in ["chain", "fanout"] {
            let leaf = || Expr::Integer(7, Span::dummy(), ExprId::fresh());
            let expr = if shape == "chain" {
                (0..size).fold(leaf(), |expr, _| {
                    Expr::Print(Box::new(expr), false, Span::dummy(), ExprId::fresh())
                })
            } else {
                Expr::ArrayLiteral(
                    (0..size).map(|_| leaf()).collect(),
                    Span::dummy(),
                    ExprId::fresh(),
                )
            };
            let (cloned, clone_allocations) = measured(|| expr.clone());
            let (_, drop_allocations) = measured(|| drop(cloned));
            // Chains need one output Box per edge; only shared traversal
            // buffers may allocate in addition. Fan-out needs an output Vec
            // and geometrically growing work/scratch buffers, not one per leaf.
            if shape == "chain" {
                assert!(clone_allocations <= size + 4, "{clone_allocations}");
                assert!(drop_allocations <= 2, "{drop_allocations}");
            } else {
                let logarithm = size.ilog2() as usize;
                assert!(
                    clone_allocations <= 2 * logarithm + 4,
                    "{clone_allocations}"
                );
                assert!(drop_allocations <= logarithm + 2, "{drop_allocations}");
            }
            println!(
                "{shape} size={size} clone_allocations={clone_allocations} drop_allocations={drop_allocations}"
            );
        }
    }
}

#[test]
fn deep_type_comparison_borrows_children_and_checks_function_results() {
    use willow_compiler::parser::ast::Type;
    for depth in [1, 8, 64, 512] {
        let wrap = |leaf: Type| (0..depth).fold(leaf, |ty, _| Type::Array(Box::new(ty)));
        let left = wrap(Type::I64);
        let right = wrap(Type::I64);
        let wrong = wrap(Type::Bool);
        assert_eq!(measured(|| left == right), (true, 0));
        assert_eq!(measured(|| left == wrong), (false, 0));

        let callable = |params: Vec<Type>, result| Type::Fn(params, Box::new(result));
        let f = callable(vec![Type::Bool, Type::I64], left);
        let same = callable(vec![Type::Bool, Type::I64], right.clone());
        let result = callable(vec![Type::Bool, Type::I64], wrong);
        let arity = callable(vec![Type::Bool], right.clone());
        let param = callable(vec![Type::Bool, Type::U64], right);
        let (equal, allocations) = measured(|| f == same);
        assert!(equal);
        assert!(
            allocations <= 1,
            "{allocations} allocations at depth {depth}"
        );
        assert_ne!(f, result);
        assert_ne!(f, arity);
        assert_ne!(f, param);
    }
}
