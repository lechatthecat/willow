//! willow-9tls.1: the very first syntax identity a process allocates must not
//! be `ExprId::placeholder()`.
//!
//! This lives in its own test binary on purpose: the range allocator behind
//! `ExprId::fresh` is process-global, so only the first allocation in a fresh
//! process can observe the original bug (the first range started at 0, the
//! placeholder's value). Keep this file to a single test so nothing else in
//! the binary allocates an identity first.
use willow_compiler::parser::ast::{ExprId, PatternId};

#[test]
fn first_identity_in_a_fresh_process_is_not_the_placeholder() {
    // `ExprId::placeholder()` is crate-private and prints as `0`; `Display`
    // is the one public window onto the raw identity.
    let first = ExprId::fresh();
    assert_ne!(
        first.to_string(),
        "0",
        "first fresh ExprId collided with ExprId::placeholder()"
    );

    // `PatternId` shares the allocator, so the next few identities of both
    // kinds are consecutive, unique and still never 0.
    let mut seen = std::collections::HashSet::new();
    assert!(seen.insert(first.to_string()));
    for _ in 0..16 {
        let expr = ExprId::fresh().to_string();
        assert_ne!(expr, "0");
        assert!(seen.insert(expr));
        // PatternId has no Display; it exists here only to keep drawing from
        // the shared allocator between ExprIds.
        let _pattern = PatternId::fresh();
    }
}
