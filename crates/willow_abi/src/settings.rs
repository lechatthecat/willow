//! Shared runtime/compiler tuning settings; policy details are included below.
//!
//! Invalid-value policy: reject an invalid override according to its declared
//! class: advisory counts use the default, resource constraints fail closed,
//! verification budgets return the invalid sentinel for the existing diagnostic.
//! Switches are predicates (not boolean parsers); their complete value domains
//! are documented. Non-Unicode numeric values behave as absent, as before.
//!
//! Reads are deliberately uncached here: owners retain their existing sampling
//! lifetimes. This module adds no per-task state or eager environment snapshot.

use std::ffi::OsStr;

/// Positive, whitespace-tolerant advisory count. Invalid and absent mean default.
pub fn positive_count(value: Option<&str>) -> Option<usize> {
    value
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|count| *count > 0)
}

pub fn worker_count() -> Option<usize> {
    positive_count(std::env::var("WILLOW_WORKERS").ok().as_deref())
}

pub fn blocking_threads() -> Option<usize> {
    positive_count(std::env::var("WILLOW_BLOCKING_THREADS").ok().as_deref())
}

pub fn blocking_queue() -> Option<usize> {
    positive_count(std::env::var("WILLOW_BLOCKING_QUEUE").ok().as_deref())
}

/// Preemption accepts whitespace, but never a zero-length quantum.
pub fn preempt_value(value: Option<&str>, default: u64) -> u64 {
    value
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

pub fn task_budget(default: u64) -> u64 {
    preempt_value(std::env::var("WILLOW_TASK_BUDGET").ok().as_deref(), default)
}

pub fn time_quantum_ms(default: u64) -> u64 {
    preempt_value(
        std::env::var("WILLOW_TIME_QUANTUM_MS").ok().as_deref(),
        default,
    )
}

/// Bounded GC advisory counts deliberately do not trim whitespace.
pub fn bounded_count(value: Option<&str>, default: u64, maximum: u64) -> u64 {
    value
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0 && n <= maximum)
        .unwrap_or(default)
}

fn bounded_env(name: &str, default: u64, maximum: u64) -> u64 {
    bounded_count(std::env::var(name).ok().as_deref(), default, maximum)
}

pub fn remark_max_slots() -> u64 {
    bounded_env("WILLOW_GC_REMARK_MAX_SLOTS", 4096, 1_000_000)
}

pub fn remark_max_bytes() -> u64 {
    bounded_env("WILLOW_GC_REMARK_MAX_BYTES", 1024 * 1024, 16 * 1024 * 1024)
}

pub fn remark_max_work_ns() -> u64 {
    bounded_env("WILLOW_GC_REMARK_MAX_WORK_NS", 1_000_000, 100_000_000)
}

pub fn satb_capacity_value(value: Option<&str>) -> usize {
    bounded_count(value, 256, 65536) as usize
}

pub fn satb_capacity() -> usize {
    satb_capacity_value(
        std::env::var("WILLOW_GC_SATB_BUFFER_ENTRIES")
            .ok()
            .as_deref(),
    )
}

fn positive_bytes(value: Option<&str>, message: &str) -> Option<usize> {
    value.map(|value| {
        value
            .parse::<usize>()
            .ok()
            .filter(|bytes| *bytes > 0)
            .expect(message)
    })
}

pub fn gc_memory_limit_value(value: Option<&str>) -> Option<usize> {
    positive_bytes(
        value,
        "WILLOW_GC_MEMORY_LIMIT must be a positive byte count",
    )
}

pub fn gc_memory_limit() -> Option<usize> {
    gc_memory_limit_value(std::env::var("WILLOW_GC_MEMORY_LIMIT").ok().as_deref())
}

pub fn nursery_bytes_value(value: Option<&str>) -> Option<usize> {
    positive_bytes(
        value,
        "WILLOW_GC_NURSERY_BYTES must be a positive byte count",
    )
}

pub fn nursery_bytes() -> Option<usize> {
    nursery_bytes_value(std::env::var("WILLOW_GC_NURSERY_BYTES").ok().as_deref())
}

pub fn gc_memory_limit_bytes_value(value: Option<&str>) -> u64 {
    value
        .map(|value| {
            value
                .parse::<u64>()
                .expect("WILLOW_GC_MEMORY_LIMIT_BYTES must be a nonnegative byte count")
        })
        .unwrap_or(0)
}

pub fn gc_memory_limit_bytes() -> u64 {
    gc_memory_limit_bytes_value(
        std::env::var("WILLOW_GC_MEMORY_LIMIT_BYTES")
            .ok()
            .as_deref(),
    )
}

/// Zero is valid (no background markers); large valid counts saturate at 64.
pub fn mark_threads_value(value: Option<&str>) -> Option<usize> {
    value
        .and_then(|value| value.parse::<usize>().ok())
        .map(|count| count.min(64))
}

pub fn mark_threads() -> usize {
    mark_threads_value(std::env::var("WILLOW_GC_MARK_THREADS").ok().as_deref()).unwrap_or_else(
        || {
            std::thread::available_parallelism()
                .map(|count| count.get().saturating_sub(1).clamp(1, 4))
                .unwrap_or(1)
        },
    )
}

fn enabled_unless_zero(value: Option<&OsStr>) -> bool {
    value != Some(OsStr::new("0"))
}

pub fn concurrent_mark() -> bool {
    enabled_unless_zero(std::env::var_os("WILLOW_GC_CONCURRENT_MARK").as_deref())
}

pub fn gc_assist() -> bool {
    enabled_unless_zero(std::env::var_os("WILLOW_GC_ASSIST").as_deref())
}

pub fn gc_pacer() -> bool {
    enabled_unless_zero(std::env::var_os("WILLOW_GC_PACER").as_deref())
}

pub fn panic_snapshot_reuse() -> bool {
    enabled_unless_zero(std::env::var_os("WILLOW_PANIC_SNAPSHOT_REUSE").as_deref())
}

fn panic_effects_value(value: Option<&str>) -> bool {
    value.is_none_or(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
}

pub fn panic_effects() -> bool {
    panic_effects_value(std::env::var("WILLOW_PANIC_EFFECTS").ok().as_deref())
}

pub fn async_frame_all() -> bool {
    std::env::var("WILLOW_ASYNC_FRAME_ALL").is_ok()
}

pub fn query_cache() -> bool {
    std::env::var_os("WILLOW_QUERY_CACHE").as_deref() == Some(OsStr::new("1"))
}

/// Invalid verification budgets must still reach the verifier's diagnostic.
pub fn verification_budget_value(value: Option<&str>, default: u64) -> u64 {
    value.map(|v| v.parse().unwrap_or(0)).unwrap_or(default)
}

pub fn verification_timeout_ms() -> u64 {
    verification_budget_value(
        std::env::var("WILLOW_VERIFICATION_TIMEOUT_MS")
            .ok()
            .as_deref(),
        5_000,
    )
}

pub fn verification_resource_limit() -> u64 {
    verification_budget_value(
        std::env::var("WILLOW_VERIFICATION_RLIMIT").ok().as_deref(),
        1_000_000,
    )
}

pub fn handshake_timeout_value(value: Option<&str>, default: u64) -> u64 {
    value
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

pub fn handshake_timeout_seconds(default: u64) -> u64 {
    handshake_timeout_value(
        std::env::var("WILLOW_GC_HANDSHAKE_TIMEOUT_SECS")
            .ok()
            .as_deref(),
        default,
    )
}

#[cfg(test)]
mod tests;
