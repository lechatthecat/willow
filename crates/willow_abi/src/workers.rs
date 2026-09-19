//! Shared worker-count policy for the compiler and runtime.

use std::sync::LazyLock;

/// Available parallelism at first use, or one worker if detection fails.
/// Cache the OS query because scheduler configuration is read on every drive.
pub fn default_worker_count() -> usize {
    static WORKERS: LazyLock<usize> = LazyLock::new(|| {
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
    });
    *WORKERS
}

/// Accept positive counts verbatim; absent, zero, invalid and overflowing
/// values leave the caller to select its default.
pub fn parse_worker_count(value: Option<&str>) -> Option<usize> {
    value
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|workers| *workers > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_overrides_preserve_positive_counts() {
        for workers in [1, 2, 4, 8, 64, usize::MAX] {
            assert_eq!(
                parse_worker_count(Some(&format!(" {workers} "))),
                Some(workers)
            );
        }
        for value in [
            None,
            Some("0"),
            Some("-1"),
            Some(""),
            Some("many"),
            Some("999999999999999999999999999999999999999"),
        ] {
            assert_eq!(parse_worker_count(value), None);
        }
    }

    #[test]
    fn detected_default_is_positive_and_cached() {
        let workers = default_worker_count();
        assert!(workers >= 1);
        for _ in 0..64 {
            assert_eq!(default_worker_count(), workers);
        }
    }
}
