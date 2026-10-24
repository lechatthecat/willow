# Fixed-contract confirmation

Environment-specific; workload adopted after exploratory results.
Five new independent processes per build; every measured run retained.

| Trial | Build | p99 ms | p99.9 ms | Max ms | Objects/s | Peak RSS bytes | GC CPU ns/byte |
|---|---|---:|---:|---:|---:|---:|---:|
| 0 | baseline | 0.510423 | 0.522315 | 0.817906 | 30299076 | 25116672 | 0.940625 |
| 0 | candidate | 0.496616 | 0.502979 | 0.509020 | 30543317 | 26202112 | 0.932383 |
| 1 | baseline | 0.515382 | 0.531962 | 0.544646 | 30215005 | 25313280 | 0.944955 |
| 1 | candidate | 0.500835 | 0.578129 | 0.784193 | 30438174 | 26312704 | 0.935846 |
| 2 | baseline | 0.512406 | 0.522004 | 0.754889 | 30258135 | 25186304 | 0.943015 |
| 2 | candidate | 0.584461 | 0.695939 | 0.784413 | 30449317 | 26062848 | 0.934549 |
| 3 | baseline | 0.537403 | 0.547371 | 0.549565 | 30291327 | 25444352 | 0.941155 |
| 3 | candidate | 0.598898 | 0.770137 | 0.822995 | 30155282 | 26169344 | 0.943619 |
| 4 | baseline | 0.517616 | 0.549736 | 0.740461 | 30249937 | 25546752 | 0.944371 |
| 4 | candidate | 0.503880 | 0.616400 | 0.706008 | 30366745 | 26247168 | 0.934961 |

Candidate/baseline ratios (median of processes; paired bootstrap 95% interval):

- objects_per_second: 1.005950, [0.995509, 1.008061].
- peak_process_rss_bytes: 1.035113, [1.027417, 1.043216].
- gc_cpu_ns_per_allocated_byte: 0.991460, [0.990036, 1.002618].
- Worst-process peak footprint ratio: 1.029982.

The five-process bootstrap is descriptive and has limited tail precision;
it does not prove a universal bound or erase host scheduling noise.

Fixed gates:

- median_throughput: PASS.
- peak_footprint: PASS.
- gc_cpu_per_allocated_byte: PASS.
- every_process_p99: PASS.

Foreign native execution and CI remain unverified.
