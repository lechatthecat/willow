"""Fresh-process telemetry avoids mixing histogram windows across thread counts."""
import os
import subprocess
from pathlib import Path

with Path(__file__).with_name("measurement.txt").open("w") as output:
    for threads in (1, 5, 16):
        env = dict(os.environ, WILLOW_GC_MEASURE="1", WILLOW_ASSIST_THREADS=str(threads))
        subprocess.run(["rtk", "proxy", "cargo", "test", "-p", "willow_runtime",
                        "allocation_assist_progress_latency_and_thread_fairness", "--", "--nocapture"],
                       env=env, stdout=output, stderr=subprocess.STDOUT, check=True, timeout=120)
    for enabled in ("1", "0"):
        print("WILLOW_GC_ASSIST=" + enabled, file=output, flush=True)
        subprocess.run(["rtk", "proxy", "cargo", "test", "-p", "willow_runtime",
                        "pacer_reference_measurement", "--", "--ignored", "--nocapture"],
                       env=dict(os.environ, WILLOW_GC_ASSIST=enabled),
                       stdout=output, stderr=subprocess.STDOUT, check=True, timeout=180)
        subprocess.run(["rtk", "proxy", "cargo", "test", "-p", "willow_runtime",
                        "allocation_assist_pressure_preserves_hard_reservation_bound", "--", "--nocapture"],
                       env=dict(os.environ, WILLOW_GC_ASSIST=enabled),
                       stdout=output, stderr=subprocess.STDOUT, check=True, timeout=180)
