use cranelift_codegen::ir::{InstBuilder, MemFlagsData, types};
use cranelift_module::Module;

use super::*;

#[willow_continuations::methods(emit_lir_interpolated, emit_lir_expr)]
impl<'a, 'b> FuncGen<'a, 'b> {
    /// The address of `class`'s DESCRIPTOR: the value that lives in word 0 of
    /// every object of that class (willow-fm7t).
    ///
    /// The descriptor holds the class's `type_id` at its own offset 0, followed
    /// by one word per virtual method slot. One store of this pointer at
    /// construction is therefore what makes both `is`/downcast and O(1) virtual
    /// dispatch work, without growing the object or moving any field.
    pub(super) fn class_descriptor_addr(&mut self, class: &str) -> cranelift_codegen::ir::Value {
        let data_id = self
            .class_descriptor_ids
            .get(class)
            .copied()
            .unwrap_or_else(|| {
                panic!("compiler invariant violated: checked class `{class}` has no descriptor")
            });
        let gv = self.module.declare_data_in_func(data_id, self.builder.func);
        let ptr_ty = self.module.target_config().pointer_type();
        self.builder.ins().symbol_value(ptr_ty, gv)
    }

    /// Store `class`'s descriptor address into word 0 of a freshly allocated
    /// object (willow-fm7t).
    pub(super) fn emit_store_class_descriptor(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        class: &str,
    ) {
        let descriptor = self.class_descriptor_addr(class);
        self.builder
            .ins()
            .store(MemFlagsData::new(), descriptor, ptr, 0i32);
    }

    /// Load the runtime `type_id` of the object `ptr` points at (willow-fm7t).
    ///
    /// Two dependent loads rather than one: word 0 of the object is the
    /// descriptor address, and offset 0 of the descriptor is the id. Only the
    /// comparatively rare `is`/downcast paths pay for this; virtual dispatch
    /// reads a slot from the same descriptor and never materialises the id.
    pub(super) fn emit_load_runtime_type_id(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let ptr_ty = self.module.target_config().pointer_type();
        let descriptor = self
            .builder
            .ins()
            .load(ptr_ty, MemFlagsData::new(), ptr, 0i32);
        self.builder
            .ins()
            .load(types::I64, MemFlagsData::new(), descriptor, 0i32)
    }
}

#[willow_continuations::methods(emit_lir_interpolated, emit_lir_expr)]
impl<'a, 'b> FuncGen<'a, 'b> {
    /// The body of [`FuncGen::emit_interpolated_string`], with the arguments
    /// supplied by a callback rather than read from the AST.
    ///
    /// The callback exists so the LIR walker can reach the same emitter with
    /// typed HIR operands (willow-0g8j.2.5): a format string that assembled its
    /// pieces differently on the two paths would produce two different strings
    /// for one program. Arguments stay LAZY — each is emitted only when its
    /// placeholder is reached — because an operand that has not been evaluated
    /// yet cannot be collected, which is what lets the rooting below be exactly
    /// one push per live piece.
    pub(super) fn emit_lir_interpolated(
        &mut self,
        spec: &str,
        operands: &[crate::ir::typed_ast::HirExpr],
    ) -> cranelift_codegen::ir::Value {
        let segments = match crate::interpolate::parse_spec(spec) {
            Ok(segments) => segments,
            // The checker rejected invalid specs; only synthesized nodes could
            // land here.
            Err(_) => return self.emit_string_literal(spec),
        };
        let mut next_arg = 0usize;
        let mut acc: Option<cranelift_codegen::ir::Value> = None;
        let mut temp_roots = 0usize;
        for segment in &segments {
            // Every step below can allocate (toString / concat), and any
            // allocation can collect — so each live string is rooted the
            // instant it exists, and stays rooted until the final pop.
            let piece = match segment {
                crate::interpolate::Segment::Literal(text) => {
                    // Literals are permanent (runtime-rooted) — no root needed.
                    let text = text.clone();
                    self.emit_string_literal(&text)
                }
                crate::interpolate::Segment::Display => {
                    if next_arg >= operands.len() {
                        break;
                    }
                    let val = self.emit_lir_expr(&operands[next_arg]);
                    let ty = operands[next_arg].ty.clone();
                    next_arg += 1;
                    let converted = match ty {
                        Type::String => val,
                        Type::F64 => self.emit_runtime_call1("willow_f64_to_string", val),
                        Type::Bool => self.emit_runtime_call1("willow_bool_to_string", val),
                        _ => self.emit_runtime_call1("willow_i64_to_string", val),
                    };
                    self.emit_push_root(converted);
                    temp_roots += 1;
                    converted
                }
                crate::interpolate::Segment::F64(format) => {
                    if next_arg >= operands.len() {
                        break;
                    }
                    let val = self.emit_lir_expr(&operands[next_arg]);
                    next_arg += 1;
                    let converted = self.emit_runtime_call1(format.runtime_symbol(), val);
                    self.emit_push_root(converted);
                    temp_roots += 1;
                    converted
                }
            };
            acc = Some(match acc {
                None => piece,
                Some(prev) => {
                    // Both operands are rooted; the result gets rooted too so
                    // it survives the NEXT piece's allocations.
                    let joined =
                        self.emit_value_runtime_call("willow_string_concat", &[prev, piece]);
                    self.emit_push_root(joined);
                    temp_roots += 1;
                    joined
                }
            });
        }
        if temp_roots > 0 {
            self.emit_pop_roots_n(temp_roots);
            self.gc_root_count -= temp_roots;
        }
        acc.unwrap_or_else(|| self.emit_string_literal(""))
    }

    /// Call a one-argument runtime function and return its single result.
    fn emit_runtime_call1(
        &mut self,
        symbol: &str,
        arg: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        self.emit_value_runtime_call(symbol, &[arg])
    }
}
