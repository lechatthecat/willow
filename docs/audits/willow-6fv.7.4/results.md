# M3 local measurements

Five independent processes per comparison cell. CPU timing is nanoseconds. Confidence intervals are deterministic bootstrap 95% intervals for the ratio of medians (10,000 resamples; seed 674). Five processes are a small sample; these are fixture-specific estimates.

## Profiling-disabled application comparison

| Workload | Markers | Throughput ratio (candidate/base) | 95% CI | Worst RSS ratio |
|---|---:|---:|---:|---:|
| alloc | 1 | 1.0148 | [0.9873, 1.0547] | 0.9891 |
| alloc | 4 | 1.0015 | [0.9901, 1.0137] | 0.9891 |
| alloc | 16 | 1.0142 | [0.9783, 1.0355] | 1.0298 |
| chain | 1 | 1.0287 | [0.9782, 1.0519] | 0.9850 |
| chain | 4 | 1.0173 | [0.9986, 1.0778] | 1.0414 |
| chain | 16 | 1.0065 | [1.0015, 1.0237] | 1.0398 |

These fixtures check instrumentation/regression risk. They do not prove a production notification optimization: the separately measured integrated prototype appears below. No generic standard-set claim is made.

## Mark-worker scaling (profiling enabled)

| Objects | Fanout | Markers | Median application ms | Median 95% CI ms | Peak RSS KiB |
|---:|---:|---:|---:|---:|---:|
| 4096 | 1 | 1 | 6.263 | [6.231, 8.452] | 3744 |
| 4096 | 1 | 2 | 7.474 | [6.329, 10.671] | 3692 |
| 4096 | 1 | 4 | 10.565 | [6.310, 24.273] | 3788 |
| 4096 | 1 | 8 | 9.132 | [6.568, 13.670] | 3880 |
| 4096 | 1 | 16 | 25.876 | [20.260, 45.754] | 3840 |
| 4096 | 4 | 1 | 15.159 | [12.172, 22.066] | 3960 |
| 4096 | 4 | 2 | 25.132 | [13.465, 26.638] | 3960 |
| 4096 | 4 | 4 | 109.885 | [81.321, 122.067] | 3964 |
| 4096 | 4 | 8 | 345.737 | [268.300, 350.652] | 3964 |
| 4096 | 4 | 16 | 313.211 | [304.885, 390.548] | 3964 |
| 32768 | 4 | 1 | 99.963 | [87.846, 123.287] | 7636 |
| 32768 | 4 | 2 | 121.021 | [110.039, 151.477] | 7796 |
| 32768 | 4 | 4 | 116.791 | [104.655, 128.898] | 7648 |
| 32768 | 4 | 8 | 127.716 | [105.732, 191.074] | 8120 |
| 32768 | 4 | 16 | 107.812 | [104.815, 118.958] | 8136 |
| 32768 | 16 | 1 | 86.979 | [86.454, 89.366] | 10184 |
| 32768 | 16 | 2 | 79.811 | [79.154, 85.366] | 10212 |
| 32768 | 16 | 4 | 96.975 | [84.203, 109.482] | 10332 |
| 32768 | 16 | 8 | 108.700 | [92.059, 123.336] | 10556 |
| 32768 | 16 | 16 | 119.158 | [106.419, 160.724] | 11036 |

One separately registered CPU-active mutator runs during 32 explicit collections in each process. Its own thread CPU/wall ratio is measured, not inferred from empty queues. Cycle p99 below is nearest-rank over 160 calls (five processes x 32); this includes collector completion, not just a global pause.

| Objects | Fanout | Markers | Cycle p99 ms | Median mutator CPU/wall | Median mutator operations/s |
|---:|---:|---:|---:|---:|---:|
| 4096 | 1 | 1 | 2.752 | 0.9257 | 2532835299 |
| 4096 | 1 | 2 | 1.705 | 0.9363 | 2473435708 |
| 4096 | 1 | 4 | 5.987 | 0.9557 | 2233735567 |
| 4096 | 1 | 8 | 5.096 | 0.9472 | 2230737707 |
| 4096 | 1 | 16 | 8.715 | 0.9730 | 1728049618 |
| 4096 | 4 | 1 | 9.144 | 0.9786 | 1639533133 |
| 4096 | 4 | 2 | 9.929 | 0.9847 | 1550472836 |
| 4096 | 4 | 4 | 25.762 | 0.9959 | 1583271517 |
| 4096 | 4 | 8 | 23.702 | 0.9982 | 1620246954 |
| 4096 | 4 | 16 | 21.708 | 0.9978 | 1599130220 |
| 32768 | 4 | 1 | 11.941 | 0.9964 | 1885374151 |
| 32768 | 4 | 2 | 16.826 | 0.9970 | 1793633466 |
| 32768 | 4 | 4 | 14.227 | 0.9966 | 1811173996 |
| 32768 | 4 | 8 | 20.128 | 0.9964 | 1745511209 |
| 32768 | 4 | 16 | 7.884 | 0.9949 | 1748987923 |
| 32768 | 16 | 1 | 2.663 | 0.9962 | 2009401067 |
| 32768 | 16 | 2 | 6.239 | 0.9960 | 2089344193 |
| 32768 | 16 | 4 | 11.675 | 0.9964 | 1949182330 |
| 32768 | 16 | 8 | 10.240 | 0.9959 | 1871620615 |
| 32768 | 16 | 16 | 16.802 | 0.9952 | 1758794864 |

The raw worker rows report CPU, work, queue-depth samples and steals separately. These shapes do not demonstrate consistent benefit from 8/16 markers; keep the current default and fallback stealing. Shared-graph duplicate claims and the sequential chain frontier are visible limitations; no bitmap partitioning experiment is justified by these data alone.

## Lock distributions

| Kind | Samples (all graph runs) | Largest per-process p99 upper bound ns | Maximum ns |
|---|---:|---:|---:|
| SATB registry acquisition | 73754180 | 64 | 366584 |
| SATB private-buffer acquisition | 16994348 | 64 | 44733 |
| SATB seal slice | 3884 | 16384 | 14707 |
| sweep heap acquisition | 4227340 | 64 | 22241 |
| sweep heap hold | 4227340 | 2048 | 60383 |

Each p99 is a power-of-two histogram bound for one process; the maximum of these is not a pooled p99. No evidence here supports replacing the bounded sweep cursor with region-local locks. Native hook/release time remains outside the heap lock.

## SATB and termination

The following are exact lines from the focused final test log; SATB measurements are instrumented debug measurements, not application-throughput evidence:

```text
m3_termination notification=false samples=100 p50_ns=619186 p99_ns=1088291 closure_cpu_ns=5012101 cpu_valid=true
m3_termination notification=true samples=100 p50_ns=19677 p99_ns=76543 closure_cpu_ns=5160300 cpu_valid=true
m3_saturation mutators=4 mandatory_requests=8 completed_cycles=8 optional_rejections=16 hard_limit=2097152 mutator_progress=667
m3_satb threads=1 stores=4096 elapsed_ns=2992114 seal_ns=6582 registry_wait_ns=168636 registry_collisions=0 buffer_wait_ns=168721
m3_satb threads=8 stores=32768 elapsed_ns=11243471 seal_ns=11221 registry_wait_ns=1390293 registry_collisions=0 buffer_wait_ns=1371399
m3_satb threads=64 stores=262144 elapsed_ns=105692551 seal_ns=65332 registry_wait_ns=13735300 registry_collisions=0 buffer_wait_ns=13724357
m3_satb threads=128 stores=524288 elapsed_ns=210562637 seal_ns=141173 registry_wait_ns=7972403501 registry_collisions=39479 buffer_wait_ns=27884921
```

No registry collisions were observed at <=64 producers in this fixture. Above 64, collisions appear; profiling itself prolongs holds. This does not justify a TLS redesign or an extrapolated production collision rate. Seal capture verifies exactly producers x 4096 deletions.

## Integrated termination prototype

Normal build versus `gc-notification-prototype` with `WILLOW_GC_NOTIFY=1`. Five fresh processes per cell, alternating order, profiling disabled. Both execute the same closure and stopped recheck; only the feature build can interrupt idle waits after independently repaying collector CPU. Runnable retry work retains its 1 ms floor. RSS is peak process RSS; CPU from GNU time is coarse (10 ms), so zeros do not establish zero CPU cost.

| Application | Markers | Throughput ratio notify/default | 95% CI | Peak RSS ratio | Median process CPU s default/notify |
|---|---:|---:|---:|---:|---:|
| alloc | 1 | 0.9907 | [0.9746, 1.0155] | 1.0323 | 0.00/0.00 |
| alloc | 4 | 0.9787 | [0.9370, 0.9905] | 0.9770 | 0.00/0.00 |
| alloc | 16 | 1.0068 | [0.9619, 1.0307] | 1.0044 | 0.00/0.01 |
| chain | 1 | 1.0035 | [0.9847, 1.0479] | 1.0631 | 0.01/0.01 |
| chain | 4 | 1.0026 | [0.9743, 1.0291] | 1.0023 | 0.01/0.01 |
| chain | 16 | 0.9846 | [0.9755, 1.0637] | 0.9707 | 0.08/0.08 |
| graph | 1 | 1.2153 | [0.9157, 1.3169] | 0.9742 | 0.02/0.02 |
| graph | 4 | 0.8915 | [0.6871, 1.1647] | 1.0261 | 0.10/0.11 |
| graph | 16 | 1.0252 | [0.7425, 1.2838] | 1.0071 | 0.29/0.28 |

Actual closure fixture: 100 last-batch publications per mode per process, five release processes. The last batch completes at 0/100/500/1500 microseconds after closure observes its active reader. Latency starts at final accounting publication immediately before owner release, ends when the real closure returns a sweep plan after stopped recheck. CPU covers only closure, excluding thread creation/join. These are controlled closure-tail measurements, not whole-heap/application latency distributions.

| Wait policy | Median process p99 ns | p99 range ns | Median closure CPU ns (100 closures) |
|---|---:|---:|---:|
| paced sleep | 1059657 | 1058174..1061631 | 1352212 |
| notification | 57677 | 57628..63359 | 1633398 |

Closure CPU notify/default ratio: 1.2079, 95% CI [1.0459, 1.3327].

Decision: do not enable notification in normal builds. Real closure-tail latency improves, but application benefit is not consistent; the four-marker graph median exceeds the 3% regression allowance and CPU non-regression is not established. Retain the integrated feature only as a reproducible experiment. This decision uses the enabled prototype comparison, not the unrelated baseline instrumentation comparison.

## Opportunity admission / Q10

| CPU load | Optional admitted | Mandatory completed | Mandatory wait range ms | Process busy ppm | System idle ppm |
|---|---:|---:|---|---|---|
| idle | 5/5 | 5/5 | 0.136..0.172 | 0..0 | 1000000..1000000 |
| saturated | 0/5 | 5/5 | 0.948..25.864 | 913580..941176 | 47058..62500 |

Decision: keep early GC explicit and opt-in. Admit only fresh measured process/system spare capacity; missing/stale samples cannot defer required GC. The measured probe waits for its submitted asynchronous request receipt (without a second explicit collection) and completed required collection in every saturated run, with a 16 MiB hard limit and zero remaining managed bytes. The deterministic four-allocating-mutator test independently exercised a 2 MiB cap. Foreign-root receipt regressions separately prove that early/handshake skips do not complete requests, retained asynchronous requests recover after release, and shutdown leaves canceled receipts incomplete (m3_.txt). These finite tests are evidence against starvation in the exercised cases, not a universal scheduling-latency guarantee.

## Native execution

Linux x86_64: executed. macOS arm64/x86_64 and Windows x86_64: Clippy cross-compiled; native execution unverified. CI and full local suites were not run. See audit.md for scope, complexity, lock order and limitations.
