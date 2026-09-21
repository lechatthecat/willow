//! End-to-end perspectives for native `i64 **` and `f64 **` (willow-n5yv).
//!
//! Stage 1 (willow-n5yv.2) gave `**` a lexer token, a right-associative parse
//! and a type rule, but every well-typed power was rejected by a codegen gate.
//! Stage 2 lowers `i64 ** i64` natively: a literal exponent unrolls into a
//! chain of `imul`s and everything else — a variable, or a constant expression
//! like `1 + 2`, which no pass folds yet — becomes a bounded square-and-multiply
//! loop, so no runtime `pow` is imported. A negative exponent has no integer
//! result: a negative *literal* is a compile error (E0204) and a negative
//! *value* raises a recoverable language panic.
//!
//! The instruction *schedule* is unit-tested in
//! `src/backend/cranelift/emit_pow.rs` (`pow_plan_01..12`); the type rules are
//! unit-tested in `src/semantic/type_checker/mod.rs` (`pow_type_01..25`). This
//! suite covers what only a running binary can show: emitted values, evaluation
//! order, panic behaviour, and agreement across both build profiles.
//!
//! Perspectives 1–26 cover the integer path; `pow_f64_27` onward cover the
//! numerical kernel, IEEE dispatch, compatibility spellings, release-build
//! parity, object linkage, async interaction, and versioned accuracy corpora.
//!
//! Integer perspectives:
//!   1  constant exponents 0..10 of a fixed base
//!   2  constant exponent equals repeated multiplication for many bases
//!   3  exponent 0 is 1 for every base, including 0 and negatives
//!   4  exponent 1 returns the base unchanged
//!   5  negative base parity (odd exponent negative, even exponent positive)
//!   6  overflow wraps modulo 2^64, exactly like `*`
//!   7  right associativity
//!   8  precedence over `*`, `/`, `%`, `+`, `-`
//!   9  binds tighter than prefix `-`
//!  10  a power is an ordinary operand in comparisons and `bool` contexts
//!  11  a dynamic exponent agrees with the constant form for every exponent
//!  12  a large dynamic exponent is bounded work, not `n` multiplications
//!  13  a negative dynamic exponent panics with the value and source location
//!  14  that panic is recoverable through `defer` + `recover()`
//!  15  a negative *literal* exponent is a compile error, `-0` is not
//!  16  native `f64` powers run, and mixed operands are a type error
//!  17  both operands are evaluated exactly once, left to right
//!  18  the base is evaluated even when the exponent folds the result away
//!  19  the LIR backend produces the same values as the AST backend
//!  20  a release build produces the same values as a debug build
//!  21  powers compose with recursion and loop accumulation
//!  22  GC-managed values stay live across a power under allocation stress
//!  23  `await` binds tighter than `**` inside an `async fn`
//!  24  extreme bases (i64::MIN / i64::MAX) wrap instead of trapping
//!  25  the runtime ABI declares the panic raiser, and integer powers stay exact
//!  26  the emitted object has no call relocation for an integer power
//!
//! Test bodies live in responsibility-specific modules. Leaf test names and the
//! `exponentiation::` filter are preserved; exact paths include the submodule.

#[path = "exponentiation/execution.rs"]
mod execution;
#[path = "exponentiation/float.rs"]
mod float;
#[path = "exponentiation/integer.rs"]
mod integer;
#[path = "exponentiation/linkage.rs"]
mod linkage;
