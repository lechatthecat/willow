//! Shared scalar conversions. Types select instructions at compile time.
use super::*;
use cranelift_codegen::ir::{InstBuilder, Value, types};
use willow_abi::runtime_symbols::names as rt;

impl FuncGen<'_, '_> {
    pub(super) fn emit_scalar_conversion(
        &mut self,
        value: Value,
        source: &Type,
        target: &Type,
        checked: bool,
    ) -> Value {
        let src = source.scalar_layout().expect("scalar source");
        let dst = target.scalar_layout().expect("scalar target");
        let word = self.widen_integer(value, source);
        let native = self.clif_type(target);
        let converted = if dst.bits == 64 {
            word
        } else {
            self.builder.ins().ireduce(native, word)
        };
        if !checked {
            return converted;
        }
        let mut invalid = self.builder.ins().iconst(types::I8, 0);
        if src.min() < dst.min() {
            invalid = self
                .builder
                .ins()
                .icmp_imm_s(IntCC::SignedLessThan, word, dst.min() as i64);
        }
        let max = if *target == Type::Char {
            0x10ffff
        } else {
            dst.max()
        };
        if src.max() > max {
            let cc = if src.signed {
                IntCC::SignedGreaterThan
            } else {
                IntCC::UnsignedGreaterThan
            };
            let high = self.builder.ins().icmp_imm_s(cc, word, max as i64);
            invalid = self.builder.ins().bor(invalid, high);
        }
        if *target == Type::Char {
            let offset = self.builder.ins().iadd_imm_s(word, -0xd800);
            let surrogate = self
                .builder
                .ins()
                .icmp_imm_u(IntCC::UnsignedLessThan, offset, 0x800);
            invalid = self.builder.ins().bor(invalid, surrogate);
        }
        let tag = self.builder.ins().uextend(types::I64, invalid);
        let payload = self.widen_integer(converted, target);
        let zero = self.builder.ins().iconst(types::I64, 0);
        let payload = self.builder.ins().select(invalid, zero, payload);
        self.emit_pair(tag, payload)
    }

    pub(super) fn emit_scalar_string(&mut self, value: Value, ty: &Type) -> Value {
        let word = self.widen_integer(value, ty);
        let kind = self
            .builder
            .ins()
            .iconst(types::I64, if *ty == Type::Char { 0 } else { 64 });
        self.emit_value_runtime_call(rt::willow_scalar_to_string, &[word, kind])
    }

    pub(super) fn emit_scalar_parse(&mut self, string: Value, target: &Type) -> Value {
        let kind = if *target == Type::Char {
            0
        } else {
            let layout = target.integer_layout().unwrap();
            i64::from(layout.bits) | (i64::from(layout.signed) << 8)
        };
        let kind = self.builder.ins().iconst(types::I64, kind);
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            3,
        ));
        let ptr =
            self.builder
                .ins()
                .stack_addr(reference_type(self.module.target_config()), slot, 0);
        let success = self.emit_value_runtime_call(rt::willow_scalar_parse, &[string, kind, ptr]);
        let none = self.builder.ins().icmp_imm_s(IntCC::Equal, success, 0);
        let tag = self.builder.ins().uextend(types::I64, none);
        let word = self.builder.ins().stack_load(
            reference_type(self.module.target_config()),
            types::I64,
            slot,
            0,
        );
        self.emit_pair(tag, word)
    }
}
