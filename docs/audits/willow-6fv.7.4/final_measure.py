#!/usr/bin/env python3
"""Run measurement jobs sequentially, with builds outside measured processes."""
import json, os, subprocess
from pathlib import Path
from validate import run, ROOT, OUT

def main():
    for i in range(5):
        run(f"barrier-process-{i}",["cargo","test","-p","willow_runtime","--release","--lib","satb_reference_store_throughput_baseline_and_current","--","--ignored","--nocapture"])
    for script in ["measure.py", "compare.py"]:
        subprocess.run(["rtk","proxy","python3",str(OUT/script)],cwd=ROOT,check=True)
    run("opportunity-build",["cargo","build","-p","willow_runtime","--release","--example","gc_m3_opportunity"])
    rows=[]
    for mode in ["idle","busy"]:
        for i in range(5):
            result=subprocess.run(["rtk","proxy",str(ROOT/"target/release/examples/gc_m3_opportunity"),mode],cwd=ROOT,env=dict(os.environ,WILLOW_GC_MEMORY_LIMIT="16777216"),capture_output=True,text=True,timeout=30,check=True)
            row=json.loads(result.stdout.strip())
            assert row["completed_cycles"] >= 1 and row["heap_after"] == 0
            if row["busy"]: assert not row["optional_accepted"]
            rows.append(dict(run=i,**row))
    (OUT/"opportunity.json").write_text(json.dumps(rows,indent=2)+"\n")
if __name__=="__main__": main()
