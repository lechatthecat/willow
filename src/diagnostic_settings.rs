//! Process diagnostic options, sampled once on first use of each setting.
//! Keep the historical predicates: presence, Unicode presence, `1`, and nonzero
//! are deliberately different. Operational/build options remain with their owners.

use std::sync::LazyLock;

pub(crate) static LIR_LOG: LazyLock<bool> =
    LazyLock::new(|| std::env::var("WILLOW_LIR_LOG").is_ok());
pub(crate) static VERIFY_DEBUG: LazyLock<bool> =
    LazyLock::new(|| std::env::var("WILLOW_VERIFY_DEBUG").is_ok());
pub(crate) static PANIC_DEPTH_LOAD_LOG: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("WILLOW_PANIC_DEPTH_LOAD_LOG").is_some());
pub(crate) static PANIC_EFFECTS_LOG: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("WILLOW_PANIC_EFFECTS_LOG").is_some());
pub(crate) static QUERY_STATS: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("WILLOW_QUERY_STATS").is_some_and(|v| v == "1"));
pub(crate) static QUERY_LOG: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("WILLOW_QUERY_LOG").is_some_and(|v| v == "1"));
pub(crate) static QUERY_TRACE: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("WILLOW_QUERY_TRACE").is_some_and(|v| v == "1"));
pub(crate) static QUERY_CACHE_STATS: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("WILLOW_QUERY_CACHE_STATS").is_some_and(|v| v == "1"));
#[cfg(test)]
pub(crate) static RETAIN_PROFILE: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("WILLOW_RETAIN_PROFILE").is_some_and(|v| v == "1"));
pub(crate) static INCREMENTAL_VERIFY: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("WILLOW_INCREMENTAL_VERIFY").is_some_and(|v| v == "1"));
pub(crate) static KEEP_OBJECT: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("WILLOW_KEEP_OBJECT").is_some_and(|value| value != "0"));
pub(crate) static UNIT_MEMORY_LOG: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("WILLOW_UNIT_MEMORY_LOG").is_some_and(|value| value != "0"));

#[cfg(test)]
mod tests;
