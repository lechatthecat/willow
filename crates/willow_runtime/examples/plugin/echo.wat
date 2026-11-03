(module
  (memory (export "memory") 1 1)
  ;; Input occupies [0, length); return the same slice (offset << 32 | length).
  (func (export "call") (param i32 i32 i32 i64) (result i64)
    local.get 2
    i64.extend_i32_u))
