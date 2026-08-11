//! Native `f64 ** f64` lowering (willow-n5yv.4-.10).
//!
//! The generated numerical kernel is a Cranelift translation of the binary64
//! `pow` algorithm shipped by rust-lang/libm 0.2.16, which in turn preserves
//! the FreeBSD fdlibm `e_pow.c` algorithm and its original notice:
//!
//! Copyright (C) 2004 by Sun Microsystems, Inc. All rights reserved.
//! Permission to use, copy, modify, and distribute this software is freely
//! granted, provided that this notice is preserved.
//!
//! Willow factors the original algorithm into three private functions emitted
//! once per object: split `log2`, split-input `exp2`, and the IEEE dispatcher.
//! Constants are constructed from the exact binary64 bits below. V1 always
//! emits the specified non-FMA sequence, even on FMA-capable targets, so target
//! feature detection cannot silently change rounding.

use anyhow::Result;
use cranelift_codegen::ir::immediates::Ieee64;
use cranelift_codegen::ir::{
    AbiParam, InstBuilder, InstructionData, MemFlagsData, Opcode, UserFuncName, Value, ValueDef,
    condcodes::{FloatCC, IntCC},
    types,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{FuncId, Linkage, Module};

use super::{Codegen, FuncGen, ParamMode, Type};

pub(super) const LOG2_F64_SYMBOL: &str = "__willow_internal_log2_f64_v1";
pub(super) const EXP2_F64_SYMBOL: &str = "__willow_internal_exp2_f64_v1";
pub(super) const POW_F64_SYMBOL: &str = "__willow_internal_pow_f64_v1";

// Exact fdlibm binary64 constants. Decimal spellings are deliberately absent:
// the object must not depend on the host parser's decimal-to-binary rounding.
const TWO53: u64 = 0x4340_0000_0000_0000;
const BP1: u64 = 0x3ff8_0000_0000_0000;
const DP_H1: u64 = 0x3fe2_b803_4000_0000;
const DP_L1: u64 = 0x3e4c_fdeb_43cf_d006;
const L1: u64 = 0x3fe3_3333_3333_3303;
const L2: u64 = 0x3fdb_6db6_db6f_abff;
const L3: u64 = 0x3fd5_5555_518f_264d;
const L4: u64 = 0x3fd1_7460_a91d_4101;
const L5: u64 = 0x3fcd_864a_93c9_db65;
const L6: u64 = 0x3fca_7e28_4a45_4eef;
const P1: u64 = 0x3fc5_5555_5555_553e;
const P2: u64 = 0xbf66_c16c_16be_bd93;
const P3: u64 = 0x3f11_566a_af25_de2c;
const P4: u64 = 0xbebb_bd41_c5d2_6bf1;
const P5: u64 = 0x3e66_3769_72be_a4d0;
const LG2: u64 = 0x3fe6_2e42_fefa_39ef;
const LG2_H: u64 = 0x3fe6_2e43_0000_0000;
const LG2_L: u64 = 0xbe20_5c61_0ca8_6c39;
const OVT: u64 = 0x3c97_1547_652b_82fe;
const CP: u64 = 0x3fee_c709_dc3a_03fd;
const CP_H: u64 = 0x3fee_c709_e000_0000;
const CP_L: u64 = 0xbe3e_2fe0_145b_01f5;
const IVLN2: u64 = 0x3ff7_1547_652b_82fe;
const IVLN2_H: u64 = 0x3ff7_1547_6000_0000;
const IVLN2_L: u64 = 0x3e54_ae0b_f85d_df44;
const HUGE: u64 = 0x7e37_e43c_8800_759c; // 1e300
const TINY: u64 = 0x01a5_6e1f_c2f8_f359; // 1e-300
const CLOSE_TO_ONE: u64 = 0x3eb0_0000_0000_0000; // 2^-20
const TWO_NEG_969: u64 = 0x0360_0000_0000_0000;
const POS_INFINITY: u64 = 0x7ff0_0000_0000_0000;
const QUIET_NAN: u64 = 0x7ff8_0000_0000_0000;
const ABS_MASK: u64 = 0x7fff_ffff_ffff_ffff;

fn f64_bits(builder: &mut FunctionBuilder<'_>, bits: u64) -> Value {
    builder.ins().f64const(Ieee64::with_bits(bits))
}

fn as_i64_bits(builder: &mut FunctionBuilder<'_>, value: Value) -> Value {
    builder
        .ins()
        .bitcast(types::I64, MemFlagsData::new(), value)
}

fn from_i64_bits(builder: &mut FunctionBuilder<'_>, value: Value) -> Value {
    builder
        .ins()
        .bitcast(types::F64, MemFlagsData::new(), value)
}

fn clear_low_word(builder: &mut FunctionBuilder<'_>, value: Value) -> Value {
    let bits = as_i64_bits(builder, value);
    let mask = builder
        .ins()
        .iconst(types::I64, 0xffff_ffff_0000_0000_u64 as i64);
    let high = builder.ins().band(bits, mask);
    from_i64_bits(builder, high)
}

impl Codegen {
    /// Declare and define the three private numerical helpers exactly once for
    /// this object, then expose the pow helper under the legacy builtin names so
    /// direct calls and function-value capture share `**` semantics.
    pub(super) fn declare_native_pow_f64(&mut self) -> Result<()> {
        if self.func_ids.contains_key(POW_F64_SYMBOL) {
            return Ok(());
        }

        let mut log_sig = self.module.make_signature();
        log_sig.params.push(AbiParam::new(types::F64));
        log_sig.returns.push(AbiParam::new(types::F64));
        log_sig.returns.push(AbiParam::new(types::F64));
        let log_id = self
            .module
            .declare_function(LOG2_F64_SYMBOL, Linkage::Local, &log_sig)?;

        let mut exp_sig = self.module.make_signature();
        exp_sig.params.push(AbiParam::new(types::F64));
        exp_sig.params.push(AbiParam::new(types::F64));
        exp_sig.returns.push(AbiParam::new(types::F64));
        let exp_id = self
            .module
            .declare_function(EXP2_F64_SYMBOL, Linkage::Local, &exp_sig)?;

        let mut pow_sig = self.module.make_signature();
        pow_sig.params.push(AbiParam::new(types::F64));
        pow_sig.params.push(AbiParam::new(types::F64));
        pow_sig.returns.push(AbiParam::new(types::F64));
        let pow_id = self
            .module
            .declare_function(POW_F64_SYMBOL, Linkage::Local, &pow_sig)?;

        self.define_log2_f64(log_id, log_sig)?;
        self.define_exp2_f64(exp_id, exp_sig)?;
        self.define_pow_f64(pow_id, pow_sig, log_id, exp_id)?;

        let fn_ty = Type::Fn(vec![Type::F64, Type::F64], Box::new(Type::F64));
        for name in [POW_F64_SYMBOL, "pow", "powf"] {
            self.func_ids.insert(name, pow_id);
            self.func_return_types.insert(name, Type::F64);
            self.fn_types.insert(name, fn_ty.clone());
            self.func_param_modes
                .insert(name, vec![ParamMode::Value, ParamMode::Value]);
            self.func_param_debug.insert(name, Vec::new());
        }
        Ok(())
    }

    fn define_log2_f64(
        &mut self,
        func_id: FuncId,
        sig: cranelift_codegen::ir::Signature,
    ) -> Result<()> {
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());
        let mut fn_ctx = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            b.seal_block(entry);
            let x = b.block_params(entry)[0];

            let close = b.create_block();
            let classify = b.create_block();
            let delta = {
                let one = f64_bits(&mut b, 1.0f64.to_bits());
                b.ins().fsub(x, one)
            };
            let abs_delta = b.ins().fabs(delta);
            let close_limit = f64_bits(&mut b, CLOSE_TO_ONE);
            let is_close = b
                .ins()
                .fcmp(FloatCC::LessThanOrEqual, abs_delta, close_limit);
            b.ins().brif(is_close, close, &[], classify, &[]);

            // Near one, fdlibm's compensated log1p polynomial avoids losing the
            // small difference before multiplication by a large exponent.
            b.switch_to_block(close);
            b.seal_block(close);
            let half = f64_bits(&mut b, 0.5f64.to_bits());
            let third = f64_bits(&mut b, (1.0f64 / 3.0).to_bits());
            let quarter = f64_bits(&mut b, 0.25f64.to_bits());
            let tt = b.ins().fmul(delta, delta);
            let tq = b.ins().fmul(delta, quarter);
            let inner = b.ins().fsub(third, tq);
            let ti = b.ins().fmul(delta, inner);
            let half_minus = b.ins().fsub(half, ti);
            let w = b.ins().fmul(tt, half_minus);
            let ivln2_h = f64_bits(&mut b, IVLN2_H);
            let u = b.ins().fmul(ivln2_h, delta);
            let ivln2_l = f64_bits(&mut b, IVLN2_L);
            let dl = b.ins().fmul(delta, ivln2_l);
            let ivln2 = f64_bits(&mut b, IVLN2);
            let wi = b.ins().fmul(w, ivln2);
            let v = b.ins().fsub(dl, wi);
            let uv = b.ins().fadd(u, v);
            let t1 = clear_low_word(&mut b, uv);
            let t1_minus_u = b.ins().fsub(t1, u);
            let t2 = b.ins().fsub(v, t1_minus_u);
            b.ins().return_(&[t1, t2]);

            b.switch_to_block(classify);
            b.seal_block(classify);
            let bits = as_i64_bits(&mut b, x);
            let high = b.ins().ushr_imm_u(bits, 32);
            let high_mask = b.ins().iconst(types::I64, 0x7fff_ffff);
            let ix = b.ins().band(high, high_mask);
            let subnormal = b.ins().icmp_imm_u(IntCC::UnsignedLessThan, ix, 0x0010_0000);
            let scale_subnormal = b.create_block();
            let normalized = b.create_block();
            for ty in [types::F64, types::I64, types::I64] {
                b.append_block_param(normalized, ty);
            }
            let zero_n = b.ins().iconst(types::I64, 0);
            b.ins().brif(
                subnormal,
                scale_subnormal,
                &[],
                normalized,
                &[x.into(), zero_n.into(), ix.into()],
            );

            b.switch_to_block(scale_subnormal);
            b.seal_block(scale_subnormal);
            let two53 = f64_bits(&mut b, TWO53);
            let scaled = b.ins().fmul(x, two53);
            let scaled_bits = as_i64_bits(&mut b, scaled);
            let scaled_high = b.ins().ushr_imm_u(scaled_bits, 32);
            let scaled_ix = b.ins().band(scaled_high, high_mask);
            let minus53 = b.ins().iconst(types::I64, -53);
            b.ins().jump(
                normalized,
                &[scaled.into(), minus53.into(), scaled_ix.into()],
            );

            b.switch_to_block(normalized);
            b.seal_block(normalized);
            let vals = b.block_params(normalized).to_vec();
            let ax = vals[0];
            let n0 = vals[1];
            let ix0 = vals[2];
            let exponent = b.ins().sshr_imm_u(ix0, 20);
            let exponent = b.ins().iadd_imm_s(exponent, -0x3ff);
            let n = b.ins().iadd(n0, exponent);
            let mant_mask = b.ins().iconst(types::I64, 0x000f_ffff);
            let j = b.ins().band(ix0, mant_mask);
            let normalized_ix = b.ins().bor_imm_u(j, 0x3ff0_0000);
            let k0 = b.create_block();
            let k1_test = b.create_block();
            let k1 = b.create_block();
            let k_up = b.create_block();
            let kernel = b.create_block();
            for ty in [types::F64, types::I64, types::I64, types::I64] {
                b.append_block_param(kernel, ty);
            }
            let low_interval = b
                .ins()
                .icmp_imm_u(IntCC::UnsignedLessThanOrEqual, j, 0x3988e);
            b.ins().brif(low_interval, k0, &[], k1_test, &[]);

            b.switch_to_block(k0);
            b.seal_block(k0);
            let zero = b.ins().iconst(types::I64, 0);
            b.ins().jump(
                kernel,
                &[ax.into(), n.into(), normalized_ix.into(), zero.into()],
            );

            b.switch_to_block(k1_test);
            b.seal_block(k1_test);
            let middle = b.ins().icmp_imm_u(IntCC::UnsignedLessThan, j, 0xbb67a);
            b.ins().brif(middle, k1, &[], k_up, &[]);

            b.switch_to_block(k1);
            b.seal_block(k1);
            let one_i = b.ins().iconst(types::I64, 1);
            b.ins().jump(
                kernel,
                &[ax.into(), n.into(), normalized_ix.into(), one_i.into()],
            );

            b.switch_to_block(k_up);
            b.seal_block(k_up);
            let n_up = b.ins().iadd_imm_u(n, 1);
            let ix_down = b.ins().iadd_imm_s(normalized_ix, -0x0010_0000);
            let zero = b.ins().iconst(types::I64, 0);
            b.ins().jump(
                kernel,
                &[ax.into(), n_up.into(), ix_down.into(), zero.into()],
            );

            b.switch_to_block(kernel);
            b.seal_block(kernel);
            let args = b.block_params(kernel).to_vec();
            let (ax0, n, ix, k) = (args[0], args[1], args[2], args[3]);
            let k_is_one = b.ins().icmp_imm_u(IntCC::Equal, k, 1);
            let one_f = f64_bits(&mut b, 1.0f64.to_bits());
            let one_half = f64_bits(&mut b, BP1);
            let bp = b.ins().select(k_is_one, one_half, one_f);
            let dp_h1 = f64_bits(&mut b, DP_H1);
            let zero_f = f64_bits(&mut b, 0.0f64.to_bits());
            let dp_h = b.ins().select(k_is_one, dp_h1, zero_f);
            let dp_l1 = f64_bits(&mut b, DP_L1);
            let dp_l = b.ins().select(k_is_one, dp_l1, zero_f);

            let ax_bits = as_i64_bits(&mut b, ax0);
            let low_mask = b.ins().iconst(types::I64, 0xffff_ffff_u64 as i64);
            let low = b.ins().band(ax_bits, low_mask);
            let ix_shift = b.ins().ishl_imm_u(ix, 32);
            let rebuilt = b.ins().bor(ix_shift, low);
            let ax = from_i64_bits(&mut b, rebuilt);
            let u = b.ins().fsub(ax, bp);
            let sum = b.ins().fadd(ax, bp);
            let v = b.ins().fdiv(one_f, sum);
            let ss = b.ins().fmul(u, v);
            let s_h = clear_low_word(&mut b, ss);

            let half_ix = b.ins().ushr_imm_u(ix, 1);
            let base_high = b.ins().bor_imm_u(half_ix, 0x2000_0000);
            let plus = b.ins().iadd_imm_u(base_high, 0x0008_0000);
            let k_shift = b.ins().ishl_imm_u(k, 18);
            let th_high = b.ins().iadd(plus, k_shift);
            let th_bits = b.ins().ishl_imm_u(th_high, 32);
            let t_h0 = from_i64_bits(&mut b, th_bits);
            let th_minus_bp = b.ins().fsub(t_h0, bp);
            let t_l = b.ins().fsub(ax, th_minus_bp);
            let shth = b.ins().fmul(s_h, t_h0);
            let u_minus = b.ins().fsub(u, shth);
            let shtl = b.ins().fmul(s_h, t_l);
            let sl_inner = b.ins().fsub(u_minus, shtl);
            let s_l = b.ins().fmul(v, sl_inner);

            let s2 = b.ins().fmul(ss, ss);
            let l6 = f64_bits(&mut b, L6);
            let l5 = f64_bits(&mut b, L5);
            let p = b.ins().fmul(s2, l6);
            let p = b.ins().fadd(l5, p);
            let l4 = f64_bits(&mut b, L4);
            let p = b.ins().fmul(s2, p);
            let p = b.ins().fadd(l4, p);
            let l3 = f64_bits(&mut b, L3);
            let p = b.ins().fmul(s2, p);
            let p = b.ins().fadd(l3, p);
            let l2 = f64_bits(&mut b, L2);
            let p = b.ins().fmul(s2, p);
            let p = b.ins().fadd(l2, p);
            let l1 = f64_bits(&mut b, L1);
            let p = b.ins().fmul(s2, p);
            let p = b.ins().fadd(l1, p);
            let s4 = b.ins().fmul(s2, s2);
            let mut r = b.ins().fmul(s4, p);
            let sh_plus_s = b.ins().fadd(s_h, ss);
            let sl_term = b.ins().fmul(s_l, sh_plus_s);
            r = b.ins().fadd(r, sl_term);
            let sh2 = b.ins().fmul(s_h, s_h);
            let three = f64_bits(&mut b, 3.0f64.to_bits());
            let th_sum = b.ins().fadd(three, sh2);
            let th_sum = b.ins().fadd(th_sum, r);
            let t_h = clear_low_word(&mut b, th_sum);
            let th_minus_three = b.ins().fsub(t_h, three);
            let th_minus = b.ins().fsub(th_minus_three, sh2);
            let t_l = b.ins().fsub(r, th_minus);
            let u = b.ins().fmul(s_h, t_h);
            let slth = b.ins().fmul(s_l, t_h);
            let tlss = b.ins().fmul(t_l, ss);
            let v = b.ins().fadd(slth, tlss);
            let uv = b.ins().fadd(u, v);
            let p_h = clear_low_word(&mut b, uv);
            let ph_minus_u = b.ins().fsub(p_h, u);
            let p_l = b.ins().fsub(v, ph_minus_u);
            let cp_h = f64_bits(&mut b, CP_H);
            let z_h = b.ins().fmul(cp_h, p_h);
            let cp_l = f64_bits(&mut b, CP_L);
            let zl0 = b.ins().fmul(cp_l, p_h);
            let cp = f64_bits(&mut b, CP);
            let plcp = b.ins().fmul(p_l, cp);
            let z_l = b.ins().fadd(zl0, plcp);
            let z_l = b.ins().fadd(z_l, dp_l);
            let n_f = b.ins().fcvt_from_sint(types::F64, n);
            let sum = b.ins().fadd(z_h, z_l);
            let sum = b.ins().fadd(sum, dp_h);
            let sum = b.ins().fadd(sum, n_f);
            let out_h = clear_low_word(&mut b, sum);
            let a = b.ins().fsub(out_h, n_f);
            let a = b.ins().fsub(a, dp_h);
            let a = b.ins().fsub(a, z_h);
            let out_l = b.ins().fsub(z_l, a);
            b.ins().return_(&[out_h, out_l]);

            b.seal_all_blocks();
            b.finalize(self.module.target_config());
        }
        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    fn define_exp2_f64(
        &mut self,
        func_id: FuncId,
        sig: cranelift_codegen::ir::Signature,
    ) -> Result<()> {
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());
        let mut fn_ctx = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            b.seal_block(entry);
            let p_h = b.block_params(entry)[0];
            let p_l = b.block_params(entry)[1];
            let z0 = b.ins().fadd(p_h, p_l);

            let nan = b.create_block();
            let overflow = b.create_block();
            let overflow_tie = b.create_block();
            let underflow_test = b.create_block();
            let underflow = b.create_block();
            let underflow_tie = b.create_block();
            let kernel = b.create_block();

            let unordered = b.ins().fcmp(FloatCC::Unordered, z0, z0);
            let ordered = b.create_block();
            b.ins().brif(unordered, nan, &[], ordered, &[]);

            b.switch_to_block(nan);
            b.seal_block(nan);
            b.ins().return_(&[z0]);

            b.switch_to_block(ordered);
            b.seal_block(ordered);
            let upper = f64_bits(&mut b, 1024.0f64.to_bits());
            let too_high = b.ins().fcmp(FloatCC::GreaterThan, z0, upper);
            let upper_check = b.create_block();
            b.ins().brif(too_high, overflow, &[], upper_check, &[]);

            b.switch_to_block(upper_check);
            b.seal_block(upper_check);
            let at_upper = b.ins().fcmp(FloatCC::Equal, z0, upper);
            b.ins()
                .brif(at_upper, overflow_tie, &[], underflow_test, &[]);

            b.switch_to_block(overflow_tie);
            b.seal_block(overflow_tie);
            let ovt = f64_bits(&mut b, OVT);
            let left = b.ins().fadd(p_l, ovt);
            let right = b.ins().fsub(z0, p_h);
            let crosses = b.ins().fcmp(FloatCC::GreaterThan, left, right);
            b.ins().brif(crosses, overflow, &[], underflow_test, &[]);

            b.switch_to_block(overflow);
            b.seal_block(overflow);
            let huge = f64_bits(&mut b, HUGE);
            let inf = b.ins().fmul(huge, huge);
            b.ins().return_(&[inf]);

            b.switch_to_block(underflow_test);
            let lower = f64_bits(&mut b, (-1075.0f64).to_bits());
            let too_low = b.ins().fcmp(FloatCC::LessThan, z0, lower);
            let lower_check = b.create_block();
            b.ins().brif(too_low, underflow, &[], lower_check, &[]);

            b.switch_to_block(lower_check);
            b.seal_block(lower_check);
            let at_lower = b.ins().fcmp(FloatCC::Equal, z0, lower);
            b.ins().brif(at_lower, underflow_tie, &[], kernel, &[]);

            b.switch_to_block(underflow_tie);
            b.seal_block(underflow_tie);
            let left = p_l;
            let right = b.ins().fsub(z0, p_h);
            let crosses = b.ins().fcmp(FloatCC::LessThanOrEqual, left, right);
            b.ins().brif(crosses, underflow, &[], kernel, &[]);

            b.switch_to_block(underflow);
            b.seal_block(underflow);
            let tiny = f64_bits(&mut b, TINY);
            let zero = b.ins().fmul(tiny, tiny);
            b.ins().return_(&[zero]);

            b.switch_to_block(kernel);
            b.seal_block(kernel);
            // `nearest` is specified as round-to-nearest, ties-to-even. The
            // preceding finite [-1075, 1024] guards dominate the conversion.
            let n_f = b.ins().nearest(z0);
            let n = b.ins().fcvt_to_sint(types::I64, n_f);
            let p_h = b.ins().fsub(p_h, n_f);
            let sum = b.ins().fadd(p_l, p_h);
            let t = clear_low_word(&mut b, sum);
            let lg2_h = f64_bits(&mut b, LG2_H);
            let u = b.ins().fmul(t, lg2_h);
            let t_minus_ph = b.ins().fsub(t, p_h);
            let pl_minus = b.ins().fsub(p_l, t_minus_ph);
            let lg2 = f64_bits(&mut b, LG2);
            let v0 = b.ins().fmul(pl_minus, lg2);
            let lg2_l = f64_bits(&mut b, LG2_L);
            let tl = b.ins().fmul(t, lg2_l);
            let v = b.ins().fadd(v0, tl);
            let z = b.ins().fadd(u, v);
            let z_minus_u = b.ins().fsub(z, u);
            let w = b.ins().fsub(v, z_minus_u);
            let zz = b.ins().fmul(z, z);
            let p5 = f64_bits(&mut b, P5);
            let p4 = f64_bits(&mut b, P4);
            let p = b.ins().fmul(zz, p5);
            let p = b.ins().fadd(p4, p);
            let p3 = f64_bits(&mut b, P3);
            let p = b.ins().fmul(zz, p);
            let p = b.ins().fadd(p3, p);
            let p2 = f64_bits(&mut b, P2);
            let p = b.ins().fmul(zz, p);
            let p = b.ins().fadd(p2, p);
            let p1 = f64_bits(&mut b, P1);
            let p = b.ins().fmul(zz, p);
            let p = b.ins().fadd(p1, p);
            let zzp = b.ins().fmul(zz, p);
            let t1 = b.ins().fsub(z, zzp);
            let zt1 = b.ins().fmul(z, t1);
            let two = f64_bits(&mut b, 2.0f64.to_bits());
            let denom = b.ins().fsub(t1, two);
            let quotient = b.ins().fdiv(zt1, denom);
            let zw = b.ins().fmul(z, w);
            let wsum = b.ins().fadd(w, zw);
            let r = b.ins().fsub(quotient, wsum);
            let one = f64_bits(&mut b, 1.0f64.to_bits());
            let r_minus_z = b.ins().fsub(r, z);
            let result = b.ins().fsub(one, r_minus_z);

            let result_bits = as_i64_bits(&mut b, result);
            let exponent = b.ins().ushr_imm_u(result_bits, 52);
            let exp_mask = b.ins().iconst(types::I64, 0x7ff);
            let exponent = b.ins().band(exponent, exp_mask);
            let new_exponent = b.ins().iadd(exponent, n);
            let subnormal_scale = b.create_block();
            let normal_scale = b.create_block();
            let became_subnormal =
                b.ins()
                    .icmp_imm_s(IntCC::SignedLessThanOrEqual, new_exponent, 0);
            b.ins()
                .brif(became_subnormal, subnormal_scale, &[], normal_scale, &[]);

            b.switch_to_block(normal_scale);
            b.seal_block(normal_scale);
            let shifted_n = b.ins().ishl_imm_u(n, 52);
            let scaled_bits = b.ins().iadd(result_bits, shifted_n);
            let scaled = from_i64_bits(&mut b, scaled_bits);
            b.ins().return_(&[scaled]);

            b.switch_to_block(subnormal_scale);
            b.seal_block(subnormal_scale);
            // fdlibm scalbn's prescale keeps the final rounding single even for
            // the smallest subnormal result.
            let prescale = f64_bits(&mut b, TWO_NEG_969);
            let prescaled = b.ins().fmul(result, prescale);
            let n2 = b.ins().iadd_imm_u(n, 969);
            let biased = b.ins().iadd_imm_u(n2, 1023);
            let scale_bits = b.ins().ishl_imm_u(biased, 52);
            let scale = from_i64_bits(&mut b, scale_bits);
            let scaled = b.ins().fmul(prescaled, scale);
            b.ins().return_(&[scaled]);

            b.seal_all_blocks();
            b.finalize(self.module.target_config());
        }
        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    fn define_pow_f64(
        &mut self,
        func_id: FuncId,
        sig: cranelift_codegen::ir::Signature,
        log_id: FuncId,
        exp_id: FuncId,
    ) -> Result<()> {
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());
        let log_ref = self.module.declare_func_in_func(log_id, &mut ctx.func);
        let exp_ref = self.module.declare_func_in_func(exp_id, &mut ctx.func);
        let mut fn_ctx = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
            let entry = b.create_block();
            b.append_block_params_for_function_params(entry);
            b.switch_to_block(entry);
            b.seal_block(entry);
            let x = b.block_params(entry)[0];
            let y = b.block_params(entry)[1];
            let x_bits = as_i64_bits(&mut b, x);
            let y_bits = as_i64_bits(&mut b, y);
            let abs_mask = b.ins().iconst(types::I64, ABS_MASK as i64);
            let ax_bits = b.ins().band(x_bits, abs_mask);
            let ay_bits = b.ins().band(y_bits, abs_mask);

            let y_zero = b.create_block();
            let x_one_test = b.create_block();
            let is_y_zero = b.ins().icmp_imm_u(IntCC::Equal, ay_bits, 0);
            b.ins().brif(is_y_zero, y_zero, &[], x_one_test, &[]);
            b.switch_to_block(y_zero);
            b.seal_block(y_zero);
            let one = f64_bits(&mut b, 1.0f64.to_bits());
            b.ins().return_(&[one]);

            b.switch_to_block(x_one_test);
            b.seal_block(x_one_test);
            let x_one = b.create_block();
            let nan_test = b.create_block();
            let is_x_one = b
                .ins()
                .icmp_imm_u(IntCC::Equal, x_bits, 1.0f64.to_bits() as i64);
            b.ins().brif(is_x_one, x_one, &[], nan_test, &[]);
            b.switch_to_block(x_one);
            b.seal_block(x_one);
            let one = f64_bits(&mut b, 1.0f64.to_bits());
            b.ins().return_(&[one]);

            b.switch_to_block(nan_test);
            b.seal_block(nan_test);
            let nan = b.create_block();
            let y_inf_test = b.create_block();
            let inf_bits = b.ins().iconst(types::I64, POS_INFINITY as i64);
            let x_nan = b.ins().icmp(IntCC::UnsignedGreaterThan, ax_bits, inf_bits);
            let y_nan = b.ins().icmp(IntCC::UnsignedGreaterThan, ay_bits, inf_bits);
            let any_nan = b.ins().bor(x_nan, y_nan);
            b.ins().brif(any_nan, nan, &[], y_inf_test, &[]);
            b.switch_to_block(nan);
            b.seal_block(nan);
            let propagated = b.ins().fadd(x, y);
            b.ins().return_(&[propagated]);

            b.switch_to_block(y_inf_test);
            b.seal_block(y_inf_test);
            let y_inf = b.create_block();
            let classify_y = b.create_block();
            let is_y_inf = b.ins().icmp(IntCC::Equal, ay_bits, inf_bits);
            b.ins().brif(is_y_inf, y_inf, &[], classify_y, &[]);
            b.switch_to_block(y_inf);
            b.seal_block(y_inf);
            let ax = b.ins().fabs(x);
            let one = f64_bits(&mut b, 1.0f64.to_bits());
            let ax_eq_one = b.ins().fcmp(FloatCC::Equal, ax, one);
            let y_inf_nonunit = b.create_block();
            let y_inf_unit = b.create_block();
            b.ins().brif(ax_eq_one, y_inf_unit, &[], y_inf_nonunit, &[]);
            b.switch_to_block(y_inf_unit);
            b.seal_block(y_inf_unit);
            b.ins().return_(&[one]);
            b.switch_to_block(y_inf_nonunit);
            b.seal_block(y_inf_nonunit);
            let y_negative = b.ins().icmp_imm_s(IntCC::SignedLessThan, y_bits, 0);
            let ax_gt_one = b.ins().fcmp(FloatCC::GreaterThan, ax, one);
            let zero = f64_bits(&mut b, 0.0f64.to_bits());
            let pos_inf = f64_bits(&mut b, POS_INFINITY);
            let greater_pos = b.ins().select(y_negative, zero, pos_inf);
            let lesser_pos = b.ins().select(y_negative, pos_inf, zero);
            let answer = b.ins().select(ax_gt_one, greater_pos, lesser_pos);
            b.ins().return_(&[answer]);

            // Pure bit classification: every finite representable |y| >= 2^53
            // is integral and even; smaller values expose fractional/odd bits.
            b.switch_to_block(classify_y);
            b.seal_block(classify_y);
            let ay = b.ins().fabs(y);
            let one_f = f64_bits(&mut b, 1.0f64.to_bits());
            let two53_f = f64_bits(&mut b, TWO53);
            let large = b.ins().fcmp(FloatCC::GreaterThanOrEqual, ay, two53_f);
            let at_least_one = b.ins().fcmp(FloatCC::GreaterThanOrEqual, ay, one_f);
            let not_large = b.ins().bxor_imm_s(large, 1);
            let small_range = b.ins().band(at_least_one, not_large);
            let exponent_bits = b.ins().ushr_imm_u(ay_bits, 52);
            let exponent_bits = b.ins().band_imm_u(exponent_bits, 0x7ff);
            let exponent = b.ins().iadd_imm_s(exponent_bits, -1023);
            let fifty_two = b.ins().iconst(types::I64, 52);
            let frac_shift = b.ins().isub(fifty_two, exponent);
            let one_i = b.ins().iconst(types::I64, 1);
            let frac_unit = b.ins().ishl(one_i, frac_shift);
            let frac_mask = b.ins().iadd_imm_s(frac_unit, -1);
            let fraction = b.ins().band(ay_bits, frac_mask);
            let no_fraction = b.ins().icmp_imm_u(IntCC::Equal, fraction, 0);
            let small_integral = b.ins().band(small_range, no_fraction);
            let is_integral = b.ins().bor(large, small_integral);
            let shifted = b.ins().ushr(ay_bits, frac_shift);
            let low_bit = b.ins().band_imm_u(shifted, 1);
            let odd_bit = b.ins().icmp_imm_u(IntCC::Equal, low_bit, 1);
            let is_odd = b.ins().band(small_integral, odd_bit);

            let zero_inf_test = b.create_block();
            let ax_zero = b.ins().icmp_imm_u(IntCC::Equal, ax_bits, 0);
            let ax_special = b.create_block();
            b.ins().brif(ax_zero, ax_special, &[], zero_inf_test, &[]);

            b.switch_to_block(zero_inf_test);
            b.seal_block(zero_inf_test);
            let ax_inf = b.ins().icmp(IntCC::Equal, ax_bits, inf_bits);
            let finite = b.create_block();
            b.ins().brif(ax_inf, ax_special, &[], finite, &[]);

            b.switch_to_block(ax_special);
            b.seal_block(ax_special);
            let y_negative = b.ins().icmp_imm_s(IntCC::SignedLessThan, y_bits, 0);
            let ax = from_i64_bits(&mut b, ax_bits);
            let one = f64_bits(&mut b, 1.0f64.to_bits());
            let reciprocal = b.ins().fdiv(one, ax);
            let magnitude = b.ins().select(y_negative, reciprocal, ax);
            let x_negative = b.ins().icmp_imm_s(IntCC::SignedLessThan, x_bits, 0);
            let negative = b.ins().band(x_negative, is_odd);
            let negated = b.ins().fneg(magnitude);
            let answer = b.ins().select(negative, negated, magnitude);
            b.ins().return_(&[answer]);

            b.switch_to_block(finite);
            b.seal_block(finite);
            let x_negative = b.ins().icmp_imm_s(IntCC::SignedLessThan, x_bits, 0);
            let not_integral = b.ins().bxor_imm_s(is_integral, 1);
            let invalid_negative = b.ins().band(x_negative, not_integral);
            let invalid = b.create_block();
            let fast_path_test = b.create_block();
            b.ins()
                .brif(invalid_negative, invalid, &[], fast_path_test, &[]);
            b.switch_to_block(invalid);
            b.seal_block(invalid);
            let qnan = f64_bits(&mut b, QUIET_NAN);
            b.ins().return_(&[qnan]);

            b.switch_to_block(fast_path_test);
            b.seal_block(fast_path_test);
            let in_convert_range = b.ins().fcmp(FloatCC::LessThanOrEqual, ay, two53_f);
            let use_integral = b.ins().band(is_integral, in_convert_range);
            let integral_check = b.create_block();
            let integral = b.create_block();
            b.append_block_param(integral, types::I64);
            let generic = b.create_block();
            b.ins()
                .brif(use_integral, integral_check, &[], generic, &[]);

            b.switch_to_block(integral_check);
            b.seal_block(integral_check);
            // Guarded by finite + |y|<=2^53 + integral and verified by the
            // source-independent bit classifier above. Keep the explicit
            // round trip as the final fail-closed proof before entering the
            // integer-only loop.
            let exponent = b.ins().fcvt_to_sint(types::I64, y);
            let round_trip = b.ins().fcvt_from_sint(types::F64, exponent);
            let exact = b.ins().fcmp(FloatCC::Equal, round_trip, y);
            b.ins()
                .brif(exact, integral, &[exponent.into()], generic, &[]);

            b.switch_to_block(integral);
            b.seal_block(integral);
            let exponent = b.block_params(integral)[0];
            let exponent_negative = b.ins().icmp_imm_s(IntCC::SignedLessThan, exponent, 0);
            let neg_exponent = b.ins().ineg(exponent);
            let remaining = b.ins().select(exponent_negative, neg_exponent, exponent);
            let abs_x = from_i64_bits(&mut b, ax_bits);
            let header = b.create_block();
            let body = b.create_block();
            let advance = b.create_block();
            let done = b.create_block();
            for block in [header, body] {
                for ty in [types::F64, types::I64, types::F64] {
                    b.append_block_param(block, ty);
                }
            }
            b.append_block_param(done, types::F64);
            let one = f64_bits(&mut b, 1.0f64.to_bits());
            b.ins()
                .jump(header, &[abs_x.into(), remaining.into(), one.into()]);

            b.switch_to_block(header);
            let carried = b.block_params(header).to_vec();
            let is_done = b.ins().icmp_imm_u(IntCC::Equal, carried[1], 0);
            b.ins().brif(
                is_done,
                done,
                &[carried[2].into()],
                body,
                &[carried[0].into(), carried[1].into(), carried[2].into()],
            );

            b.switch_to_block(body);
            b.seal_block(body);
            let carried = b.block_params(body).to_vec();
            let low = b.ins().band_imm_u(carried[1], 1);
            let odd = b.ins().icmp_imm_u(IntCC::Equal, low, 1);
            let product = b.ins().fmul(carried[2], carried[0]);
            let next_result = b.ins().select(odd, product, carried[2]);
            let next_remaining = b.ins().ushr_imm_u(carried[1], 1);
            let finished = b.ins().icmp_imm_u(IntCC::Equal, next_remaining, 0);
            b.ins()
                .brif(finished, done, &[next_result.into()], advance, &[]);

            b.switch_to_block(advance);
            b.seal_block(advance);
            let next_acc = b.ins().fmul(carried[0], carried[0]);
            b.ins().jump(
                header,
                &[next_acc.into(), next_remaining.into(), next_result.into()],
            );
            b.seal_block(header);

            b.switch_to_block(done);
            b.seal_block(done);
            let magnitude = b.block_params(done)[0];
            let reciprocal = b.ins().fdiv(one, magnitude);
            let magnitude = b.ins().select(exponent_negative, reciprocal, magnitude);
            let negative = b.ins().band(x_negative, is_odd);
            let negated = b.ins().fneg(magnitude);
            let answer = b.ins().select(negative, negated, magnitude);
            b.ins().return_(&[answer]);

            b.switch_to_block(generic);
            b.seal_block(generic);
            let abs_x = from_i64_bits(&mut b, ax_bits);
            let log_call = b.ins().call(log_ref, &[abs_x]);
            let log = b.inst_results(log_call).to_vec();
            let y1 = clear_low_word(&mut b, y);
            let y_tail = b.ins().fsub(y, y1);
            let tail_hi = b.ins().fmul(y_tail, log[0]);
            let tail_lo = b.ins().fmul(y, log[1]);
            let p_l = b.ins().fadd(tail_hi, tail_lo);
            let p_h = b.ins().fmul(y1, log[0]);
            let exp_call = b.ins().call(exp_ref, &[p_h, p_l]);
            let magnitude = b.inst_results(exp_call)[0];
            let negative = b.ins().band(x_negative, is_odd);
            let negated = b.ins().fneg(magnitude);
            let answer = b.ins().select(negative, negated, magnitude);
            b.ins().return_(&[answer]);

            b.seal_all_blocks();
            b.finalize(self.module.target_config());
        }
        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }
}

fn const_f64_operand(builder: &FunctionBuilder<'_>, value: Value) -> Option<f64> {
    let dfg = &builder.func.dfg;
    let ValueDef::Result(inst, 0) = dfg.value_def(value) else {
        return None;
    };
    match dfg.insts[inst] {
        InstructionData::UnaryIeee64 {
            opcode: Opcode::F64const,
            imm,
        } => Some(f64::from_bits(imm.bits())),
        InstructionData::Unary {
            opcode: Opcode::Fneg,
            arg,
        } => const_f64_operand(builder, arg).map(|value| -value),
        _ => None,
    }
}

impl FuncGen<'_, '_> {
    /// Emit `f64 ** f64`. Integral literals within the exact i64 conversion
    /// range are unrolled at the call site; all other inputs use the one local
    /// dispatcher, which includes the matching dynamic integral loop.
    pub(super) fn emit_pow_f64(&mut self, base: Value, exponent: Value) -> Value {
        if let Some(value) = const_f64_operand(self.builder, exponent)
            && value.is_finite()
            && value.fract() == 0.0
            && value.abs() <= (1u64 << 53) as f64
        {
            return self.emit_pow_f64_integral_literal(base, value);
        }
        let fid = self.func_id(POW_F64_SYMBOL);
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[base, exponent]);
        self.builder.inst_results(call)[0]
    }

    fn emit_pow_f64_integral_literal(&mut self, base: Value, exponent: f64) -> Value {
        let negative = exponent.is_sign_negative() && exponent != 0.0;
        let magnitude = exponent.abs() as u64;
        let steps = super::emit_pow::pow_unroll_steps(magnitude);
        let mut accumulator = base;
        let mut result = None;
        for step in steps {
            match step {
                super::emit_pow::PowStep::Mul => {
                    result = Some(match result {
                        None => accumulator,
                        Some(current) => self.builder.ins().fmul(current, accumulator),
                    });
                }
                super::emit_pow::PowStep::Square => {
                    accumulator = self.builder.ins().fmul(accumulator, accumulator);
                }
            }
        }
        let one = self.builder.ins().f64const(1.0);
        let result = result.unwrap_or(one);
        if negative {
            self.builder.ins().fdiv(one, result)
        } else {
            result
        }
    }
}
