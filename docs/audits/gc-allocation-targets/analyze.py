"""Derive acceptance from raw, checksum-checked trials without dropping outliers."""
import json
from pathlib import Path
HERE = Path(__file__).resolve().parent
rows = [json.loads(s) for s in (HERE / "final-comparison.jsonl").read_text().splitlines()]
summaries = {(r["case"], r["language"], r["phase"]): r for r in rows if r["type"] == "summary"}
def value(case, language="willow", phase="elapsed_ns"):
    return summaries[case, language, phase]["median_ns"]
chain = [json.loads(s) for s in (HERE / "final-chain.jsonl").read_text().splitlines()]
chain_summary = next(r for r in chain if r.get("summary") and r["n"] == 3000000)
values = {
    "build_ns_per_node": value("linked_list", phase="setup_ns") / 3000000,
    "linked_list_vs_go": value("linked_list") / value("linked_list", "go"),
    "scheduler_vs_churn": (value("gc_scheduler") / 12000000) / (value("object_churn") / 20000000),
    "scheduler_vs_java": value("gc_scheduler") / value("gc_scheduler", "java"),
    "interface_setup_ms": value("virtual_dispatch", phase="setup_ns") / 1000000,
    "chain_probe_ns_per_node": chain_summary["median"],
    "chain_spread": chain_summary["spread"],
}
limits = dict(build_ns_per_node=60, linked_list_vs_go=5, scheduler_vs_churn=1.3, scheduler_vs_java=5, interface_setup_ms=200, chain_probe_ns_per_node=60, chain_spread=.1)
result = dict(values=values, limits=limits, passes={k: v < limits[k] if k == "chain_spread" else v <= limits[k] for k, v in values.items()},
              comparison_trials=sum(r["type"] == "trial" for r in rows), chain_trials=sum(not r.get("summary") for r in chain),
              note="Five trials per workload/language and chain size; all outliers retained. Linux x86_64 native only.")
(HERE / "results.json").write_text(json.dumps(result, indent=2) + "\n")
print(json.dumps(result, indent=2))
assert all(result["passes"].values())
