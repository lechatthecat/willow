//! Lazily sampled, runtime-owned diagnostic options.

use std::ffi::{OsStr, OsString};
use std::sync::{LazyLock, OnceLock};

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

pub(crate) struct RuntimeDiagnostics {
    pub(crate) gc_trace_capacity: LazyLock<Option<usize>>,
    pub(crate) gc_handshake_timeout: LazyLock<Option<std::time::Duration>>,
    trace_stops: OnceLock<bool>,
    trace_milestones: OnceLock<bool>,
    trace_observations: OnceLock<bool>,
    gc_observations: OnceLock<bool>,

    pub(crate) gc_notify: LazyLock<bool>,
    pub(crate) gc_measure: LazyLock<bool>,
    pub(crate) sched_trace: LazyLock<bool>,
    pub(crate) task_trace: LazyLock<bool>,
    pub(crate) gc_trace_version: LazyLock<u8>,
    pub(crate) gc_stress_modes: LazyLock<Vec<String>>,
    pub(crate) stress_override: RuntimeStressOverride,
    pub(crate) gc_verify_no_pin: LazyLock<bool>,
    pub(crate) gc_log: LazyLock<Presence>,
    pub(crate) gc_profile: LazyLock<Presence>,
    pub(crate) verify_regions: LazyLock<bool>,
    pub(crate) verify_barrier: LazyLock<Presence>,
    /// Whether barriers count per-variant telemetry. Follows the presence of
    /// `WILLOW_GC_VERIFY_BARRIER`; a test fixture runtime enables it directly,
    /// so production barriers carry no test-build branch.
    pub(crate) barrier_counting: LazyLock<bool>,
}
impl RuntimeDiagnostics {
    pub(crate) fn trace_stops(&self) -> bool {
        *self
            .trace_stops
            .get_or_init(|| matches!(*self.gc_trace_version, 2..=4))
    }
    pub(crate) fn trace_milestones(&self) -> bool {
        *self
            .trace_milestones
            .get_or_init(|| matches!(*self.gc_trace_version, 3 | 4))
    }
    /// Only version4 streams emit observation lines, even when measurement is enabled.
    pub(crate) fn trace_observations(&self) -> bool {
        *self
            .trace_observations
            .get_or_init(|| *self.gc_trace_version == 4)
    }
    pub(crate) fn gc_observations(&self) -> bool {
        *self
            .gc_observations
            .get_or_init(|| *self.gc_measure || *self.gc_trace_version == 4)
    }
    pub(crate) const fn new() -> Self {
        Self {
            gc_trace_capacity: LazyLock::new(|| {
                use crate::gc_telemetry::observations::{CAPACITY, MAX_CAPACITY};
                env_capacity("WILLOW_GC_TRACE_CAPACITY", CAPACITY, MAX_CAPACITY)
            }),
            gc_handshake_timeout: LazyLock::new(|| {
                let seconds = match std::env::var("WILLOW_GC_HANDSHAKE_TIMEOUT_SECS")
                    .ok()
                    .and_then(|value| value.trim().parse::<u64>().ok())
                {
                    Some(seconds) => seconds,
                    None => DEFAULT_HANDSHAKE_TIMEOUT_SECS,
                };
                (seconds != 0).then(|| std::time::Duration::from_secs(seconds))
            }),
            trace_stops: OnceLock::new(),
            trace_milestones: OnceLock::new(),
            trace_observations: OnceLock::new(),
            gc_observations: OnceLock::new(),
            gc_notify: LazyLock::new(|| {
                std::env::var_os("WILLOW_GC_NOTIFY").as_deref() == Some(std::ffi::OsStr::new("1"))
            }),
            gc_measure: LazyLock::new(|| std::env::var("WILLOW_GC_MEASURE").as_deref() == Ok("1")),
            sched_trace: LazyLock::new(|| std::env::var_os("WILLOW_SCHED_TRACE").is_some()),
            task_trace: LazyLock::new(|| std::env::var_os("WILLOW_TASK_TRACE").is_some()),
            gc_trace_version: LazyLock::new(|| {
                match std::env::var("WILLOW_GC_TRACE_VERSION").as_deref() {
                    Ok("2") => 2,
                    Ok("3") => 3,
                    Ok("4") => 4,
                    _ => 1,
                }
            }),
            stress_override: new_stress_override(),
            gc_stress_modes: LazyLock::new(|| {
                parse_gc_stress_modes(&std::env::var("WILLOW_GC_STRESS").unwrap_or_default())
            }),
            gc_verify_no_pin: LazyLock::new(|| {
                verify_no_pin_enabled(std::env::var_os("WILLOW_GC_VERIFY_NO_PIN").as_deref())
            }),
            gc_log: LazyLock::new(|| Presence::parse(std::env::var_os("WILLOW_GC_LOG").as_deref())),
            gc_profile: LazyLock::new(|| {
                Presence::parse(std::env::var_os("WILLOW_GC_PROFILE").as_deref())
            }),
            verify_regions: LazyLock::new(|| std::env::var("WILLOW_GC_VERIFY_REGIONS").is_ok()),
            verify_barrier: LazyLock::new(|| {
                Presence::parse(std::env::var_os("WILLOW_GC_VERIFY_BARRIER").as_deref())
            }),
            barrier_counting: LazyLock::new(|| {
                std::env::var_os("WILLOW_GC_VERIFY_BARRIER").is_some()
            }),
        }
    }
}

// The trace sink's LazyLock calls this once. Do not cache the path separately:
// opening the sink consumes it, so even a long path leaves no retained copy.
pub(crate) fn gc_trace_path() -> Option<OsString> {
    std::env::var_os("WILLOW_GC_TRACE").filter(|p| !p.is_empty())
}

pub(crate) fn parse_gc_stress_modes(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
        .map(str::to_owned)
        .collect()
}

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

/// Opt-in bounded history size: absent selects `default`; unparsable or
/// outside `1..=max` yields None, which callers report instead of hiding.
pub(crate) fn bounded_capacity(value: Option<&str>, default: usize, max: usize) -> Option<usize> {
    match value {
        None => Some(default),
        Some(text) => text.parse::<usize>().ok().filter(|n| (1..=max).contains(n)),
    }
}

#[cfg(test)]
mod tests;

/// How long to wait before diagnosing a wedged registered mutator.
const DEFAULT_HANDSHAKE_TIMEOUT_SECS: u64 = 120;

// Each stop-history owner samples its configuration on first use.
pub(crate) fn stop_trace_capacity() -> Option<usize> {
    use crate::gc_telemetry::stops::{EVENT_CAPACITY, MAX_EVENT_CAPACITY};
    env_capacity(
        "WILLOW_GC_STOP_TRACE_CAPACITY",
        EVENT_CAPACITY,
        MAX_EVENT_CAPACITY,
    )
}

trait StressOverride {
    fn enabled(&self, kind: &str, configured: impl FnOnce() -> bool) -> bool;
}

#[cfg(not(test))]
pub(crate) struct RuntimeStressOverride;
#[cfg(not(test))]
impl RuntimeStressOverride {
    const fn new() -> Self {
        Self
    }
}
#[cfg(not(test))]
impl StressOverride for RuntimeStressOverride {
    fn enabled(&self, _kind: &str, configured: impl FnOnce() -> bool) -> bool {
        configured()
    }
}

#[cfg(test)]
pub(crate) type RuntimeStressOverride = std::sync::Mutex<Option<Vec<String>>>;
#[cfg(test)]
impl StressOverride for RuntimeStressOverride {
    fn enabled(&self, kind: &str, configured: impl FnOnce() -> bool) -> bool {
        let modes = self.lock().unwrap_or_else(|p| p.into_inner());
        match &*modes {
            Some(modes) => modes.iter().any(|mode| mode == "all" || mode == kind),
            None => configured(),
        }
    }
}

impl RuntimeDiagnostics {
    pub(crate) fn stress_enabled(&self, kind: &str) -> bool {
        self.stress_override.enabled(kind, || {
            self.gc_stress_modes
                .iter()
                .any(|mode| mode == "all" || mode == kind)
        })
    }
}

#[cfg(not(test))]
const fn new_stress_override() -> RuntimeStressOverride {
    RuntimeStressOverride::new()
}
#[cfg(test)]
const fn new_stress_override() -> RuntimeStressOverride {
    std::sync::Mutex::new(None)
}
