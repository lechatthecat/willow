"""Run a previously compiled release benchmark; emits raw wall times and checksum."""
import json
import subprocess
import sys
import time

binary, label = sys.argv[1:]
subprocess.run([binary], check=True, capture_output=True)
times = []
for _ in range(7):
    start = time.perf_counter()
    run = subprocess.run([binary], check=True, capture_output=True, text=True)
    times.append(time.perf_counter() - start)
    assert run.stdout == "900000000\n", run.stdout
print(json.dumps({"label": label, "seconds": times, "checksum": 900000000}))
