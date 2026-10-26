#!/usr/bin/env python3
"""Compare public HEAD runtime with working tree, profiling disabled."""
import hashlib, io, json, os, random, shutil, statistics, subprocess, tarfile, tempfile
from pathlib import Path
ROOT=Path(__file__).resolve().parents[3]
OUT=Path(__file__).resolve().parent

def call(args, **kwargs):
    return subprocess.run(["rtk", "proxy", *map(str,args)], check=True, **kwargs)

def main():
    names=call(["git", "ls-tree", "-r", "--name-only", "HEAD"], cwd=ROOT, capture_output=True, text=True).stdout.splitlines()
    assert not any(n.startswith("requirements/") for n in names)
    head=call(["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    with tempfile.TemporaryDirectory(prefix="willow-m3-baseline-") as tmp:
        tmp=Path(tmp)
        archive=call(["git", "archive", "HEAD"], cwd=ROOT, capture_output=True).stdout
        with tarfile.open(fileobj=io.BytesIO(archive)) as tar: tar.extractall(tmp, filter="data")
        env=dict(os.environ, CARGO_TARGET_DIR=str(ROOT / "target"))
        env.pop("WILLOW_GC_PROFILE", None)
        call(["cargo", "build", "-p", "willow_runtime", "--release", "--example", "gc_allocation_bench"], cwd=tmp, env=env)
        baseline=tmp/"baseline"
        shutil.copy2(ROOT/"target/release/examples/gc_allocation_bench", baseline)
        call(["cargo", "build", "-p", "willow_runtime", "--release", "--example", "gc_allocation_bench"], cwd=ROOT, env=env)
        current=ROOT/"target/release/examples/gc_allocation_bench"
        hashes={"baseline": hashlib.sha256(baseline.read_bytes()).hexdigest(), "current": hashlib.sha256(current.read_bytes()).hexdigest()}
        assert hashes["baseline"] != hashes["current"], "baseline/candidate binary collision"
        rows=[]
        for mode in ["alloc", "chain"]:
            for workers in [1,4,16]:
                for run in range(5):
                    for version, exe in ([('baseline',baseline),('current',current)] if run%2==0 else [('current',current),('baseline',baseline)]):
                        rss=tmp/"rss"
                        result=call(["/usr/bin/time","-f","%M","-o",rss,exe,"65536",mode], cwd=ROOT, env=dict(env,WILLOW_GC_MARK_THREADS=str(workers)),capture_output=True,text=True,timeout=120)
                        data=json.loads(result.stdout.strip())
                        rows.append(dict(version=version,mode=mode,workers=workers,run=run,rss_kib=int(rss.read_text()),**data))
                print(f"comparison {mode} workers={workers} complete",flush=True)
        (OUT/"comparison.json").write_text(json.dumps(dict(baseline=head,profile=False,binary_sha256=hashes,rows=rows),indent=2)+"\n")
if __name__=="__main__": main()
