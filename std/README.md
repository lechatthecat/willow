# Willow standard library

This page lists everything a Willow program can call without a dependency:
the `std` modules, the builtin types and their methods, and the namespace
functions. Complexity is stated per call; *n* is the size of the receiver and
*m* the size of an argument. "Expected" bounds assume hashing; "amortized"
bounds average over a sequence of calls.

Map and set bounds count hash-table operations; ordered-map bounds count key
comparisons. String keys also incur the cost of hashing or comparing their
bytes, so their length is an independent input dimension. Costs exclude later
garbage collection of discarded storage.

`import std::<module>::{Item, ...}` brings types into scope. Function modules
(`env`, `fs`, `net`, `parallel`, `path`, `text`, `time`) can be called as
`module::function(...)` without an import, or through an alias
(`import std::path as p;`). A misspelled module or item is a compile error
(E2006/E2007) that suggests the nearest name.

## Overview

| Module | Provides | Example |
| --- | --- | --- |
| `std::collections` | `Array<T>`, `Map<K, V>`, `FrozenArray<T>`, `FrozenMap<K, V>` | [arrays](../example/arrays.wi), [frozen_map](../example/frozen_map.wi) |
| `std::set` | `HashSet<T>` | [hash_set](../example/hash_set.wi) |
| `std::sorted_map` | `SortedMap<K, V>` (ordered map) | [sorted_map](../example/sorted_map.wi) |
| `std::strings` | `StringBuilder` | [string_builder](../example/string_builder.wi) |
| `std::bytes` | `ByteBuffer`, `Utf8Error` | [byte_buffer](../example/byte_buffer.wi) |
| `std::json` | `Value`, `Member`, `Error`, `parse`, `serialize`, `get`, `at` | [json](../example/json.wi) |
| `std::option`, `std::result` | `Option<T>`, `Result<T, E>` (also in the prelude) | [option_result](../example/option_result.wi) |
| `std::io` | `println`, `print`, `eprintln` (also in the prelude) | |
| `std::path` | Lexical path operations | [path_ops](../example/path_ops.wi) |
| `std::env` | Program arguments, environment variables | [env_var](../example/env_var.wi) |
| `std::fs` | Files (synchronous and `_async`) | [file_io](../example/file_io.wi) |
| `std::net` | TCP sockets | [tcp_echo](../example/tcp_echo.wi) |
| `std::parallel` | Data-parallel `map` | [parallel_map](../example/parallel_map.wi) |
| `std::text` | `from_scalar` | |
| `std::time` | `now_ns` | |

Concurrency types (`Channel`, `Mutex`, `RwLock`, `TaskScope`,
`CancellationToken`, atomics, cells) are builtin and need no import; see
[below](#concurrency-types).

## Scalars and strings

### `i64`

| Call | Result | Notes |
| --- | --- | --- |
| `x.to_string()` | `String` | |
| `x.wrapping_add(y)`, `wrapping_sub`, `wrapping_mul`, `wrapping_neg()` | `i64` | Two's-complement wrap in every build mode |
| `x.checked_add(y)`, `checked_sub`, `checked_mul`, `checked_neg()` | `Option<i64>` | `None` on overflow |
| `i64::abs(x)`, `i64::min(a, b)`, `i64::max(a, b)`, `i64::clamp(x, lo, hi)` | `i64` | |
| `i64::parse(s)` | `Result<i64, ParseIntError>` | |

Plain `+ - *` overflow panics in debug builds and wraps with `--release`.

### `f64`

| Call | Result | Notes |
| --- | --- | --- |
| `x.to_string()`, `f64::to_string(x)` | `String` | Shortest round-trip form; `1e10` prints `10000000000` |
| `f64::abs`, `sqrt`, `floor`, `ceil`, `round` | `f64` | |
| `f64::min`, `f64::max`, `f64::clamp` | `f64` | |
| `f64::from_i64(i)` | `f64` | |
| `f64::trunc_to_i64(x)` | `i64` | |
| `f64::parse(s)` | `Result<f64, ParseFloatError>` | |

### `bool`

`b.to_string()` gives `true` / `false`.

### `String`

Strings are immutable UTF-8. Lengths and offsets count bytes.

| Method | Result | Cost |
| --- | --- | --- |
| `len()` | `i64` (bytes) | O(1) |
| `byte_at(i)` | `i64` | O(1) |
| `substring(start, end)`, `slice(start, end)` | `String` | O(end - start) |
| `find(needle)` | `Option<i64>` (byte offset) | O(n + m) |
| `contains(needle)`, `starts_with(p)`, `ends_with(p)` | `bool` | O(n + m) / O(m) |
| `split(separator)` | `Array<String>` | O(n) |
| `replace(from, to)` | `String` | O(n + output) |
| `trim()`, `trim_start()`, `trim_end()` | `String` | O(n) |
| `to_lower()`, `to_upper()` | `String` | O(n) |
| `repeat(count)` | `String` | O(output) |
| `to_string()` | `String` | O(1) |

`a + b` concatenates in O(len(a) + len(b)). To build text from many pieces,
use [`StringBuilder`](#stdstrings-stringbuilder) instead of repeated `+`.

`text::from_scalar(code)` returns the one-character string for a Unicode
scalar value and panics on a surrogate or a value above `0x10FFFF`.

## `std::collections`

### `Array<T>`

Growable, indexable sequence. `arr[i]` reads and `arr[i] = v` writes in O(1)
and panic when `i` is out of bounds.

`Array<u8>` and fixed arrays `[u8; N]` store one byte per element, plus
the array handle, buffer header, alignment and spare capacity. GC traces the
buffer owner but never interprets its bytes as pointers. Generic `Array<T>`
code specialized with `T = u8` retains this packed representation: passing,
returning, capturing or storing an array does not widen or copy its elements.
The compiler selects byte loads/stores statically; runtime allocation, growth
and copying share the width-aware array implementation. Other element layouts
and the array handle ABI are unchanged.

Use `concat` for an independent shallow concatenation and `content_equals`
for ordered scalar/string element equality (including empty arrays). These
methods also work in generic code; array `+`, `==` and `!=` operators remain
undefined. Floating element equality follows scalar equality, including NaN.
See [packed byte arrays](../example/packed_byte_arrays.wi).

| Method | Result | Cost |
| --- | --- | --- |
| `Array::new()`, `[a, b, c]` | `Array<T>` | O(1) / O(n) |
| `Array::with_len(n, value)` | `Array<T>` | O(n) |
| `len()` | `i64` | O(1) |
| `push(v)` | | Amortized O(1) (capacity doubles) |
| `pop()` | `T` | O(1); panics when empty |
| `insert(i, v)`, `remove(i)` | / `T` | O(n - i) |
| `swap(i, j)` | | O(1) |
| `fill(v)` | | O(n) |
| `slice(start, end)` | `Array<T>` (copy) | O(end - start) |
| `concat(other)` | `Array<T>` (copy) | O(n + m) |
| `content_equals(other)` | `bool` | O(n) worst case, O(1) for different lengths; scalar and `String` elements |
| `index_of(v)` | `Option<i64>` | O(n); scalar and `String` elements |
| `contains(v)` | `bool` | O(n); scalar and `String` elements |
| `sort()` | | O(n log n); integer and `f64` elements |
| `sort_by(less)` | | O(n log n) comparisons |
| `join(separator)` | `String` | O(total bytes); `String` elements |
| `to_string()` | `String` | O(total); `[1, 2]`, strings quoted |
| `freeze()` | `FrozenArray<T>` | O(n) copy |

### `Map<K, V>`

Hash map. Keys are `i64`, `f64`, `bool` or `String`; `0.0` and `-0.0` are
the same key. Passing a NaN key to any operation (`insert`, `get`,
`contains`, `remove`) panics with `NaN cannot be used as a Map key`.

| Method | Result | Cost |
| --- | --- | --- |
| `Map::new()` | `Map<K, V>` | O(1) |
| `insert(k, v)` | | Expected O(1), amortized over growth |
| `get(k)` | `Option<V>` | Expected O(1) |
| `contains(k)` | `bool` | Expected O(1) |
| `remove(k)` | `Option<V>` | Expected O(1) |
| `len()` | `i64` | O(1) |
| `keys()`, `values()`, `entries()` | `Array<K>`, `Array<V>`, `Array<(K, V)>` | O(n log n); ascending key order |
| `to_string()` | `String` | O(n log n); `{a: 1, b: 2}` |
| `freeze()` | `FrozenMap<K, V>` | O(n) |

### `FrozenArray<T>`, `FrozenMap<K, V>`

Immutable snapshots that may be shared across tasks. `FrozenArray` offers
`len()` and indexing; `FrozenMap` offers `get`, `contains` and `len`, with the
same costs as `Map`.

## `std::set`: `HashSet<T>`

```willow
import std::set::HashSet;

let seen: HashSet<String> = new HashSet();
seen.insert("a");                   // true: newly added
let s = HashSet<i64>::from_array([3, 1, 3]);
```

`T` is `i64`, `f64`, `bool` or `String`. Elements live in a dense array with a
hash index, so iteration is proportional to the size, not the capacity.
NaN is never an element: `insert(NaN)` (and `from_array` with a NaN) panics
with `HashSet element must not be NaN`, while `contains(NaN)` and
`remove(NaN)` return `false`, the same rule as `SortedMap`.

| Method | Result | Cost |
| --- | --- | --- |
| `new HashSet()`, `HashSet<T>::from_array(values)` | `HashSet<T>` | O(1) / expected O(m) |
| `insert(v)` | `bool` (`true` when added) | Expected O(1), amortized |
| `contains(v)` | `bool` | Expected O(1) |
| `remove(v)` | `bool` (`true` when present) | Expected O(1) |
| `len()`, `is_empty()` | `i64`, `bool` | O(1) |
| `clear()` | | O(1) |
| `values()` | `Array<T>` (snapshot) | O(n); internal order, which changes after a removal |
| `union(o)`, `intersection(o)`, `difference(o)` | `HashSet<T>` | Expected O(n + m) / O(n) / O(n) |
| `is_subset(o)` | `bool` | Expected O(n) |
| `copy()` | `HashSet<T>` | Expected O(n) |
| `to_string()` | `String` | O(n log n); ascending, `{1, 2, 3}` |

## `std::sorted_map`: `SortedMap<K, V>`

```willow
import std::sorted_map::SortedMap;

let scores: SortedMap<String, i64> = new SortedMap();
scores.insert("bob", 85);
for name in scores.keys() { ... }   // ascending
```

An AVL tree ordered by key. `K` is `i64`, `f64` or `String` (`String` keys
compare by bytes). Inserting a NaN key panics; looking one up
(`get`, `contains`, `remove`, `floor_key`, `ceiling_key`) finds nothing, and
`range_keys` with a NaN bound is empty.

| Method | Result | Cost |
| --- | --- | --- |
| `new SortedMap()` | `SortedMap<K, V>` | O(1) |
| `insert(k, v)` | | Amortized O(log n); replaces an existing value |
| `get(k)` | `Option<V>` | O(log n) |
| `contains(k)` | `bool` | O(log n) |
| `remove(k)` | `Option<V>` | O(log n) |
| `len()`, `is_empty()` | `i64`, `bool` | O(1) |
| `clear()` | | O(1) |
| `first_key()`, `last_key()` | `Option<K>` | O(log n) |
| `floor_key(k)` / `ceiling_key(k)` | `Option<K>` (largest ≤ k / smallest ≥ k) | O(log n) |
| `range_keys(from, to)` | `Array<K>`: keys in `[from, to)` | O(log n + m) |
| `keys()`, `values()`, `entries()` | arrays in key order | O(n) |

`SortedMap` has no `to_string()`; format `entries()` instead.

## `std::strings`: `StringBuilder`

```willow
import std::strings::StringBuilder;

let out = new StringBuilder();
for i in 0..3 { out.append_i64(i); out.append(", "); }
println(out.to_string());
```

| Method | Result | Cost |
| --- | --- | --- |
| `new StringBuilder()`, `StringBuilder::with_text(s)` | `StringBuilder` | O(1) |
| `append(s)`, `append_line(s)` | | Amortized O(1) (the piece is kept, not copied) |
| `append_i64(x)`, `append_f64(x)`, `append_bool(b)` | | Amortized O(1) plus the formatting |
| `append_scalar(code)` | | Amortized O(1); panics like `text::from_scalar` |
| `len()`, `is_empty()` | `i64` (bytes), `bool` | O(1) |
| `clear()` | | O(1) |
| `to_string()` | `String` | O(total bytes) when anything was appended since the last call; O(1) otherwise |

Each `to_string()` joins the cached text and the pending pieces into one new
string and keeps it, so a build of *k* total bytes that calls `to_string()`
once costs O(k) however many pieces it has, and repeated calls with no append
in between copy nothing. Calling `to_string()` after every append copies the
whole text each time, because strings are immutable.

## `std::bytes`: `ByteBuffer`, `Utf8Error`

```willow
import std::bytes::ByteBuffer;

let b = ByteBuffer::from_string("héllo");
b.write_i64_be(2026);
match b.slice(0, 6).decode_utf8() { Ok(t) => println(t), Err(e) => println(e.position) }
```

A growable buffer of bytes, each an `i64` in `0..=255`. It interoperates with
the `Array<i64>` byte arrays of `std::net`. Storing a value outside `0..=255`
panics with `byte value must be in 0..=255`; reading integers past the end
panics with `byte buffer read out of bounds`.

| Method | Result | Cost |
| --- | --- | --- |
| `new ByteBuffer()` | `ByteBuffer` | O(1) |
| `ByteBuffer::from_array(bytes)` | `ByteBuffer` (validated copy) | O(m) |
| `ByteBuffer::from_string(s)` | `ByteBuffer` (UTF-8 bytes) | O(m) |
| `len()`, `is_empty()` | `i64`, `bool` | O(1) |
| `get(i)`, `set(i, byte)` | `i64` / | O(1) |
| `push(byte)` | | Amortized O(1) |
| `extend(bytes)`, `write_string(s)` | | Amortized O(m) |
| `write_i64_le(x)`, `write_i64_be(x)` | | Amortized O(1) (8 bytes) |
| `read_i64_le(offset)`, `read_i64_be(offset)` | `i64` | O(1) |
| `slice(start, end)` | `ByteBuffer` (copy) | O(end - start) |
| `to_array()` | `Array<i64>` (copy) | O(n) |
| `clear()` | | O(1) |
| `decode_utf8()` | `Result<String, Utf8Error>` | O(n) |
| `to_string()` | `String` | O(n); `[104, 195, 169]` |

`decode_utf8` is strict: overlong forms, surrogates, values above `U+10FFFF`
and truncated sequences fail with `Utf8Error.position`, the byte offset where
the invalid sequence starts.

## `std::json`

`json::parse(text) -> Result<Value, Error>` and
`json::serialize(value) -> Result<String, Error>` convert between text and the
`Value` enum (`Null`, `Bool`, `Number` (exact spelling as `String`), `String`, `Array`, `Object`). `json::get`
and `json::at` look up an object member (O(member count), last duplicate
wins) or a list element. Parsing and serialization are linear in the input
and output size. See
[json.wi](../example/json.wi).

## `std::option`, `std::result`

| Method | `Option<T>` | `Result<T, E>` | Notes |
| --- | --- | --- | --- |
| `is_some()`, `is_none()` | yes | | |
| `is_ok()`, `is_err()` | | yes | |
| `unwrap()` | yes | yes | Panics on `None` / `Err` |
| `unwrap_err()` | | yes | Panics on `Ok` |
| `expect(message)` | yes | yes | Panics with `message` on `None` / `Err` |
| `unwrap_or(default)` | yes | yes | |
| `map(f)` | yes | yes | Transforms the `Some` / `Ok` value |
| `map_err(f)` | | yes | Transforms the `Err` value |
| `and_then(f)` | yes | yes | `f` returns the same family |
| `or_else(f)` | yes | yes | `f` produces the fallback |

`expr?` returns early from a function on `None` / `Err`.

## Global functions

These are callable anywhere without an import.

| Function | Result | Notes |
| --- | --- | --- |
| `panic(message)`, `panic(spec, args...)` | never returns | Raises a recoverable panic; the variadic form formats `{}` placeholders |
| `format(spec, args...)` | `String` | Replaces each `{}` with the next argument's text |
| `recover()` | `Option<PanicInfo>` | In a `defer`, stops an unwinding panic; `PanicInfo` has `message`, `file`, `line`, `column` |
| `pow(base, exp)`, `powf(base, exp)` | `f64` | Both take `f64` arguments; usable as `fn` values |
| `gc_collect()`, `gc_minor_collect()` | | Force a full / minor collection |
| `gc_task_frame_diagnostics()` | | Prints one snapshot of task-frame lifecycle counters to stdout |

Runtime diagnostics counters also exist as zero-argument functions returning
`i64`: `gc_allocated_bytes`, `gc_total_allocated_bytes`, `gc_minor_collections`,
`gc_pause_count`, `gc_pause_total_ns`, `gc_pause_max_ns` and the other `gc_*`
and `sched_*` counters (`sched_steal_attempts`, ...). They are meant for
measurements and tests, not program logic.

## `std::io`

`println(x)`, `print(x)`, `eprintln(x)` print a scalar, a `String`, or a
value with `to_string()`.

## `std::path`

Lexical operations; none of them reads the file system, so they are allowed
in `@effects(no_io)` functions. Separators, Windows drive and UNC prefixes,
and roots follow the platform the program runs on.

| Function | Result | Notes |
| --- | --- | --- |
| `path::join(base, child)` | `String` | An absolute `child` replaces `base`; an empty `base` adds no separator |
| `path::parent(p)` | `Option<String>` | `None` for a root or empty path; `Some("")` for a bare name |
| `path::file_name(p)` | `Option<String>` | `None` when `p` ends in `..` or is a root |
| `path::extension(p)` | `Option<String>` | After the last `.`; `None` for `.bashrc` |
| `path::file_stem(p)` | `Option<String>` | File name without its extension |
| `path::is_absolute(p)` | `bool` | On Windows needs a prefix and a root (`C:\x`) |

Component queries take O(len(p)); joining takes O(len(base) + len(child)).

## `std::env`

| Function | Result | Notes |
| --- | --- | --- |
| `env::args()` | `Array<String>` | Arguments after the program name |
| `env::arg(i)` | `Option<String>` | |
| `env::args_len()` | `i64` | |
| `env::program_name()` | `String` | |
| `env::var(name)` | `Option<String>` | `None` when unset, when the value is not valid Unicode, or when `name` is empty or contains `=` or NUL |

These read process state, so they count as I/O for `@effects(no_io)`.

## `std::fs`

| Function | Result |
| --- | --- |
| `fs::read_to_string(path)` | `Result<String, IoError>` |
| `fs::write_string(path, text)` | `Result<(), IoError>` |
| `fs::exists(path)` | `bool` |
| `fs::remove_file(path)` | `Result<(), IoError>` |
| `fs::temp_path(prefix)` | `String`: a process-unique path in the OS temp directory |

Each has an `_async` form (`read_to_string_async`, ...) that returns a `Task`
and runs the operation on a bounded blocking pool instead of a scheduler
worker.

## `std::net`

TCP with numeric `IP:port` addresses (no DNS lookup).

| Function | Result |
| --- | --- |
| `net::bind(addr)` | `Result<TcpListener, IoError>` |
| `net::local_addr(listener)` | `Result<String, IoError>` |
| `net::accept_async(listener)`, `net::connect_async(addr)` | `Task<Result<TcpStream, IoError>>` |
| `net::read_async(stream, max)`, `net::write_async(stream, text)` | `Task<Result<String, IoError>>`, `Task<Result<(), IoError>>` |
| `net::read_bytes_async(stream, max)`, `net::write_bytes_async(stream, bytes)` | `Task<Result<Array<i64>, IoError>>`, `Task<Result<(), IoError>>` |
| `net::peer_addr(stream)`, `net::shutdown(stream)` | `Result<String, IoError>`, `Result<(), IoError>` |

## `std::parallel`, `std::time`

- `parallel::map(values: FrozenArray<i64>, f: fn(i64) -> i64) -> Task<Array<i64>>`
  applies `f` to every element on the worker pool.
- `time::now_ns() -> i64` is a process-local monotonic clock in nanoseconds,
  not a wall-clock timestamp.

## Concurrency types

| Type | Construct | Methods |
| --- | --- | --- |
| `Channel<T>` | `Channel<T>::new()`, `Channel<T>::with_capacity(n)` | `send(v)`, `recv()`, `recv_opt()`, `close()` |
| `Task<T>`, `JoinHandle<T>` (same type) | calling an `async fn` | `await`, `cancel()`, `is_cancelled()`, `result()` |
| `TaskScope` | `TaskScope::new()` | `add(task)`, `child()`, `cancel()`, `is_cancelled()`, `finish()` |
| `CancellationToken` | `CancellationToken::new()` | `cancel()`, `is_cancelled()`, `child()`, `attach(task)` |
| `Mutex<T>`, `RwLock<T>` | `Mutex::new(v)`, `RwLock::new(v)` | `lock m as mut v { ... }`, `lock read` / `lock write` |
| `AtomicI64` | `AtomicI64::new(x)` | `load`, `store`, `swap`, `add`, `sub`, `compare_exchange` |
| `AtomicBool` | `AtomicBool::new(b)` | `load`, `store`, `swap`, `compare_exchange` |
| `BlockingCell<T>`, `BlockingRwCell<T>` | `::new(v)` | `get` / `set`, `read` / `write` |

See the [concurrency section of the README](../README.md#concurrency-checks-what-the-compiler-guarantees)
for the rules the compiler checks around these types.
