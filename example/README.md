# Willow Examples

Examples in this directory are split into two groups.

- Root `*.wi` files are intended to compile and run as the current compiler grows.
- `future/**/*.wi` files are intentionally ambitious examples for planned language features. They may not compile yet.

Future examples should start with:

```text
// status: future
```

That marker lets tests keep them in the example catalog without treating them as runnable programs.

Interactive or intentionally non-terminating examples should contain:

```text
// test: manual
```

Manual examples remain runnable, but the automated example catalog does not execute them.

## Optional values

`Option<T>` is Willow's only absence type. Construct it explicitly with
`Some(value)` or `None`, and inspect it with `match`, `is_some()`, `is_none()`,
`unwrap()`, or `expect(...)`:

```willow
let name: Option<String> = Some("willow");
match name {
    Some(value) => println(value),
    None => println("missing"),
}
```

The type spelling `T?` is retained as parser sugar and means exactly
`Option<T>`; repeated suffixes preserve nesting, so `T??` means
`Option<Option<T>>`. Willow does not implicitly wrap a `T` as `Some(T)`.
See `option_absence.wi`, `gc_linked_list.wi`, and `nil_safe_chain.wi`.

## Asynchronous filesystem operations

The unsuffixed `fs::read_to_string`, `fs::write_string`, `fs::exists`, and
`fs::remove_file` functions are synchronous compatibility APIs. They return
their result immediately and therefore cannot be awaited.

Inside an `async fn`, use the blocking-pool variants so the scheduler worker is
free to run other Tasks:

```willow
let text = await fs::read_to_string_async(path);
let written = await fs::write_string_async(path, contents);
let present = await fs::exists_async(path);
let removed = await fs::remove_file_async(path);
```

See `file_io.wi` for synchronous and asynchronous forms in one program.

## Asynchronous TCP

`std::net` accepts numeric `IP:port` addresses, which keeps DNS resolution off
scheduler workers. `net::bind` creates a non-blocking `TcpListener`; readiness
operations return eagerly scheduled Tasks:

```willow
let listener = net::bind("127.0.0.1:0")?;
let address = net::local_addr(listener)?;
let accepting = net::accept_async(listener);
let client = (await net::connect_async(address))?;
(await net::write_async(client, "hello"))?;
let server = (await accepting)?;
let text = (await net::read_async(server, 4096))?;
```

`connect_async`, `accept_async`, `read_async`, and `write_async` park their Task
on epoll (Linux), kqueue (macOS), or WinSock readiness polling (Windows). Task
cancellation removes the operation's registration. See `tcp_echo.wi`.

## Cancellation and structured Tasks

`CancellationToken` fans one cancellation request out to attached Tasks. Child
tokens inherit cancellation from their parent, while cancellation never travels
from a child back to its parent:

```willow
let token = CancellationToken::new();
let first = token.attach(work());
let second = token.attach(work());
token.cancel();
```

`TaskScope` explicitly owns Tasks and nested scopes. Async calls are still eager;
`add` adopts and returns the same Task rather than spawning another frame.
`finish` closes the scope to new Tasks and returns a Task that waits for every
owned child:

```willow
let scope = TaskScope::new();
let task = scope.add(work());
match await scope.finish() {
    Ok(done) => println("all children completed"),
    Err(Cancelled) => println("a child was cancelled"),
}
```

Call `scope.cancel()` before `finish()` to cancel all descendants. Task panic
still aborts the process; ordinary cancellation is observed through
`await task.result()` or the `finish()` result. See `structured_tasks.wi`.

## Bounded parallel mapping

`parallel::map` distributes immutable scalar input over the existing M:N
worker pool. It creates at most one chunk Task per active worker, writes output
at the original input index, and therefore preserves deterministic result
ordering:

```willow
let values: Array<i64> = [5, 1, 4, 2, 3];
let squared = parallel::map(values.freeze(), |value| value * value);
println((await squared).toString());
```

The v1 mapper is `fn(i64) -> i64` — a bare code address with no environment, so
the lambda passed here must not capture an enclosing local. A lambda that does
capture is a `closure` value instead; see `lir_closures.wi`. Cancelling the returned Task cancels all chunk Tasks and exposes no
partial result. A mapper panic follows the normal Task policy and aborts the
process. See `parallel_map.wi`.
