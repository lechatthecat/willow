//! Post-return debug instrumentation; the bridge and its shim keep their ABI.
use super::*;
use crate::diagnostics::Span;
use willow_abi::runtime_symbols::names as rt;

impl UnitCodegenContext<'_> {
    pub(super) fn declare_handle_debug_output(&mut self, f: &FunctionDecl) -> Result<()> {
        if self.build_mode != BuildMode::Debug {
            return Ok(());
        }
        let ty = self.canonical_enum_type(&f.return_type);
        if self.output.handle_debug_outputs.contains_key(&ty) {
            return Ok(());
        }
        let Some(words) = f
            .rust_bridge
            .as_ref()
            .and_then(|b| b.handle_debug_descriptor())
        else {
            self.output.handle_debug_outputs.insert(ty, None);
            return Ok(());
        };
        let name = format!(
            "__willow_handle_shape_{}",
            self.output.handle_debug_outputs.len()
        );
        let id = self
            .output
            .module
            .declare_data(&name, Linkage::Local, false, false)?;
        let mut data = DataDescription::new();
        data.set_align(8);
        // All supported targets use little-endian 64-bit words.
        data.define(
            words
                .into_iter()
                .flat_map(u64::to_le_bytes)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        self.output.module.define_data(id, &data)?;
        self.output.handle_debug_outputs.insert(ty, Some(id));
        Ok(())
    }
}

impl FuncGen<'_, '_> {
    pub(super) fn emit_handle_debug_site(
        &mut self,
        result: cranelift_codegen::ir::Value,
        ty: &Type,
        span: Span,
    ) {
        if self.diagnostics.build_mode != BuildMode::Debug || self.locals.terminated {
            return;
        }
        let Some(&Some(descriptor)) = self.handle_debug_outputs.get(ty) else {
            return;
        };
        let file = self.diagnostics.source_file.to_string();
        let (file, len) = self
            .emit_static_str_bytes(&file)
            .expect("source path declared for debug calls");
        let global = self
            .module
            .declare_data_in_func(descriptor, self.builder.func);
        let shape = self
            .builder
            .ins()
            .symbol_value(reference_type(self.module.target_config()), global);
        let line = self.builder.ins().iconst(types::I32, span.line as i64);
        self.emit_void_runtime_call(
            rt::willow_debug_record_handle_output,
            &[result, shape, file, len, line],
        );
    }
}
