use std::path::PathBuf;
use willow_abi::workers::{default_worker_count, parse_worker_count};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BuildMode {
    Debug,
    Release,
}

#[derive(Debug, Clone)]
pub struct TargetOptions {
    pub build_mode: BuildMode,
    pub emit_debug_info: bool,
    pub emit_source_map: bool,
    pub strip_symbols: bool,
    pub runtime_lib: Option<PathBuf>,
    pub cargo_target_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct CompilerOptions {
    /// Require an existing, current project.lock; never update it.
    pub locked: bool,
    /// Resolve dependencies exclusively from the local package cache.
    pub offline: bool,
    pub target: TargetOptions,
    pub worker_count: Option<usize>,
    /// Retained for API compatibility; compilation always enforces Send/Sync.
    pub enforce_send_sync: bool,
}

/// Compatibility alias for callers that used the pre-library API name.
pub type CodegenOptions = CompilerOptions;

struct CompilerEnvironment {
    workers: Option<usize>,
    runtime_lib: Option<PathBuf>,
    cargo_target_dir: Option<PathBuf>,
}

impl Default for CompilerEnvironment {
    fn default() -> Self {
        Self {
            workers: Some(default_worker_count()),
            runtime_lib: None,
            cargo_target_dir: None,
        }
    }
}

impl CompilerEnvironment {
    fn read() -> Self {
        Self {
            workers: Some(
                parse_worker_count(std::env::var("WILLOW_WORKERS").ok().as_deref())
                    .unwrap_or_else(default_worker_count),
            ),
            runtime_lib: std::env::var_os("WILLOW_RUNTIME_LIB").map(PathBuf::from),
            cargo_target_dir: std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from),
        }
    }
}

impl CompilerOptions {
    pub fn debug() -> Self {
        Self {
            target: TargetOptions {
                build_mode: BuildMode::Debug,
                emit_debug_info: true,
                emit_source_map: true,
                strip_symbols: false,
                runtime_lib: None,
                cargo_target_dir: None,
            },
            worker_count: None,
            enforce_send_sync: true,
            locked: false,
            offline: false,
        }
    }

    pub fn release() -> Self {
        Self {
            target: TargetOptions {
                build_mode: BuildMode::Release,
                emit_debug_info: false,
                emit_source_map: false,
                strip_symbols: false,
                runtime_lib: None,
                cargo_target_dir: None,
            },
            worker_count: None,
            enforce_send_sync: true,
            locked: false,
            offline: false,
        }
    }

    pub fn release_with_debug_info() -> Self {
        Self {
            target: TargetOptions {
                build_mode: BuildMode::Release,
                emit_debug_info: true,
                emit_source_map: true,
                strip_symbols: false,
                runtime_lib: None,
                cargo_target_dir: None,
            },
            worker_count: None,
            enforce_send_sync: true,
            locked: false,
            offline: false,
        }
    }

    pub(crate) fn resolve_environment(self) -> Self {
        self.with_environment(CompilerEnvironment::read())
    }

    fn with_environment(mut self, environment: CompilerEnvironment) -> Self {
        self.worker_count = Some(
            self.worker_count
                .filter(|workers| *workers > 0)
                .or(environment.workers.filter(|workers| *workers > 0))
                .unwrap_or_else(default_worker_count),
        );
        // Type safety is independent of the runtime scheduling policy.
        self.enforce_send_sync = true;
        if self.target.runtime_lib.is_none() {
            self.target.runtime_lib = environment.runtime_lib;
        }
        if self.target.cargo_target_dir.is_none() {
            self.target.cargo_target_dir = environment.cargo_target_dir;
        }
        self
    }
}

#[cfg(test)]
mod compiler_options_tests {
    use super::*;

    #[test]
    fn debug_and_release_profiles_live_in_target_options() {
        let debug = CompilerOptions::debug();
        assert_eq!(debug.target.build_mode, BuildMode::Debug);
        assert!(debug.target.emit_debug_info);

        let release = CompilerOptions::release();
        assert_eq!(release.target.build_mode, BuildMode::Release);
        assert!(!release.target.emit_source_map);
    }

    #[test]
    fn multi_worker_environment_keeps_send_sync_checks() {
        let options = CompilerOptions::debug().with_environment(CompilerEnvironment {
            workers: Some(8),
            ..CompilerEnvironment::default()
        });
        assert_eq!(options.worker_count, Some(8));
        assert!(options.enforce_send_sync);
    }

    #[test]
    fn default_environment_uses_available_parallelism_and_checks() {
        let options = CompilerOptions::debug().with_environment(CompilerEnvironment::default());
        assert_eq!(options.worker_count, Some(default_worker_count()));
        assert!(options.enforce_send_sync);
    }

    #[test]
    fn single_worker_override_keeps_checks_enabled() {
        let options = CompilerOptions::debug().with_environment(CompilerEnvironment {
            workers: Some(1),
            ..CompilerEnvironment::default()
        });
        assert_eq!(options.worker_count, Some(1));
        assert!(options.enforce_send_sync);
    }

    #[test]
    fn worker_count_cannot_disable_type_safety() {
        for workers in [0, 1, 2, 8, 64] {
            let mut options = CompilerOptions::debug();
            options.worker_count = Some(workers);
            options.enforce_send_sync = false;
            let options = options.with_environment(CompilerEnvironment {
                workers: Some(3),
                ..CompilerEnvironment::default()
            });
            assert_eq!(
                options.worker_count,
                Some(if workers == 0 { 3 } else { workers })
            );
            assert!(options.enforce_send_sync);
        }
    }

    #[test]
    fn explicit_options_take_precedence_over_environment() {
        let mut options = CompilerOptions::debug();
        options.worker_count = Some(2);
        options.enforce_send_sync = true;
        options.target.runtime_lib = Some(PathBuf::from("explicit-runtime.a"));
        options.target.cargo_target_dir = Some(PathBuf::from("explicit-target"));
        let options = options.with_environment(CompilerEnvironment {
            workers: Some(8),
            runtime_lib: Some(PathBuf::from("environment-runtime.a")),
            cargo_target_dir: Some(PathBuf::from("environment-target")),
        });
        assert_eq!(options.worker_count, Some(2));
        assert_eq!(
            options.target.runtime_lib,
            Some(PathBuf::from("explicit-runtime.a"))
        );
        assert_eq!(
            options.target.cargo_target_dir,
            Some(PathBuf::from("explicit-target"))
        );
    }

    #[test]
    fn environment_paths_fill_unspecified_target_options() {
        let options = CompilerOptions::debug().with_environment(CompilerEnvironment {
            runtime_lib: Some(PathBuf::from("runtime.a")),
            cargo_target_dir: Some(PathBuf::from("target-dir")),
            ..CompilerEnvironment::default()
        });
        assert_eq!(options.target.runtime_lib, Some(PathBuf::from("runtime.a")));
        assert_eq!(
            options.target.cargo_target_dir,
            Some(PathBuf::from("target-dir"))
        );
    }

    #[test]
    fn worker_count_parser_rejects_invalid_values() {
        assert_eq!(parse_worker_count(Some("4")), Some(4));
        assert_eq!(parse_worker_count(Some(" 2 ")), Some(2));
        assert_eq!(parse_worker_count(Some("8")), Some(8));
        assert_eq!(parse_worker_count(Some("invalid")), None);
        assert_eq!(parse_worker_count(Some("0")), None);
        assert_eq!(parse_worker_count(None), None);
    }
}
