use cranelift_codegen::ir::{InstBuilder, types};
use cranelift_module::Module;

use super::*;

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Emit `[e0, e1, ...]`: allocate an array sized to the literal, then store
    /// each element through the array ABI. The array is rooted during element
    /// evaluation so a GC triggered mid-construction keeps it (and its stored
    /// elements) alive.
    pub(super) fn emit_array_literal(
        &mut self,
        elements: &[Expr],
        elem_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let len_val = self.builder.ins().iconst(types::I64, elements.len() as i64);
        let is_ref = if is_gc_managed(elem_ty, self.enum_infos) {
            1
        } else {
            0
        };
        let is_ref_val = self.builder.ins().iconst(types::I64, is_ref);
        let new_id = self.func_id("willow_array_new");
        let new_ref = self.module.declare_func_in_func(new_id, self.builder.func);
        let call = self.builder.ins().call(new_ref, &[len_val, is_ref_val]);
        let arr = self.builder.inst_results(call)[0];

        self.emit_push_root(arr);
        for (i, el) in elements.iter().enumerate() {
            // Box class elements when the array's element type is an interface.
            let val = self.emit_expr_coerced(el, elem_ty);
            let word = self.coerce_to_i64(val, elem_ty);
            let idx_val = self.builder.ins().iconst(types::I64, i as i64);
            let set_id = self.func_id("willow_array_set");
            let set_ref = self.module.declare_func_in_func(set_id, self.builder.func);
            self.builder.ins().call(set_ref, &[arr, idx_val, word]);
        }
        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        arr
    }

    /// Emit `arr[index]`: bounds-checked element read, converted back to the
    /// element type.
    pub(super) fn emit_index(
        &mut self,
        arr_expr: &Expr,
        index_expr: &Expr,
        elem_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let arr = self.emit_expr(arr_expr);
        let index = self.emit_expr(index_expr);
        let get_id = self.func_id("willow_array_get");
        let get_ref = self.module.declare_func_in_func(get_id, self.builder.func);
        let call = self.builder.ins().call(get_ref, &[arr, index]);
        let word = self.builder.inst_results(call)[0];
        self.coerce_i64_to(word, elem_ty)
    }

    /// Emit a `Map<K, V>` method call. Keys/values cross the runtime ABI as raw
    /// 64-bit words plus ref-ness flags; `get` returns a runtime-built
    /// `Option<V>` pointer.
    pub(super) fn emit_map_method_call(
        &mut self,
        map: cranelift_codegen::ir::Value,
        key_ty: &Type,
        val_ty: &Type,
        m: &MethodCallExpr,
    ) -> cranelift_codegen::ir::Value {
        let key_is_ref = self.builder.ins().iconst(
            types::I64,
            i64::from(is_gc_managed(key_ty, self.enum_infos)),
        );
        match m.method.as_str() {
            "insert" => {
                // Root the map while evaluating key/value (either may allocate).
                // A GC-managed key must also stay rooted while the value is
                // evaluated, and both key and value must stay rooted across the
                // insert call itself, which may allocate (grow buckets / copy)
                // and trigger a collection before they are stored (willow-oewp.6).
                self.emit_push_root(map);
                let mut temp_roots = 1usize;
                let k = self.emit_expr(&m.args[0].expr);
                let k_word = self.coerce_to_i64(k, key_ty);
                if is_gc_managed(key_ty, self.enum_infos) {
                    self.emit_push_root(k);
                    temp_roots += 1;
                }
                let v = self.emit_expr(&m.args[1].expr);
                let v_word = self.coerce_to_i64(v, val_ty);
                if is_gc_managed(val_ty, self.enum_infos) {
                    self.emit_push_root(v);
                    temp_roots += 1;
                }
                let val_is_ref = self.builder.ins().iconst(
                    types::I64,
                    i64::from(is_gc_managed(val_ty, self.enum_infos)),
                );
                let id = self.func_id("willow_map_insert");
                let r = self.module.declare_func_in_func(id, self.builder.func);
                self.builder
                    .ins()
                    .call(r, &[map, k_word, key_is_ref, v_word, val_is_ref]);
                self.emit_pop_roots_n(temp_roots);
                self.gc_root_count -= temp_roots;
                self.builder.ins().iconst(types::I64, 0) // void
            }
            "get" => {
                // Root the map across the get call: it allocates the `Option<V>`
                // result, and a temporary map (reachable only here) must survive
                // that allocation so its stored value is not collected
                // (willow-oewp.6).
                self.emit_push_root(map);
                let k = self.emit_expr(&m.args[0].expr);
                let k_word = self.coerce_to_i64(k, key_ty);
                let id = self.func_id("willow_map_get");
                let r = self.module.declare_func_in_func(id, self.builder.func);
                let call = self.builder.ins().call(r, &[map, k_word, key_is_ref]);
                let result = self.builder.inst_results(call)[0]; // Option<V> pointer
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
                result
            }
            "contains" => {
                let k = self.emit_expr(&m.args[0].expr);
                let k_word = self.coerce_to_i64(k, key_ty);
                let id = self.func_id("willow_map_contains");
                let r = self.module.declare_func_in_func(id, self.builder.func);
                let call = self.builder.ins().call(r, &[map, k_word, key_is_ref]);
                let raw = self.builder.inst_results(call)[0];
                self.builder.ins().ireduce(types::I8, raw) // bool
            }
            "len" => {
                let id = self.func_id("willow_map_len");
                let r = self.module.declare_func_in_func(id, self.builder.func);
                let call = self.builder.ins().call(r, &[map]);
                self.builder.inst_results(call)[0]
            }
            // `map.freeze()` -> an immutable copy (willow-dgwo.10).
            "freeze" => {
                let id = self.func_id("willow_map_copy");
                let r = self.module.declare_func_in_func(id, self.builder.func);
                let call = self.builder.ins().call(r, &[map]);
                self.builder.inst_results(call)[0]
            }
            _ => self.builder.ins().iconst(types::I64, 0),
        }
    }
}
