# Willow vs Go scheduler benchmarks

This suite compares equivalent scheduler operations with Willow and Go 1.27.0.
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

Build and run, for example:

```sh
python3 benches/go_vs_willow/run.py idle_spawn 100000 --trials 5
python3 benches/go_vs_willow/run.py wake_fanout 100000 --trials 5 --no-build
python3 benches/go_vs_willow/run.py yield_switch 100000 100 --trials 5 --no-build
python3 benches/go_vs_willow/run.py ping_pong 1000000 --trials 5 --no-build
python3 benches/go_vs_willow/run.py gc_scheduler 10000 100 --trials 5 --no-build
```

Do not run the 1M-task case until the 100k-task RSS result confirms that it is
safe on the test machine. These numbers are machine-specific; avoid comparing
runs made under different worker counts, power profiles, or background load.
