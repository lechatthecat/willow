//! `i64 ** i64` lowering (willow-n5yv.3).
//!
//! Integer exponentiation is a compiler primitive, not a runtime call: a
//! non-negative *literal* exponent unrolls into a multiplication chain and
//! everything else becomes a bounded exponentiation-by-squaring loop. Every
//! multiplication is a Cranelift `imul`, so overflow wraps modulo 2^64 exactly
//! like an ordinary Willow `*`.
//!
//! "Literal" is meant strictly. The unroll decision reads the Cranelift
//! definition of the already-emitted exponent and fires only when that
//! definition is an `iconst`, which today means an integer literal wrote it.
//! There is no constant-folding pass in this compiler, so a constant
//! *expression* such as `x ** (1 + 2)` is an `iadd` at this point and takes the
//! dynamic path — same result, one loop instead of two `imul`s. Widening the
//! unroll to folded expressions needs a folding pass first; when one lands,
//! this decision picks it up with no change here.
//!
//! A negative exponent has no integer result. Negative literals are rejected by
//! the type checker (E0204); every other negative enters the normal language
//! panic path through `willow_pow_negative_exponent`, so `recover` sees it like
//! any other runtime fault.

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{
    InstBuilder, InstructionData, Opcode, TrapCode, Value, ValueDef, types,
};
use cranelift_frontend::FunctionBuilder;
use cranelift_module::Module;

use super::FuncGen;
use crate::diagnostics::Span;

/// One step of the unrolled multiplication schedule for a literal exponent.
///
/// The schedule is plain binary exponentiation: walk the exponent's bits from
/// least to most significant, squaring a running accumulator between bits and
/// folding the accumulator into the result wherever a bit is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PowStep {
    /// Fold the squaring accumulator into the result: `result *= acc`. The
    /// FIRST `Mul` of a schedule initializes the result to `acc` instead, so it
    /// costs no instruction.
    Mul,
    /// Advance the squaring accumulator: `acc *= acc`.
    Square,
}

/// The multiplication schedule for `base ** exponent` with a known non-negative
/// `exponent`. An empty schedule means the result is the constant `1`
/// (`x ** 0`), which needs no multiplication at all.
pub(crate) fn pow_unroll_steps(mut exponent: u64) -> Vec<PowStep> {
    let mut steps = Vec::new();
    if exponent == 0 {
        return steps;
    }
    loop {
        if exponent & 1 == 1 {
            steps.push(PowStep::Mul);
        }
        exponent >>= 1;
        if exponent == 0 {
            break;
        }
        steps.push(PowStep::Square);
    }
    steps
}

/// How many `imul` instructions [`pow_unroll_steps`] emits. Every step is one
/// multiplication except the first `Mul`, which only names the accumulator.
/// The emitter does not need the count; the schedule's cost is what the tests
/// pin, since there is no CLIF dump to assert against.
#[cfg(test)]
pub(crate) fn pow_unroll_imul_count(steps: &[PowStep]) -> usize {
    steps.len().saturating_sub(1)
}

/// Is `value` an `iconst`, and if so what does it hold?
///
/// Reading the emitted definition rather than the source expression is what
/// lets both the AST emitter and the LIR walker share one unroll decision. It
/// recognizes exactly what some earlier stage already reduced to an `iconst` —
/// today only an integer literal, since nothing folds constant expressions, so
/// `x ** (1 + 2)` answers `None` here and takes the dynamic path.
pub(crate) fn const_i64_operand(builder: &FunctionBuilder, value: Value) -> Option<i64> {
    let dfg = &builder.func.dfg;
    let ValueDef::Result(inst, 0) = dfg.value_def(value) else {
        return None;
    };
    match dfg.insts[inst] {
        InstructionData::UnaryImm {
            opcode: Opcode::Iconst,
            imm,
        } => Some(imm.bits()),
        _ => None,
    }
}

impl FuncGen<'_, '_> {
    /// Lower `base ** exponent` for `i64` operands. Both operands are already
    /// evaluated, which is what keeps evaluation left-to-right and exactly
    /// once: `x ** 0` still runs (and can still panic inside) `x`.
    pub(super) fn emit_pow_i64(&mut self, base: Value, exponent: Value, span: Span) -> Value {
        match const_i64_operand(self.builder, exponent) {
            // A negative constant cannot produce an integer, and the checker
            // already rejected the literal form (E0204). A negative that still
            // reaches here takes the dynamic path and raises at runtime.
            Some(constant) if constant >= 0 => self.emit_pow_i64_const(base, constant as u64),
            _ => self.emit_pow_i64_dynamic(base, exponent, span),
        }
    }

    /// Literal exponent: no branch, no loop, no runtime call — just the `imul`
    /// chain from [`pow_unroll_steps`].
    fn emit_pow_i64_const(&mut self, base: Value, exponent: u64) -> Value {
        let steps = pow_unroll_steps(exponent);
        let mut accumulator = base;
        let mut result: Option<Value> = None;
        for step in steps {
            match step {
                PowStep::Mul => {
                    result = Some(match result {
                        None => accumulator,
                        Some(current) => self.builder.ins().imul(current, accumulator),
                    });
                }
                PowStep::Square => {
                    accumulator = self.builder.ins().imul(accumulator, accumulator);
                }
            }
        }
        // `x ** 0` is 1 for every base, including 0.
        result.unwrap_or_else(|| self.builder.ins().iconst(types::I64, 1))
    }

    /// Dynamic exponent: guard the sign, then exponentiate by squaring. The
    /// loop shifts the exponent right once per iteration, so it runs at most
    /// 63 times regardless of the value.
    fn emit_pow_i64_dynamic(&mut self, base: Value, exponent: Value, span: Span) -> Value {
        let negative_block = self.builder.create_block();
        let header_block = self.builder.create_block();
        let body_block = self.builder.create_block();
        let exit_block = self.builder.create_block();
        for block in [header_block, body_block] {
            self.builder.append_block_param(block, types::I64); // squaring accumulator
            self.builder.append_block_param(block, types::I64); // remaining exponent
            self.builder.append_block_param(block, types::I64); // result so far
        }
        self.builder.append_block_param(exit_block, types::I64);

        let one = self.builder.ins().iconst(types::I64, 1);
        let is_negative = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::SignedLessThan, exponent, 0);
        self.builder.ins().brif(
            is_negative,
            negative_block,
            &[],
            header_block,
            &[base.into(), exponent.into(), one.into()],
        );

        self.builder.switch_to_block(negative_block);
        self.builder.seal_block(negative_block);
        self.emit_pow_negative_exponent_panic(exponent, span);

        self.builder.switch_to_block(header_block);
        let carried = self.builder.block_params(header_block).to_vec();
        let done = self.builder.ins().icmp_imm_s(IntCC::Equal, carried[1], 0);
        self.builder.ins().brif(
            done,
            exit_block,
            &[carried[2].into()],
            body_block,
            &[carried[0].into(), carried[1].into(), carried[2].into()],
        );

        self.builder.switch_to_block(body_block);
        self.builder.seal_block(body_block);
        let live = self.builder.block_params(body_block).to_vec();
        let (accumulator, remaining, result) = (live[0], live[1], live[2]);
        let low_bit = self.builder.ins().band_imm_u(remaining, 1);
        let is_odd = self.builder.ins().icmp_imm_s(IntCC::Equal, low_bit, 1);
        let multiplied = self.builder.ins().imul(result, accumulator);
        let next_result = self.builder.ins().select(is_odd, multiplied, result);
        let next_accumulator = self.builder.ins().imul(accumulator, accumulator);
        let next_remaining = self.builder.ins().ushr_imm_u(remaining, 1);
        self.builder.ins().jump(
            header_block,
            &[
                next_accumulator.into(),
                next_remaining.into(),
                next_result.into(),
            ],
        );
        // The header's second predecessor only exists now.
        self.builder.seal_block(header_block);

        self.builder.switch_to_block(exit_block);
        self.builder.seal_block(exit_block);
        self.builder.block_params(exit_block)[0]
    }

    /// Raise the negative-exponent language fault and terminate the block. The
    /// runtime must not return here; if it does, the multiplication loop would
    /// run on a nonsensical exponent, so that is an ABI violation.
    fn emit_pow_negative_exponent_panic(&mut self, exponent: Value, span: Span) {
        let source_file = self.source_file.to_string();
        let file_ptr = self.emit_string_literal(&source_file);
        let line = self.builder.ins().iconst(types::I32, span.line as i64);
        let column = self.builder.ins().iconst(types::I32, span.col as i64);
        let panic_id = self.func_id("willow_pow_negative_exponent");
        let panic_ref = self
            .module
            .declare_func_in_func(panic_id, self.builder.func);
        let panic_depth = self.emit_pre_runtime_call_panic_depth("willow_pow_negative_exponent");
        self.builder
            .ins()
            .call(panic_ref, &[exponent, file_ptr, line, column]);
        self.emit_post_willow_call_panic_check(panic_depth);
        self.builder.ins().trap(TrapCode::unwrap_user(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a schedule the way [`FuncGen::emit_pow_i64_const`] emits it, with
    /// the same wrapping multiplication Cranelift's `imul` performs.
    fn evaluate(steps: &[PowStep], base: i64) -> i64 {
        let mut accumulator = base;
        let mut result: Option<i64> = None;
        for step in steps {
            match step {
                PowStep::Mul => {
                    result = Some(match result {
                        None => accumulator,
                        Some(current) => current.wrapping_mul(accumulator),
                    });
                }
                PowStep::Square => accumulator = accumulator.wrapping_mul(accumulator),
            }
        }
        result.unwrap_or(1)
    }

    /// Perspective 1: `x ** 0` needs no multiplication and no base at all.
    #[test]
    fn pow_plan_01_exponent_zero_is_the_empty_schedule() {
        assert!(pow_unroll_steps(0).is_empty());
        assert_eq!(pow_unroll_imul_count(&pow_unroll_steps(0)), 0);
        assert_eq!(evaluate(&pow_unroll_steps(0), 7), 1);
        assert_eq!(evaluate(&pow_unroll_steps(0), 0), 1);
    }

    /// Perspective 2: `x ** 1` is the base itself, not a multiplication.
    #[test]
    fn pow_plan_02_exponent_one_costs_nothing() {
        assert_eq!(pow_unroll_steps(1), vec![PowStep::Mul]);
        assert_eq!(pow_unroll_imul_count(&pow_unroll_steps(1)), 0);
        assert_eq!(evaluate(&pow_unroll_steps(1), -9), -9);
    }

    /// Perspective 3: `x ** 2` is one squaring — the shape a reader expects to
    /// see in the generated code.
    #[test]
    fn pow_plan_03_square_is_one_imul() {
        assert_eq!(pow_unroll_steps(2), vec![PowStep::Square, PowStep::Mul]);
        assert_eq!(pow_unroll_imul_count(&pow_unroll_steps(2)), 1);
    }

    /// Perspective 4: powers of two cost exactly `log2(n)` multiplications, so
    /// `x ** 4` is 2 and `x ** 8` is 3 — repeated squaring, never a chain of
    /// `n - 1` products.
    #[test]
    fn pow_plan_04_powers_of_two_cost_log2_imuls() {
        for (exponent, expected) in [(4u64, 2usize), (8, 3), (16, 4), (1024, 10)] {
            let steps = pow_unroll_steps(exponent);
            assert_eq!(
                pow_unroll_imul_count(&steps),
                expected,
                "exponent {exponent}: {steps:?}"
            );
            assert_eq!(steps.iter().filter(|s| **s == PowStep::Mul).count(), 1);
        }
    }

    /// Perspective 5: a non-power-of-two exponent folds one accumulator per set
    /// bit; `x ** 3` is `x * (x * x)`, two multiplications.
    #[test]
    fn pow_plan_05_non_power_of_two_folds_each_set_bit() {
        assert_eq!(
            pow_unroll_steps(3),
            vec![PowStep::Mul, PowStep::Square, PowStep::Mul]
        );
        assert_eq!(pow_unroll_imul_count(&pow_unroll_steps(3)), 2);
        assert_eq!(pow_unroll_imul_count(&pow_unroll_steps(10)), 4);
    }

    /// Perspective 6: the schedule computes the same value as Rust's
    /// `wrapping_pow` for a wide sweep of small bases and exponents.
    #[test]
    fn pow_plan_06_matches_wrapping_pow() {
        for base in -20i64..=20 {
            for exponent in 0u32..=12 {
                assert_eq!(
                    evaluate(&pow_unroll_steps(exponent as u64), base),
                    base.wrapping_pow(exponent),
                    "{base} ** {exponent}"
                );
            }
        }
    }

    /// Perspective 7: sign parity — a negative base is positive at even
    /// exponents and negative at odd ones.
    #[test]
    fn pow_plan_07_negative_base_parity() {
        assert_eq!(evaluate(&pow_unroll_steps(2), -3), 9);
        assert_eq!(evaluate(&pow_unroll_steps(3), -3), -27);
        assert_eq!(evaluate(&pow_unroll_steps(0), -3), 1);
    }

    /// Perspective 8: overflow wraps modulo 2^64, exactly like repeated `*`.
    #[test]
    fn pow_plan_08_overflow_wraps_like_multiplication() {
        let steps = pow_unroll_steps(21);
        assert_eq!(evaluate(&steps, 3), 3i64.wrapping_pow(21));
        let by_hand = (0..21).fold(1i64, |acc, _| acc.wrapping_mul(3));
        assert_eq!(evaluate(&steps, 3), by_hand);

        let big = pow_unroll_steps(64);
        assert_eq!(evaluate(&big, 2), 0, "2 ** 64 wraps to 0");
    }

    /// Perspective 9: bases 0 and 1 stay 0 and 1 for every positive exponent,
    /// and -1 alternates.
    #[test]
    fn pow_plan_09_degenerate_bases() {
        for exponent in 1u64..=8 {
            assert_eq!(evaluate(&pow_unroll_steps(exponent), 0), 0);
            assert_eq!(evaluate(&pow_unroll_steps(exponent), 1), 1);
            let expected = if exponent % 2 == 0 { 1 } else { -1 };
            assert_eq!(evaluate(&pow_unroll_steps(exponent), -1), expected);
        }
    }

    /// Perspective 10: the schedule is bit-width bounded — even the largest
    /// exponent an `i64` can hold stays under 2 * 63 multiplications, so
    /// unrolling can never blow up compile time.
    #[test]
    fn pow_plan_10_schedule_is_bit_width_bounded() {
        let steps = pow_unroll_steps(i64::MAX as u64);
        assert_eq!(steps.len(), 63 + 62, "63 set bits, 62 squarings");
        assert!(pow_unroll_imul_count(&steps) <= 2 * 63);
        for exponent in [1u64, 7, 255, 1 << 32, i64::MAX as u64] {
            assert!(pow_unroll_imul_count(&pow_unroll_steps(exponent)) <= 2 * 63);
        }
    }

    /// Perspective 11: every schedule starts by folding the low bit or by
    /// squaring, and contains exactly one `Mul` per set bit — the invariant the
    /// emitter relies on when it treats the first `Mul` as free.
    #[test]
    fn pow_plan_11_one_mul_per_set_bit() {
        for exponent in 1u64..=512 {
            let steps = pow_unroll_steps(exponent);
            assert_eq!(
                steps.iter().filter(|s| **s == PowStep::Mul).count(),
                exponent.count_ones() as usize,
                "exponent {exponent}"
            );
            assert_eq!(
                steps.iter().filter(|s| **s == PowStep::Square).count(),
                (63 - exponent.leading_zeros()) as usize,
                "exponent {exponent}"
            );
        }
    }

    /// Perspective 12: a schedule never ends with a wasted squaring — the last
    /// step is always the fold that produces the result.
    #[test]
    fn pow_plan_12_no_trailing_dead_squaring() {
        for exponent in 1u64..=512 {
            assert_eq!(
                pow_unroll_steps(exponent).last(),
                Some(&PowStep::Mul),
                "exponent {exponent}"
            );
        }
    }
}
