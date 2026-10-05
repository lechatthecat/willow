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

## `if let`, `while let` and match guards

[if_let_guards.wi](if_let_guards.wi) matches a single pattern without a full
`match`:

```willow
if let Some(port) = config { println(port); } else { println("no port"); }
while let Some(top) = stack.pop() { total = total + top; }
```

The pattern's bindings exist only inside the first block. `else if let` and
`else if` chain as usual. `while let` evaluates its scrutinee again before every
iteration and stops at the first value that does not match; `break` and
`continue` work as in `while`.

A `match` arm may add a guard, `P if cond => ...`. The guard runs after `P`
binds and sees its bindings. When the guard is false, the next arm is tried. A
guarded arm never makes a match exhaustive, so `match n { x if x > 0 => 1, 0 =>
2 }` is still error E1206; add an unguarded arm such as `_ => ...`. A guard must
be `bool` (E0203). Let chains (`if let P = e && cond`) and nested patterns are
not supported; bind first and test inside the block.

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

## Map type inference

`map_inference.wi` shows an unannotated local map whose first `insert(key, value)`
determines its key and value types. Later insertions must match those types.
Use an explicit `Map<K, V>` annotation for an empty map or when reading its
contents before an insertion provides type information.

`map_float_keys.wi` demonstrates `Map<f64, V>` key equality: both signed
zeros use the single canonical `+0.0` key. All other non-NaN numbers, including
infinities, retain their bits. NaN keys raise the recoverable runtime panic
`NaN cannot be used as a Map key` in `insert`, `get`, and `contains`.
`FrozenMap` uses the same rules. Ordinary floating-point arithmetic and `==`
keep IEEE semantics, and NaN remains valid as a map value. Map `toString()`
sorts entries by rendered key text for deterministic output.

## Integer arithmetic

[Integer overflow](integer_overflow.wi) shows the `i64` overflow policy:

- Debug builds (the default `willow build`) check `+`, `-`, `*`, `**` and
  prefix `-` on `i64`. On overflow they raise the recoverable panic
  ``integer overflow: `<op>` `` (for negation, ``integer overflow: negation of
  `i64::MIN` ``) at the operator's `file:line:col`. `defer` and `recover()`
  can catch it, as they can a division by zero.
- `willow build --release` does not check these operators: they wrap modulo
  2^64 (two's complement).
- Division and remainder by zero, and `i64::MIN / -1` and `i64::MIN % -1`,
  panic in every build.
- Constant expressions fold only when the result fits. An overflowing constant
  expression follows the same per-build rule at run time.

The explicit methods behave the same in both builds:

| Method | Result and semantics |
| --- | --- |
| `wrapping_add(other: i64)` | `i64` sum modulo 2^64. |
| `wrapping_sub(other: i64)` | `i64` difference modulo 2^64. |
| `wrapping_mul(other: i64)` | `i64` product modulo 2^64. |
| `wrapping_neg()` | `i64` negation modulo 2^64; `i64::MIN` stays `i64::MIN`. |
| `checked_add(other: i64)` | `Option<i64>`: `Some(sum)`, or `None` on overflow. |
| `checked_sub(other: i64)` | `Option<i64>`: `Some(difference)`, or `None` on overflow. |
| `checked_mul(other: i64)` | `Option<i64>`: `Some(product)`, or `None` on overflow. |
| `checked_neg()` | `Option<i64>`: `Some(-value)`, or `None` for `i64::MIN`. |

Code that relies on modular arithmetic, such as hashes or random-number
generators, should use the `wrapping_*` methods so that it does not panic in
debug builds.

[Bitwise operators](bitwise_operators.wi) shows `&`, `|`, `^`, `<<`, `>>`
and prefix `!` on `i64`, with the compound forms `&=`, `|=`, `^=`, `<<=` and
`>>=`:

- They accept only `i64` operands. On `bool`, use `&&`, `||` and `!=`.
  Prefix `!` is bitwise not on `i64` and logical not on `bool`; there is no
  `~` operator.
- Precedence follows Rust, from loosest to tightest: `||`, `&&`, comparisons,
  `|`, `^`, `&`, `<<`/`>>`, `+`/`-`, `*`/`/`/`%`, prefix `-`/`!`, `**`. All
  binary operators are left-associative except `**`. So `1 + 2 << 3` is 24 and
  `flags & READ != 0` compares the masked value.
- `>>` is an arithmetic shift: it copies the sign bit, so `-16 >> 2` is -4.
- A shift amount outside `0..64` raises the recoverable panic
  ``integer overflow: `<<` shift amount outside 0..64`` (or `` `>>` ``) in a
  debug build. A `--release` build uses the amount's low six bits. `<<` never
  checks for bits shifted out of the top, so `3 << 62` is a valid negative
  number.
- `<<` and `>>` are written without a space between the two characters.
  A generic type such as `Option<Option<i64>>` still closes normally.

## String methods

`String.len()` returns an `i64` UTF-8 byte count in constant time, excluding
the trailing NUL: `"".len()` is 0, `"abc".len()` is 3, and `"日本語".len()`
is 9. It does not measure Unicode characters, grapheme clusters, or terminal
columns. See `language_gaps/main.wi` for length and imported-array examples.

[String methods](string_methods.wi) demonstrates the remaining core API:

| Method | Result and semantics |
| --- | --- |
| `substring(start: i64, end: i64)` / `slice(start: i64, end: i64)` | New `String` containing the byte range `[start, end)`. Both positions must be UTF-8 boundaries, with `0 <= start <= end <= len()`. Invalid ranges raise a recoverable panic. |
| `find(needle: String)` | `i64` byte offset of the first exact match, or `-1`. An empty needle returns `0`. |
| `contains(needle: String)` | `bool` indicating an exact substring match. An empty needle always matches. |
| `starts_with(prefix: String)` | `bool` indicating an exact prefix match. An empty prefix always matches. |
| `split(separator: String)` | `Array<String>` of non-overlapping literal matches; preserves leading, trailing, and adjacent empty fields. Empty separator splits at Unicode scalar boundaries, including leading/trailing empty fields: `"é".split("")` gives `["", "é", ""]`, and `"".split("")` gives `["", ""]`. |
| `trim()` | New `String` with Unicode whitespace removed from both ends. Interior whitespace is preserved. |
| `repeat(count: i64)` | New `String` repeated `count` times; zero produces `""`. Negative counts and size overflow raise recoverable panics. |
| `toString()` | The original `String`, unchanged. |

Matching is case-sensitive and performs no Unicode normalization. Embedded NUL
bytes are ordinary string content. Slicing copies bytes and does not retain the
original allocation. `"日本語".find("本")` returns `3`, and
`"日本語".slice(3, 6)` returns `"本"`. Slicing at byte `1` panics because it
would split a UTF-8 character. `split` returns a normal mutable array; an explicit
`Array<String>` type annotation requires `import std::collections::Array;`.

### Default floating-point display

`println(value)`, `value.toString()`, `format("{}", value)`, and
`f64::to_string(value)` omit the fractional suffix for integral `f64` values:
`2.0` displays as `2`, `0.0` as `0`, and negative zero as `-0`.
`f64::parse` round-trips these strings, preserving negative zero's sign.
Fractional values retain their round-tripping decimal representation;
`NaN`, `Infinity`, and `-Infinity` keep those spellings. Explicit precision
placeholders such as `{:.6f}` retain their requested decimal places.
Collections use the same default formatting for floating-point elements.
See [f64_integral_display.wi](f64_integral_display.wi) for all four entry points.
