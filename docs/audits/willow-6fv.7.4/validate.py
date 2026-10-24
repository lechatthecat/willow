#!/usr/bin/env python3
"""Focused validation only. No full suite, CI, staging, or Beads mirror."""
import os, re, subprocess
from pathlib import Path
ROOT=Path(__file__).resolve().parents[3]
OUT=Path(__file__).resolve().parent

def run(name,args,env=None):
    result=subprocess.run(["rtk","proxy",*args],cwd=ROOT,env=env,text=True,stdout=subprocess.PIPE,stderr=subprocess.STDOUT)
    (OUT/(name+".txt")).write_text(result.stdout)
    print(name, result.returncode,flush=True)
    if result.returncode or ("test" in args and not re.search(r"test result: ok\. [1-9]",result.stdout)):
        print(result.stdout[-6000:]); raise SystemExit(1)

if __name__=="__main__":
    run("fmt",["cargo","fmt","--all","--","--check"])
    for target in ["x86_64-unknown-linux-gnu","x86_64-pc-windows-msvc","aarch64-apple-darwin","x86_64-apple-darwin"]:
        run("clippy-"+target,["cargo","clippy","--workspace","--all-targets","--target",target,"--","-D","warnings"])
        run("clippy-notification-"+target,["cargo","clippy","--workspace","--all-targets","--features","willow_runtime/gc-notification-prototype","--target",target,"--","-D","warnings"])
    for name in ["foreign_owner_tests", "allocation_resumes_while_automatic_collection_is_tracing", "coordinator_panic_terminates", "automatic_request_survives_explicit_collector_election_loss", "shutdown_notification", "m3_", "termination_", "marker_panics_publish_failure", "concurrent_closure_waits_for_batched_work", "concurrent_closure_paces_contention", "root_activation_late_registration", "a_mutator_that_dies_registered_reports", "satb_parallel_mutators_retain_deleted_subgraphs", "satb_registration_owns_buffer", "satb_remark_cap", "satb_closure_wave_cap", "bounded_sweep_amortizes_heap_holds", "bounded_sweep_competing_allocators", "bounded_sweep_descriptor_writer_contention", "assist_work_credit_counts", "marker_pool_shared_budget", "marker_pool_drains_each_claim", "termination_spurious_pool_wakes", "native_retry_exhaustion", "hard_limit", "concurrent_chain_continuations", "concurrent_nonleaf_fanout_batches", "pointer_free_leaf_fanout", "satb_repeated_equivalent_deletions"]:
        run(name,["cargo","test","-p","willow_runtime","--lib",name,"--","--test-threads=1","--nocapture"],dict(os.environ,WILLOW_GC_PROFILE="1"))
    for name in ["satb_codegen_loads_old_before_each_reference_store_in_linear_code", "interface_pair_stores_trace_only_object_word_in_linear_code"]:
        run(name,["cargo","test","-p","willow","--lib",name,"--","--nocapture"])

    for name in ["concurrent_closure_waits_for_batched_work", "concurrent_closure_paces_contention", "termination_", "native_retry_exhaustion"]:
        run("notification-"+name,["cargo","test","-p","willow_runtime","--features","gc-notification-prototype","--lib",name,"--","--test-threads=1","--nocapture"],dict(os.environ,WILLOW_GC_NOTIFY="1"))
