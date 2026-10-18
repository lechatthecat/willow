//! Bounded JSON diagnostics; no waits, graph scans or owned GC references.
use super::measurement::GcMemoryV3;
use std::fmt::{self, Write as _};
use std::io::{self, Write};

pub const FAILURE_RECORD_LIMIT: usize = 8192;
const REASON_CHARS: usize = 256;
#[derive(Clone, Copy, Default)]
struct Reproduction {
    seed: u64,
    case: u64,
}
thread_local! {
    static REPRO: std::cell::Cell<Reproduction> = const { std::cell::Cell::new(Reproduction { seed: 0, case: 0 }) };
    static EMITTING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
pub fn reproduction(seed: u64, case: u64) {
    REPRO.set(Reproduction { seed, case });
}

struct Buffer {
    bytes: [u8; FAILURE_RECORD_LIMIT],
    len: usize,
}
impl fmt::Write for Buffer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self
            .len
            .checked_add(s.len())
            .filter(|&end| end <= self.bytes.len())
            .ok_or(fmt::Error)?;
        self.bytes[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}
struct Number(Option<u64>);
impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(n) => n.fmt(f),
            None => f.write_str("null"),
        }
    }
}
fn string(out: &mut Buffer, value: &str) -> fmt::Result {
    out.write_char('"')?;
    for ch in value.chars().take(REASON_CHARS) {
        match ch {
            '"' => out.write_str("\\\"")?,
            '\\' => out.write_str("\\\\")?,
            ch if ch <= '\u{1f}' => write!(out, "\\u{:04x}", ch as u32)?,
            ch => out.write_char(ch)?,
        }
    }
    out.write_char('"')
}

fn record(
    out: &mut Buffer,
    kind: &str,
    reason: &str,
    memory: Option<GcMemoryV3>,
    actual: Option<u64>,
    limit: Option<u64>,
) -> fmt::Result {
    let epoch = super::milestones::try_snapshot().map(|r| r.epoch);
    let r = REPRO.get();
    let frames = crate::frame_lifetime::snapshot();
    out.write_str("{\"version\":3,\"event\":\"gc_failure\",\"kind\":")?;
    string(out, kind)?;
    out.write_str(",\"reason\":")?;
    string(out, reason)?;
    write!(
        out,
        ",\"timestamp_ns\":{},\"epoch\":{},\"seed\":{},\"case\":{},\"heap_available\":{},\"heap_consistency\":\"metadata_without_lab_merge\",\"actual_bytes\":{},\"limit_bytes\":{}",
        super::timestamp_ns(),
        Number(epoch),
        r.seed,
        r.case,
        memory.is_some(),
        Number(actual),
        Number(limit)
    )?;
    for (name, value) in [
        ("managed_bytes", memory.map(|m| m.managed_bytes)),
        ("external_bytes", memory.map(|m| m.external_bytes)),
        ("old_reserved_bytes", memory.map(|m| m.old_reserved_bytes)),
        (
            "young_reserved_bytes",
            memory.map(|m| m.young_reserved_bytes),
        ),
    ] {
        write!(out, ",\"{name}\":{}", Number(value))?;
    }
    let cycle = super::diagnostic_cycle();
    let stop = super::stops::diagnostic_stop();
    let survivors = crate::gc::diagnostic_survivors();
    let coord = crate::gc::diagnostic_coordination();
    for (name, value) in [
        ("phase", cycle.map(|c| u64::from(c.phase))),
        (
            "last_major_live_bytes",
            memory.map(|m| m.last_major_retained_bytes),
        ),
        ("allocated_bytes", memory.map(|m| m.managed_bytes)),
        ("root_scan_bytes", cycle.map(|c| c.root_scan_bytes)),
        ("last_stop_reason", stop.map(|s| u64::from(s.reason))),
        ("last_stop_outcome", stop.map(|s| u64::from(s.outcome))),
        (
            "minor_deferrals",
            survivors.map(|s| s.deferred_minor_collections),
        ),
        (
            "minor_deferral_age_bytes",
            survivors.map(|s| s.minor_deferral_age_bytes),
        ),
        (
            "pin_fallback_count",
            survivors.map(|s| s.pin_fallback_count),
        ),
        ("pinned_bytes", survivors.map(|s| s.pinned_bytes)),
        ("registered_mutators", coord.map(|s| s.0)),
        ("parked_mutators", coord.map(|s| s.1)),
        ("pending_mutators", coord.map(|s| s.2)),
    ] {
        write!(out, ",\"{name}\":{}", Number(value))?;
    }
    out.write_str(",\"os\":")?;
    string(out, std::env::consts::OS)?;
    out.write_str(",\"arch\":")?;
    string(out, std::env::consts::ARCH)?;
    writeln!(
        out,
        ",\"allocated_frame_bytes\":{},\"reclaimed_frame_bytes\":{},\"quarantined_frame_bytes\":{},\"task_frames_running\":{},\"task_frames_rooted\":{},\"task_frames_terminal\":{},\"task_frames_retired\":{}}}",
        frames.allocated_frame_bytes,
        frames.reclaimed_frame_bytes,
        frames.quarantined_bytes,
        frames.task_frames_running,
        frames.task_frames_rooted,
        frames.task_frames_terminal,
        frames.task_frames_retired
    )
}

pub(crate) fn emit(kind: &str, reason: &str) {
    let memory = crate::gc::diagnostic_memory_snapshot();
    emit_with(
        kind,
        reason,
        memory,
        memory.map(|m| m.managed_bytes.saturating_add(m.external_bytes)),
        None,
    );
}
pub(crate) fn emit_with(
    kind: &str,
    reason: &str,
    memory: Option<GcMemoryV3>,
    actual: Option<u64>,
    limit: Option<u64>,
) {
    if EMITTING.replace(true) {
        return;
    }
    let mut out = Buffer {
        bytes: [0; FAILURE_RECORD_LIMIT],
        len: 0,
    };
    if record(&mut out, kind, reason, memory, actual, limit).is_ok() {
        let _ = io::stderr().lock().write_all(&out.bytes[..out.len]);
    } else {
        let _ = io::stderr().lock().write_all(
            b"{\"version\":3,\"event\":\"gc_failure\",\"kind\":\"format_unavailable\"}\n",
        );
    }
    EMITTING.set(false);
}

/// Opt-in test/control check. Snapshot merges TLAB accounting before testing.
/// Both actual bytes and contributing categories are captured before panicking.
pub fn assert_memory_at_most(limit: u64) {
    let memory = crate::gc::telemetry_memory_threshold_snapshot();
    let actual = memory.managed_bytes.saturating_add(memory.external_bytes);
    if actual > limit {
        emit_with(
            "memory_threshold",
            "occupied memory exceeds limit",
            Some(memory),
            Some(actual),
            Some(limit),
        );
        panic!("GC memory threshold exceeded: actual={actual} limit={limit}");
    }
}

/// Strict byte bound using the caller's sample, including when it holds the heap
/// lock. The kind identifies the measured quantity; categories are GC accounting
/// from that sample, not a claim that GC accounts for all process RSS.
#[cfg(test)]
pub(crate) fn assert_sample_below(kind: &str, memory: GcMemoryV3, actual: u64, limit: u64) {
    if actual >= limit {
        emit_with(
            kind,
            "byte sample must be below limit",
            Some(memory),
            Some(actual),
            Some(limit),
        );
        panic!("GC byte threshold exceeded: actual={actual} exclusive_limit={limit}");
    }
}

#[cfg(test)]
pub(crate) fn install_test_panic_hook() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let old = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let reason = info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
                .unwrap_or("runtime test panicked with a non-string payload");
            emit("test_panic", reason);
            old(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn m0_failure_json_bound_escaping_and_actual_bytes() {
        install_test_panic_hook();
        let mut out = Buffer {
            bytes: [0; FAILURE_RECORD_LIMIT],
            len: 0,
        };
        let memory = GcMemoryV3 {
            managed_bytes: u64::MAX,
            external_bytes: u64::MAX,
            old_reserved_bytes: u64::MAX,
            young_reserved_bytes: u64::MAX,
            ..Default::default()
        };
        record(
            &mut out,
            "memory_threshold",
            &"\u{1f}\"\\\n日本語".repeat(1000),
            Some(memory),
            Some(u64::MAX),
            Some(u64::MAX),
        )
        .unwrap();
        let text = std::str::from_utf8(&out.bytes[..out.len]).unwrap();
        assert!(out.len <= FAILURE_RECORD_LIMIT);
        assert_eq!(text.lines().count(), 1);
        assert!(text.starts_with('{') && text.ends_with("}\n"));
        assert!(text.contains("\\u001f\\\"\\\\\\u000a日本語"));
        assert!(text.contains("\"actual_bytes\":18446744073709551615"));
        assert!(text.contains("\"managed_bytes\":18446744073709551615"));
        eprint!("{text}");
        out.len = 0;
        record(
            &mut out,
            &"\u{1f}".repeat(1000),
            &"\u{1f}".repeat(1000),
            Some(memory),
            Some(u64::MAX),
            Some(u64::MAX),
        )
        .unwrap();
        assert!(out.len <= FAILURE_RECORD_LIMIT);
        eprint!("{}", std::str::from_utf8(&out.bytes[..out.len]).unwrap());
        out.len = 0;
        record(&mut out, "panic", "locked heap", None, None, None).unwrap();
        let text = std::str::from_utf8(&out.bytes[..out.len]).unwrap();
        assert!(text.contains("\"heap_available\":false"));
        assert!(text.contains("\"managed_bytes\":null"));
    }

    #[test]
    fn m0_failure_real_invariant_panic_threshold_and_locked_heap() {
        const KEY: &str = "WILLOW_M0_FAILURE_CASE";
        if let Ok(case) = std::env::var(KEY) {
            let _guard = crate::gc::runtime_test_guard();
            crate::gc::reset_internal_for_test();
            reproduction(7_276_289, 42);
            crate::gc::willow_alloc(16);
            match case.as_str() {
                "invariant" => crate::failure::fatal_invariant("injected GC invariant failure"),
                #[cfg(debug_assertions)]
                "invalid_pointer" => crate::gc::diagnostic_invalid_pointer_for_test(),
                "panic" => panic!("injected test panic"),
                "threshold" => assert_memory_at_most(0),
                "locked" => crate::gc::diagnostic_locked_panic_for_test(),
                _ => unreachable!(),
            }
            panic!("failure path returned");
        }
        let mut cases = vec!["invariant", "panic", "threshold", "locked"];
        if cfg!(debug_assertions) {
            cases.push("invalid_pointer");
        }
        for case in cases {
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "gc_telemetry::failure_record::tests::m0_failure_real_invariant_panic_threshold_and_locked_heap", "--nocapture"])
                .env(KEY, case).output().unwrap();
            assert!(!child.status.success());
            let stderr = String::from_utf8_lossy(&child.stderr);
            let json: Vec<_> = stderr
                .lines()
                .filter(|line| line.starts_with("{\"version\":3,\"event\":\"gc_failure\""))
                .collect();
            assert!(!json.is_empty(), "{case}: {stderr}");
            for line in &json {
                assert!(line.len() < FAILURE_RECORD_LIMIT);
                eprintln!("{line}");
            }
            assert!(json[0].contains("\"seed\":7276289,\"case\":42"));
            if case == "locked" {
                assert!(json[0].contains("\"heap_available\":false"));
            } else {
                assert!(json[0].contains(&format!(
                    "\"managed_bytes\":{}",
                    crate::gc::header_size_for_test() + 16
                )));
            }
            if case == "threshold" {
                assert!(json[0].contains("\"kind\":\"memory_threshold\""));
                assert!(json[0].contains("\"limit_bytes\":0"));
            }
        }
    }
}
