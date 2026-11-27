# Runtime and compiler tuning settings

`willow_abi::settings` owns environment reads and parsing for runtime and compiler
tuning. Both consumers already depend on this crate; the module defines no ABI
symbols or layouts. Diagnostic output switches remain in `diagnostic_settings`;
paths, tool locations, application environment access and test-harness controls
remain with their owners.

## Invalid-value policy

Reject invalid overrides according to the setting's declared policy class.
Advisory values fall back to their documented default; explicit resource
constraints fail closed; verification budgets preserve the invalid sentinel
which produces the verifier's existing diagnostic. A switch is an exact
predicate over its entire value domain, not a general boolean parser.
These classes are intentional: silently discarding a malformed hard memory
limit removes a requested constraint, while rejecting an advisory scheduler
hint would unnecessarily prevent startup.

All numeric classes treat absent and non-Unicode values as unset. Unsigned
integer parsing accepts an optional `+`; negatives, empty strings, non-digits
and integer overflow are invalid. Only the rows marked “trim” accept surrounding
whitespace. Zero and range handling are part of each class below. No values,
defaults, diagnostics or sampling lifetimes changed in this consolidation.

| Class | Variables (prefix `WILLOW_`) | Valid values | Unset / invalid override |
| --- | --- | --- | --- |
| Advisory positive count (trim) | `WORKERS` | Positive `usize` | Detected parallelism, or 1 |
| Advisory positive count (trim) | `BLOCKING_THREADS`, `BLOCKING_QUEUE` | Positive `usize` | 4 threads; queue = threads × 16, saturating |
| Advisory positive duration/count (trim) | `TASK_BUDGET`, `TIME_QUANTUM_MS` | Positive `u64` | `u64::MAX` polls; 10 ms |
| Advisory bounded count | `GC_REMARK_MAX_SLOTS` | 1–1,000,000 | 4,096 |
| Advisory bounded count | `GC_REMARK_MAX_BYTES` | 1–16,777,216 | 1,048,576 |
| Advisory bounded count | `GC_REMARK_MAX_WORK_NS` | 1–100,000,000 | 1,000,000 |
| Advisory bounded count | `GC_SATB_BUFFER_ENTRIES` | 1–65,536 | 256 |
| Advisory saturating count | `GC_MARK_THREADS` | Any `usize`; zero disables background markers; valid counts above 64 become 64 | Available parallelism minus one, clamped to 1–4; detection failure gives 1 |
| Advisory timeout (trim) | `GC_HANDSHAKE_TIMEOUT_SECS` | Any `u64`; zero disables the timeout | 120 seconds |
| Resource constraint | `GC_MEMORY_LIMIT`, `GC_NURSERY_BYTES` | Positive `usize` bytes | Unset: no hard limit / adaptive nursery. Invalid (including zero): panic with the existing setting-specific positive-byte-count message |
| Resource constraint | `GC_MEMORY_LIMIT_BYTES` | Any `u64` bytes; zero disables the target | Unset: zero. Invalid: panic with the existing nonnegative-byte-count message and parse error |
| Verification budget | `VERIFICATION_TIMEOUT_MS`, `VERIFICATION_RLIMIT` | Parsed `u64`, then checked by the verifier (1–60,000 ms; 1–1,000,000,000 resources) | Unset: 5,000 / 1,000,000. Invalid parse: zero, which the verifier rejects with its existing diagnostic |

| Switch | Predicate |
| --- | --- |
| `WILLOW_GC_CONCURRENT_MARK`, `WILLOW_GC_ASSIST`, `WILLOW_GC_PACER`, `WILLOW_PANIC_SNAPSHOT_REUSE` | Enabled unless the value is exactly `0`; unset, empty, `false`, whitespace and non-Unicode values enable it |
| `WILLOW_PANIC_EFFECTS` | Enabled unless exactly `0` or ASCII case-insensitive `false`; no trimming; unset/non-Unicode enables it |
| `WILLOW_ASYNC_FRAME_ALL` | Enabled by any Unicode value, including empty and `0`; unset/non-Unicode disables it |
| `WILLOW_QUERY_CACHE` | Enabled only by exactly `1`; all other values, including unset, disable it |

## The two memory limits

These names are retained for compatibility; they are independent controls and
neither aliases nor overrides the other.

| Variable | Scope and behavior | Zero / unset |
| --- | --- | --- |
| `WILLOW_GC_MEMORY_LIMIT` | **Hard reservation cap**, in bytes, on GC regions and TLAB reservations. Does not cap RSS, native task stacks or Rust containers. Enforced by allocation; it also bounds nursery policy. | Zero is invalid; unset means no hard cap |
| `WILLOW_GC_MEMORY_LIMIT_BYTES` | **Soft pressure target**, in bytes. The controller considers managed reservations plus native requested capacity, and independently process RSS. It adjusts collection goals and may enter relief rather than refusing every allocation beyond the target. Can be changed by `gc::willow_gc_set_memory_limit`. | Zero or unset disables the target |

Setting both applies both mechanisms. Changing the soft target through the runtime
API does not change the hard reservation cap. The runtime telemetry reports pressure, collection goals and relief decisions.

## Sampling and cost

The shared module performs only requested reads. Owners retain their sampling
points: preemption once per process; worker configuration, marker count and
handshake timeout at their existing runtime-owned initialization points; pool
configuration when a pool is created; collector constraints during construction
and reset; remark, SATB, assist and pacing at their existing policy boundaries.
Compiler switches remain read at their existing decision points.

No eager snapshot, new cache, per-object field or hot-path branch is introduced.
A single setting read retains its environment lookup plus linear parsing in the
value length, with constant auxiliary parser space. Unrelated variables are not
read. Existing lazy locks continue to keep environment lookup off the preemption
poll path.
