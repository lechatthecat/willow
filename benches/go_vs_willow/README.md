# Willow vs Go vs Java benchmarks

Latest full measurement (three languages, 2026-10-25, commit `453fc6c`):
[`MEASUREMENT_2026-10-25.md`](MEASUREMENT_2026-10-25.md), raw trials in
[`MEASUREMENT_2026-10-25.jsonl`](MEASUREMENT_2026-10-25.jsonl).
Run everything and regenerate a dated report with:

```sh
python3 benches/go_vs_willow/compare.py run \
  --label <label> --trials 5 --workers 8 --languages willow,go,java \
  --baseline benches/go_vs_willow/MEASUREMENT_2026-10-25.jsonl
```

`compare.py run` builds the release compiler and the Go/Java benchmark programs,
runs `run_sync.py` (all cases) and `run.py` (the seven standard scheduler cases)
for the selected languages, writes `MEASUREMENT_<label>.jsonl` with every trial
record, and renders `MEASUREMENT_<label>.md` with median tables, Willow/other
ratios and optional deltas against a baseline JSONL. `compare.py report <jsonl>`
renders Markdown to stdout without running benchmarks or creating/changing a
report file, including when `--baseline` is supplied. To save a report, pass
`--out <path>` explicitly; this overwrites that path if it already exists.
For example, `compare.py report <jsonl> --out /tmp/preview.md` saves a preview.
`run_sync.py` and `run.py` accept the same
`--languages willow,go,java` flag (default `willow,go`); a JSONL from a
single-language run can be concatenated with an earlier run's JSONL and rendered
with `report`.

## Java equivalents

`sync/java/Bench.java` and `java/Bench.java` mirror the Go programs case by case
and print the same markers and result lines. Goroutines/Willow tasks map to
virtual threads (every benchmark body, including the coordinating side, runs on a
virtual thread); channels map to `ArrayBlockingQueue` with the same capacities;
`runtime.Gosched()` maps to `Thread.yield()`; `runtime.KeepAlive` maps to
`Reference.reachabilityFence`; the growable `[]int64` maps to an unboxed
`long[]` list rather than a boxing `ArrayList<Long>`. GC counters come from
`GarbageCollectorMXBean` (cycles and cumulative collector time, 1 ms resolution)
and `com.sun.management.ThreadMXBean.getTotalThreadAllocatedBytes()`; metric
names use the `JAVA_` prefix. The JVM runs with default GC/JIT settings plus
`-XX:ActiveProcessorCount=<workers>` and
`-Djdk.virtualThreadScheduler.parallelism=<workers>`; whole-process sync timing
therefore includes JVM startup and JIT warm-up, as the Qiita method intends.
`javatool.py` finds `javac`, or falls back to `java -m jdk.compiler/...` on
JRE-only images such as Ubuntu's `openjdk-26-jre-headless`.

## Scheduler benchmarks

This suite compares equivalent scheduler operations in Willow, Go and Java (virtual threads).
It uses 8 workers (`WILLOW_WORKERS=8`, `GOMAXPROCS=8`) to match the machine's
physical core count. RSS is read externally from `/proc/<pid>/status`; benchmark
timings use the harness's monotonic clock and markers printed by each process.
CPU time is the phase delta of process `utime + stime` from `/proc/<pid>/stat`.
On the current Linux host that value has 10 ms resolution, so it is only an
approximation for very short phases.

The priority cases are:

- `idle_spawn`: eager task/goroutine creation followed by channel parking. Reports
  spawn time, RSS delta, RSS bytes/task, and each runtime's cumulative allocation
  counter.
- `wake_fanout`: N parked workers on N private channels, N channel sends, and
  fan-in of N completion messages. This measures a burst of wake-to-completion
  operations rather than a channel-close broadcast.
- `yield_switch`: N workers each cooperatively yield R times.
- `ping_pong`: one million channel round trips between two tasks/goroutines.
- `gc_scheduler`: N workers allocate one object, keep it live across a yield,
  and repeat R times. Willow reports its public allocation/collection counters;
  Go reports `runtime.MemStats`, including pause time. Willow currently exposes
  neither GC pause duration nor GC CPU percentage through its language ABI.
- `channel_select_fan_in`: four producers each send tokens on their own
  capacity-1 channel; the consumer `select`s across all four until it has taken
  N tokens, then cancels the producers and joins them inside the timed phase
  (Willow `task.cancel()` + `result()`, Go `close(done)` + `WaitGroup`). Java has
  no multi-queue select, so its four producers share one `ArrayBlockingQueue(4)`
  and are stopped with `interrupt()` + `join()`; treat that column as a
  fan-in-queue figure, not a select figure.
- `spawn_join_tree`: arguments are `fanout depth`. Every node spawns `fanout`
  children and joins all of them (Willow `TaskScope`, Go `WaitGroup`, Java one
  `newVirtualThreadPerTaskExecutor()` per node); `10 5` spawns 111,110 tasks
  and prints the 100,000 leaves.

Build and run, for example:

```sh
python3 benches/go_vs_willow/run.py idle_spawn 100000 --trials 5
python3 benches/go_vs_willow/run.py wake_fanout 100000 --trials 5 --no-build
python3 benches/go_vs_willow/run.py yield_switch 100000 100 --trials 5 --no-build
python3 benches/go_vs_willow/run.py ping_pong 1000000 --trials 5 --no-build
python3 benches/go_vs_willow/run.py gc_scheduler 10000 100 --trials 5 --no-build
python3 benches/go_vs_willow/run.py channel_select_fan_in 1000000 --trials 5 --no-build
python3 benches/go_vs_willow/run.py spawn_join_tree 10 5 --trials 5 --no-build
```

Do not run the 1M-task case until the 100k-task RSS result confirms that it is
safe on the test machine. These numbers are machine-specific; avoid comparing
runs made under different worker counts, power profiles, or background load.

## Synchronous benchmarks

`run_sync.py` follows the whole-program method in the
[referenced Qiita article](https://qiita.com/hanaata/items/c91788bcac2a40f1bb05)
and compares seventeen equivalent, single-main-thread workloads using
release-built Willow and Go binaries and the Java classes (`--languages`):

- `leibniz_pow`: the Qiita article's Leibniz-series expression, including terms
  0 through 100,000,000 and computing the alternating sign with exponentiation.
  Willow's pow returns +/-1 for a unit-magnitude base without multiplying (as
  fdlibm does; Go's `math.Pow` does not), so this case mostly measures that
  shortcut, not general pow throughput.
- `leibniz_pow_nonunit`: the same loop with base `-1.0000001`, so every pow
  call performs integral-exponent work in all three languages.
- `leibniz_reduced`: the same terms with exponentiation strength-reduced to a
  sign flip, isolating loop and floating-point arithmetic throughput.
- `fibonacci`: naive recursive `fib(40)`.
- `array_sum`: append five million `i64` values, then traverse and sum them.
- `linked_list`: allocate and traverse a one-million-node linked list.
- `closure_call`: call 1,024 capturing closures through an array, so every call
  is indirect (Go `func` values, Java `LongUnaryOperator`).
- `string_build`: build two million short strings from two integer-to-decimal
  conversions and three concatenations each.

Phase-timed cases print `START`/`END` markers and report only the time between
them (process startup excluded); the `array_*_only` cases isolate one array
operation each, and four more target the allocator, the map and dispatch:

- `object_churn`: ten million short-lived objects written into a 1,024-slot
  ring (the store defeats escape analysis; only 1,024 stay live), with each
  runtime's GC counters printed after the result — Willow minor/major/promoted
  counters, Go `MemStats`, JVM `GarbageCollectorMXBean`.
- `array_literal_churn`: the array analogue of `object_churn` — ten million
  three-element `i64` array literals written into a 1,024-slot ring, with the
  same GC counters.
- `map_lookup_insert`: one million inserts of strided `i64` keys into
  `Map<i64,i64>` / `map[int64]int64` / `HashMap<Long,Long>` (Java boxes), then
  two million lookups of which half miss.
- `virtual_dispatch`: 1,000 interface values over four classes, called 20,000
  rounds through one megamorphic call site.

The harness validates every result, alternates language launch order, and uses
five fresh-process trials by default. Python's monotonic clock measures wall
time; GNU `time` supplies process user/system time and peak RSS. All runtimes
receive a worker limit of eight for consistency with the scheduler suite,
although the benchmark body itself is synchronous.

```sh
python3 benches/go_vs_willow/run_sync.py
python3 benches/go_vs_willow/run_sync.py fibonacci array_sum --trials 3 --no-build
```

Build time is intentionally excluded. The reported wall time includes process
startup and shutdown, as in the linked Qiita article.
