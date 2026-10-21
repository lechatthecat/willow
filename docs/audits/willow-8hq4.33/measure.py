#!/usr/bin/env python3
"""Release allocation loop timing and native text size; run from repo root."""
import pathlib, subprocess, tempfile, time, statistics, sys
root = pathlib.Path(__file__).resolve().parent
compiler = pathlib.Path("target/debug/willow").resolve()
with tempfile.TemporaryDirectory() as tmp:
    binary = pathlib.Path(tmp)/"probe"
    subprocess.run([str(compiler), "build", str(root/"probe.wi"), "--release", "-o", str(binary)], check=True, capture_output=True)
    samples=[]
    for _ in range(9):
        p=subprocess.Popen([str(binary)], stdout=subprocess.PIPE, text=True)
        start=None
        for line in p.stdout:
            if line.strip()=="@start": start=time.perf_counter_ns()
            elif line.strip()=="@end": samples.append((time.perf_counter_ns()-start)/4000000)
            elif line.strip(): assert line.strip()=="7999998000000", line
        assert p.wait()==0
    print("label",sys.argv[1], "ns/iteration",samples,"median",statistics.median(samples))
    print("generated function sizes:")
    for line in subprocess.check_output(["nm","-S",str(binary)],text=True).splitlines():
        if line.endswith(" literal"): print(line)
    print(subprocess.check_output(["size",str(binary)],text=True))
