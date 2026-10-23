# Windows WSAPoll array reuse audit — willow-r52c.18

Date: 2026-10-24. Scope: `crates/willow_runtime/src/netpoll.rs`.

## Change and ownership

Windows fills one retained WSAPOLLFD vector directly from the live descriptor
index. The old intermediate `(fd, interest)` vector and second allocation are
removed. Each wait takes ownership of its vector under GLOBAL_NETPOLL, releases
the mutex before WSAPoll, then returns an empty vector for reuse. Concurrent
waits own independent vectors; the largest returned buffer is retained. No new
lock spans an OS wait. The UDP wake receiver is Arc-owned so a snapshot keeps
its native socket alive without a per-wait handle duplication syscall.

A snapshot captures the next registration sequence. Native results only visit
live registrations older than that sequence. Removal, task cancellation,
interest replacement, and descriptor reuse cannot transfer an old readiness
result to a newly registered waiter, including the same task/descriptor tuple.
Unchanged shared-descriptor waiters retain FIFO and interest filtering. Explicit
queued wake tokens keep their existing dispatch semantics. Linux epoll and Apple
kqueue construction, waiting and event decoding are unchanged; their common
ready lookup uses the same ordered index with an unbounded-in-practice cutoff.

## Complexity and deterministic evidence

Let N be live distinct descriptors, R live registrations, W concurrent waits,
F wait frequency, K registrations on a ready descriptor, and H the maximum
snapshot size retained. Filling and scanning a WSAPoll array costs O(N), with
N+1 entries including the wake socket. This is required by the WSAPoll flat-array
interface; this change does not claim to remove its linear kernel scan. The
existing shrinking descriptor HashMap bounds iteration capacity after churn.
Across F waits the cost is O(F*N), rather than a purported constant-time poll.
The intermediate vector is eliminated and a serial steady-size wait performs
zero poll-array reallocations after warmup. Ready lookup costs O(log K + eligible
K), plus expected O(1) task dedup per visited registration; it does not scan all
R registrations per event. Each descriptor appears once in the snapshot.

Portable tests use production fill and cutoff helpers. For N=1, 1,000, 10,000,
and 20,000, each fill produces exactly 2, 1,001, 10,001, and 20,001 entries.
Four repeat fills at each size assert identical allocation pointer and capacity,
and reset every revents field. Fragmentation removes 19,999 descriptors, then
checks a two-entry snapshot and read/write interest replacement/merging. These
are deterministic entry/storage assertions, not wall-clock complexity estimates.

Memory is O(R + W*H) in the worst case including registration indexes and
in-flight snapshots, with one idle high-water buffer retained. Concurrent waits
may allocate when the shared idle buffer is in use; this is intentional to avoid
serializing OS waits. An idle buffer is not shrunk after a spike, trading retained
memory for avoiding allocation churn. Ready-result and task vectors still
allocate. No compiler, generated code, runtime ABI, or build scheduling changes.

## Validation coverage

The focused netpoll tests cover index consistency, large registration sets,
fragmented buckets, FIFO, interest filtering, task dedup, replacement, rollback,
deregistration, shared sockets, explicit wake, Linux native readiness, purge,
platform-interest resync, buffer reuse and snapshot sequence rejection.

The Windows-only native test additionally exercises TCP writable/readable
readiness, cancellation between snapshot and dispatch, and disconnect after
draining the payload. It is cross-compiled by Clippy but not executed here.
Portable sequence tests exercise read, write and combined/error readiness,
same-task replacement, newly added shared waiters, retained old waiters and
complete removal of snapshot-era registrations. Real OS handle reuse timing
and concurrent scheduler races are not claimed to have been executed.

## Reproducible Windows measurement

Run on Windows:

```text
cargo test -p willow_runtime windows_wsapoll_scale_measurement --release -- --ignored --nocapture
cargo test -p willow_runtime windows_snapshot_native_read_write_disconnect -- --nocapture
```

The ignored measurement uses 1, 1,000, 10,000 and 10,001 bound loopback UDP
sockets. For each count it compares fresh versus retained poll-array allocation
at one send per 1, 10 and 100 scheduled iterations over 1,000 iterations.
Each send remains pending until readiness followed by a successful nonblocking
receive. Missing readiness and WouldBlock cause additional zero-timeout polls,
up to a five-second deadline per datagram; other receive errors fail the run.
All additional polls count toward reported throughput. It prints actual poll
count and poll
throughput, observed wakes, send-to-receipt p50/p99 latency and process CPU
nanoseconds from GetProcessTimes. This isolates allocation reuse within the new
implementation, not a complete old-revision comparison. Setup is outside the
timed region. CPU includes sender work and has Windows process-time resolution;
short runs may report zero CPU. It measures scheduled send ratios (delivery retries change the actual
polls-per-send ratio), not a
fixed external arrival rate, TCP throughput, or scheduler end-to-end latency.

No Windows performance numbers were obtained on this Linux host. No IOCP
migration is justified by the available evidence. Foreign native execution and
CI evidence are explicitly not required by the current user instruction; this
is an execution limitation, not a claim that the original Windows measurement
has run. No CI was queried and no full local suite/example audit was run.

## Local results

- `cargo fmt --all -- --check`: passed.
- `cargo test -p willow_runtime netpoll::tests::`: 16 passed, 1,857 filtered out.
- `cargo clippy --workspace --all-targets --target <target> -- -D warnings`:
  passed for x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc,
  aarch64-apple-darwin, x86_64-apple-darwin.
- `git diff --check`: passed.
- Native execution: Linux only. Windows and both macOS architectures unexecuted.
- Review provenance: the previous approval came from an independent reviewer,
  not the user. A subsequent audit-document edit invalidated that snapshot;
  the final cumulative change is being submitted for fresh independent review.
  Foreign native execution remains unperformed under the explicit user exception.
  Nothing staged, committed or pushed. Ticket status is maintained in Beads;
  approval/closure must not modify this snapshot-covered document.

## Review repair: delayed UDP delivery

The measurement no longer assumes send completion implies readability. It keeps
one timestamped send pending, waits for actual readiness and receipt, handles
WouldBlock by retrying, and measures latency only on successful receipt. Poll
throughput uses all attempted polls, including delivery retries. A missing
packet produces a bounded timeout instead of an immediate nonblocking unwrap
failure. Only the test harness changed in this repair; production netpoll paths
remain unchanged.

Deterministic injected clock/operation tests cover absent readiness, readiness
followed by WouldBlock, later successful receipt, exact retry-poll accounting,
receipt latency, deadline exhaustion, and propagation of non-retryable errors.
Per-send work is O(P*N) for P attempted polls and N sockets; pending state is
O(1), latency samples O(S) for S sends. No unbounded pending queue is introduced.
The deadline bounds elapsed waiting, not a guaranteed fixed poll count.

Repair validation: `cargo test -p willow_runtime windows_measurement_` passed
2 tests (1,873 filtered out) on Linux. Formatting and diff whitespace checks
passed. Workspace all-target Clippy passed again for all four targets listed
above. The prior 16-test netpoll result predates this harness-only repair;
production code was not modified. Windows/macOS native execution remains
unperformed, and no full suite or CI query was run.

## Recovery validation of the cumulative change

The final cumulative source diff was inspected again after the supervision
upgrade. No runtime or test implementation changes were needed in recovery;
the outstanding correction was review provenance and snapshot status wording.
`cargo test -p willow_runtime netpoll::tests::` now passes all 18 focused tests
(1,857 filtered out), including the delivery-retry repair. `cargo fmt --all --
--check` and workspace all-target Clippy for Linux x86_64, Windows MSVC x86_64,
Apple arm64 and Apple x86_64 passed again. The complexity, ownership and
execution limitations described above still apply. No foreign native run,
full local suite, or CI query was performed.
