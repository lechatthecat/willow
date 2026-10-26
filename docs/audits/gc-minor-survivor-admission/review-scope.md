# Independent review request for gc-minor-survivor-admission

Review this ticket independently and read files locally. Do not modify source,
close tickets, stage/commit/push, check CI or launch full test suites. Inspect
the entire relevant working-tree change, new files and evidence, not only the
old findings. User authorized three additional reviews. Each reviewer is to
return concrete file/line findings or approval with limitations; do not assume
another review or historical validation constitutes your approval. Do not read
other new reviewers' outputs until your own review is complete.

Repository: /home/shu/Documents/gitproject/willow
Instructions: AGENTS.md and /home/shu/.codex/RTK.md
Original findings: .git/ticket-loop-runs/ticket-8mem9_5y/review-1.txt
Current ticket: .git/ticket-loop-runs/ticket-psznjx4q/ticket.json
Current notes: .git/ticket-loop-runs/ticket-psznjx4q/review-notes.md
Public audit/evidence directory: docs/audits/gc-minor-survivor-admission/
Contract: contract.md and confirmation-lock.json within that directory.
Fresh confirmation: confirmation-01/report.md, replicates.json,
replicates-samples.jsonl, and individual stdout/stderr/status files.
Historical evidence: measurements.json, samples.jsonl, replicates.json,
replicates-samples.jsonl. Do not paste these raw files in messages.
Validation: validation.txt, fmt.txt, clippy-*.txt, example.txt and
confirmation-validation.txt (new probe validation).

Source scope: crates/willow_runtime/src/gc.rs, gc/minor.rs,
gc/minor_tests.rs, gc/minor_m4_probe.rs, gc/safepoint.rs, gc/telemetry.rs,
gc_telemetry.rs, gc_telemetry/measurement.rs, gc_telemetry/stops.rs,
gc_telemetry/pause.rs; inspect affected callers and unchanged overlap tests.
Also review docs/gc_telemetry.md, reproducer and .gitignore allowlist/privacy.
Use git diff and local files; no huge diff or raw measurements in prompts.

Check admission before both stops, accounting, cleanup, cap fallback and
concurrent old-mark safety; component boundaries/counting/cause-generation;
independent scaling, GC CPU/byte, throughput/footprint definitions, fixed
contract, unchanged baseline, retained failures, percentile/CI math, complexity
and all original review-1 findings. User adopted the fixture after seeing
exploratory results: verify this is explicit and fresh five-pair confirmation
happened after the contract was frozen. Do not waive GC CPU or other criteria.
Foreign native execution/CI absence alone is not a blocker per explicit user
policy; report it accurately. Local full suites are prohibited. Requirements
are private/local-only: never quote or copy them into review messages/reports.
