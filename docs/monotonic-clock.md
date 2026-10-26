# Monotonic time

`import std::time;` exposes `time::now_ns() -> i64`. Module aliases work too: `import std::time as clock;` followed by
`clock::now_ns()`. Use the qualified call form; importing a function item does
not introduce a callable bare function name in the current std import system.

```willow
import std::time;

async fn main() {
    let start = time::now_ns();
    await sleep(10);
    println(time::now_ns() - start);
}
```

Values are nonnegative nanoseconds from a process-local epoch initialized on
first use of the runtime timer/clock. All threads and tasks share that epoch.
Subtract an earlier reading from a later one to measure elapsed time, including
waiting. For a time budget, compare that difference with the budget in nanoseconds.
The clock works in synchronous functions too and does not require a scheduler.

This is elapsed monotonic time, not Unix time, calendar time, or CPU time. Values
must not be compared across processes or persisted as timestamps. Successive
readings can be equal: nanosecond units do not promise nanosecond resolution.
System suspend behavior depends on the operating system. Wall-clock adjustments
do not change the epoch. The runtime uses Rust `std::time::Instant` on Linux,
macOS, and Windows, with no platform-specific compiler lowering.

Readings saturate at `i64::MAX` (approximately 292 years after the epoch), never
wrap into negative numbers. Elapsed measurements stop advancing at saturation.
Avoid adding arbitrary budgets to a timestamp, since ordinary integer arithmetic
can overflow; subtract timestamps first. A read allocates no heap storage and
performs constant work; N reads require O(N) work and O(1) retained storage.

Runtime ABI: `willow_time_now_ns`, C calling convention, no arguments, signed
64-bit return, no GC allocation or suspension. Calls must remain observable;
the compiler must not fold or merge consecutive clock reads.

Runnable example: [`monotonic_clock.wi`](../example/monotonic_clock.wi).
