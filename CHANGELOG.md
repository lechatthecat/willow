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
