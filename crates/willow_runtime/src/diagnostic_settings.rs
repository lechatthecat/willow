//! Process diagnostic options. Each variable is sampled on first use and shared
//! by all consumers. Operational GC/scheduler tuning remains with its owner.

use std::ffi::{OsStr, OsString};
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Presence {
    pub os: bool,
    pub unicode: bool,
    pub one: bool,
}
impl Presence {
    fn parse(value: Option<&OsStr>) -> Self {
        Self {
            os: value.is_some(),
            unicode: value.is_some_and(|v| v.to_str().is_some()),
            one: value == Some(OsStr::new("1")),
        }
    }
}

pub(crate) static GC_LOG: LazyLock<Presence> =
    LazyLock::new(|| Presence::parse(std::env::var_os("WILLOW_GC_LOG").as_deref()));
pub(crate) static GC_PROFILE: LazyLock<Presence> =
    LazyLock::new(|| Presence::parse(std::env::var_os("WILLOW_GC_PROFILE").as_deref()));
pub(crate) static GC_VERIFY_BARRIER: LazyLock<Presence> =
    LazyLock::new(|| Presence::parse(std::env::var_os("WILLOW_GC_VERIFY_BARRIER").as_deref()));
pub(crate) static GC_VERIFY_REGIONS: LazyLock<bool> =
    LazyLock::new(|| std::env::var("WILLOW_GC_VERIFY_REGIONS").is_ok());
pub(crate) static GC_NOTIFY: LazyLock<bool> = LazyLock::new(|| {
    std::env::var_os("WILLOW_GC_NOTIFY").as_deref() == Some(std::ffi::OsStr::new("1"))
});
pub(crate) static GC_MEASURE: LazyLock<bool> =
    LazyLock::new(|| std::env::var("WILLOW_GC_MEASURE").as_deref() == Ok("1"));
pub(crate) static SCHED_TRACE: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("WILLOW_SCHED_TRACE").is_some());
pub(crate) static TASK_TRACE: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("WILLOW_TASK_TRACE").is_some());

/// Unknown versions (including non-Unicode) retain the V1-only behavior.
pub(crate) static GC_TRACE_VERSION: LazyLock<u8> =
    LazyLock::new(
        || match std::env::var("WILLOW_GC_TRACE_VERSION").as_deref() {
            Ok("2") => 2,
            Ok("3") => 3,
            Ok("4") => 4,
            _ => 1,
        },
    );

// The trace sink's LazyLock calls this once. Do not cache the path separately:
// opening the sink consumes it, so even a long path leaves no retained copy.
pub(crate) fn gc_trace_path() -> Option<OsString> {
    std::env::var_os("WILLOW_GC_TRACE").filter(|p| !p.is_empty())
}

pub(crate) static GC_STRESS_MODES: LazyLock<Vec<String>> =
    LazyLock::new(|| parse_gc_stress_modes(&std::env::var("WILLOW_GC_STRESS").unwrap_or_default()));

pub(crate) fn parse_gc_stress_modes(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(crate) static GC_VERIFY_NO_PIN: LazyLock<bool> =
    LazyLock::new(|| verify_no_pin_enabled(std::env::var_os("WILLOW_GC_VERIFY_NO_PIN").as_deref()));

pub(crate) fn verify_no_pin_enabled(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty() && value != "0")
}

/// Reads `var` for [`bounded_capacity`]; a non-Unicode value is invalid.
fn env_capacity(var: &str, default: usize, max: usize) -> Option<usize> {
    match std::env::var(var) {
        Ok(value) => bounded_capacity(Some(&value), default, max),
        Err(std::env::VarError::NotPresent) => bounded_capacity(None, default, max),
        Err(std::env::VarError::NotUnicode(_)) => None,
    }
}

pub(crate) static TRACE_STOPS: LazyLock<bool> =
    LazyLock::new(|| matches!(*GC_TRACE_VERSION, 2..=4));
pub(crate) static TRACE_MILESTONES: LazyLock<bool> =
    LazyLock::new(|| matches!(*GC_TRACE_VERSION, 3 | 4));
/// V4 observation lines go only to version-4 streams; V1-V3 consumers never
/// see them even when `WILLOW_GC_MEASURE=1` enables the aggregate snapshot.
pub(crate) static TRACE_OBSERVATIONS: LazyLock<bool> = LazyLock::new(|| *GC_TRACE_VERSION == 4);
/// Opt-in bounded history size: absent selects `default`; unparsable or
/// outside `1..=max` yields None, which callers report instead of hiding.
pub(crate) fn bounded_capacity(value: Option<&str>, default: usize, max: usize) -> Option<usize> {
    match value {
        None => Some(default),
        Some(text) => text.parse::<usize>().ok().filter(|n| (1..=max).contains(n)),
    }
}

pub(crate) static GC_OBSERVATIONS: LazyLock<bool> =
    LazyLock::new(|| *GC_MEASURE || *GC_TRACE_VERSION == 4);

pub(crate) static GC_TRACE_CAPACITY: LazyLock<Option<usize>> = LazyLock::new(|| {
    use crate::gc_telemetry::observations::{CAPACITY, MAX_CAPACITY};
    env_capacity("WILLOW_GC_TRACE_CAPACITY", CAPACITY, MAX_CAPACITY)
});
pub(crate) static GC_STOP_TRACE_CAPACITY: LazyLock<Option<usize>> = LazyLock::new(|| {
    use crate::gc_telemetry::stops::{EVENT_CAPACITY, MAX_EVENT_CAPACITY};
    env_capacity(
        "WILLOW_GC_STOP_TRACE_CAPACITY",
        EVENT_CAPACITY,
        MAX_EVENT_CAPACITY,
    )
});

#[cfg(test)]
mod tests;

/// How long to wait before diagnosing a wedged registered mutator.
const DEFAULT_HANDSHAKE_TIMEOUT_SECS: u64 = 120;
pub(crate) static GC_HANDSHAKE_TIMEOUT: LazyLock<Option<std::time::Duration>> =
    LazyLock::new(|| {
        let seconds = match std::env::var("WILLOW_GC_HANDSHAKE_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
        {
            Some(seconds) => seconds,
            None => DEFAULT_HANDSHAKE_TIMEOUT_SECS,
        };
        (seconds != 0).then(|| std::time::Duration::from_secs(seconds))
    });
