# Mutually recursive types and module imports

Willow requires an acyclic module import graph. This applies even when imports
are used only in field types or enum payloads; there is no type-only import
exception. E0403 reports the module path cycle, without internal package indices.
Paths in the cycle are package-local module names, not import aliases. Dependency
package cycles are rejected separately by package resolution.

Keep a group of mutually recursive declarations in one shared model module, and
put operations in separate modules that import that model. The model must not
import those operation modules. This preserves type identity without duplicating
declarations, and lets the loader resolve dependencies before their consumers.

The runnable example in `example/recursive_type_modules/` contains:

- `model.wi`: `Value::Proc(Lambda)`, `Lambda.env: Scope`, and
  `Scope.vars: Map<String, Value>` in one module.
- `scope.wi`: scope construction using the model.
- `value.wi`: operations matching model values and inspecting captured scopes.
- `main.wi`: imports the model and both operation modules.

Run from the repository root:

```sh
cargo run --bin willow -- run example/recursive_type_modules/main.wi
```

Expected output is two lines containing `0`. The import graph is
`main -> {model, scope, value}`, `scope -> model`, and `value -> model`.
Types remain mutually recursive, while module imports do not form a cycle.
Moving the declarations into separate mutually importing modules is unsupported;
co-locate the declarations rather than copying them or adding reciprocal imports.
