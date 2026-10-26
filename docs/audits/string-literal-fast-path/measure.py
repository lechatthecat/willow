#!/usr/bin/env python3
"""Linux x86-64 proof; binaries stay in a caller-owned temporary directory.
Usage: measure.py BEFORE_BINARY AFTER_BINARY AFTER_COMPILER TEMP_DIR
Run after building string_build.wi with each compiler in --release mode.
"""
import json
from pathlib import Path
import statistics
import subprocess
import sys
import time

before, after, compiler, temporary = sys.argv[1:]
root = Path(temporary)
root.mkdir(parents=True, exist_ok=True)
results = {"benchmark": {}, "counts": []}
samples = {"before": [], "after": []}
for iteration in range(8):
    for name, binary in ([('before', before), ('after', after)] if iteration % 2 == 0
                         else [('after', after), ('before', before)]):
        start = time.perf_counter()
        out = subprocess.check_output([binary], text=True)
        elapsed = time.perf_counter() - start
        assert out == "START\nEND\n31301585\n", repr(out)
        if iteration:
            samples[name].append(elapsed)
for name, values in samples.items():
    results["benchmark"][name] = {"seconds": values, "median": statistics.median(values)}
print(json.dumps({"benchmark": results["benchmark"]}), flush=True)
# Break at the real runtime entry. Any initialized argument indicates a
# generated fast-path miss: inspect the slot before the runtime can touch it.
for unique in [1, 8, 32]:
    for repetitions in [1, 16, 256]:
        source = root / "probe.wi"
        binary = root / "probe"
        body = "\n".join(f'total = total + "literal {n}".len();' for n in range(unique))
        source.write_text(f'fn main() {{ let mut total = 0; let mut i = 0; '
                          f'while i < {repetitions} {{ {body} i = i + 1; }} println(total); }}')
        subprocess.run([compiler, 'build', str(source), '--release', '-o', str(binary)],
                       check=True, stdout=subprocess.DEVNULL)
        commands = root / "count.gdb"
        commands.write_text(r"""set pagination off
set confirm off
set $calls = 0
set $hits = 0
break willow_string_literal_slot
commands
silent
set $calls = $calls + 1
if *(unsigned long*)$rdi > 1
set $hits = $hits + 1
end
continue
end
run
printf "COUNTS %d %d\n", $calls, $hits
quit
""")
        out = subprocess.check_output(['gdb', '-q', '-batch', '-x', str(commands), str(binary)],
                                      text=True, stderr=subprocess.STDOUT)
        counts = next(line for line in out.splitlines() if line.startswith('COUNTS ')).split()
        calls, hits = map(int, counts[1:])
        assert (calls, hits) == (unique, 0), out
        record = dict(unique=unique, repetitions=repetitions, calls=calls, initialized_calls=hits)
        results['counts'].append(record)
        print(json.dumps(record), flush=True)
(root / 'results.json').write_text(json.dumps(results, indent=2) + '\n')
