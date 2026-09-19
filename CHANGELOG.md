# Changelog

Notable user-facing changes to the Willow compiler, runtime, and toolchain.

## Unreleased

### Breaking

- **E0810 now rejects recursive synchronous helpers called from task context.**
  Previously the non-preemptibility analysis seeded only from helpers that
  contained a loop, so a loop-free recursive helper — the classic
  `fib(n - 1) + fib(n - 2)` — was admitted into a task and ran to completion on
  the scheduler worker, starving every other runnable task. The analysis now
  also seeds from strongly connected components of the synchronous call graph,
  so direct self recursion, mutual recursion, and longer cycles are caught, as
  is any helper that transitively reaches one.

  Programs that compiled before may now fail with:

  ```text
  error[E0810]: sync helper `fib` can run unbounded recursive work in task context
  ```

  To migrate, move the work into the `async fn` itself. An async fn gets
  safepoints at every loop backedge and before every call-bearing statement, so
  an iterative version stays preemptible and the scheduler stays fair. Calling
  the recursive helper from ordinary synchronous code is unaffected — a
  synchronous caller holds no scheduler worker.

  **This rejection is temporary.** It is lifted once task-aware
  synchronous-stack preemption ships, at which point recursive SCCs preempt
  instead of erroring. See `example/task_recursion_rejected.wi`.

### Fixed

- **Class `extends` cycles are now rejected with E0426.** A ring such as
  `open class A extends B {}` / `open class B extends A {}` used to compile
  silently when the classes had no fields, and to abort with an internal
  compiler error (E0800, "constructor `A` was not lowered to operands") when
  they did. The checker now walks each class's base chain and reports a chain
  that returns to its start once per cycle, labelling every member:

  ```text
  error[E0426]: cyclic class inheritance involving `A`
  note: inheritance cycle: A extends B extends A
  help: remove one `extends` so the chain ends at a class with no base
  ```

  A class that merely extends into a ring is not reported on its own, and
  `new` of any class whose chain reaches a ring skips the constructor arity
  check: cutting the ring is the one fix, and the checks that depend on a
  finished base chain run again once it is cut. Interface `extends` cycles
  keep E0423. See `example/class_inheritance_cycle_rejected.wi`.

- **A local class may extend a module class of the same short name.**
  `import shapes;` followed by `class Sized extends shapes::Sized { .. }`
  used to send the compiler into an infinite loop the moment an inherited
  field such as `s.width` was read: lowering recorded the base by the last
  segment of its path, so the local `Sized` became its own base. The same
  slip made an unrelated local `class Sized { width: String }` retype
  `width` on a `Cube extends shapes::Sized`, which aborted codegen with
  E0800 ("the field `width` on a `Cube` ... has incompatible operands"). The
  base now keeps its module qualifier and the inherited-member walk stops at
  any repeated class. `import shapes::Sized as Base;` / `class Sized extends
  Base` was never affected. See `example/module_base_short_name/main.wi`.
