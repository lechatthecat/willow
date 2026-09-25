use std::path::PathBuf;
use target_lexicon::Triple;

/// Runtime capabilities belong to the compilation target, not the machine
/// running a semantic query. The native backend currently selects HOST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetCapabilities {
    pub sync_stack_preemption: bool,
}

impl TargetCapabilities {
    pub fn for_triple(target: &Triple) -> Self {
        let arch = target.architecture.to_string();
        let os = target.operating_system.to_string();
        let env = target.environment.to_string();
        let native_arch = arch == "x86_64" || arch == "aarch64";
        Self {
            sync_stack_preemption: native_arch
                && ((os == "linux" && env == "gnu")
                    || os == "darwin"
                    || os == "macosx"
                    || (os == "windows" && env == "msvc" && arch == "x86_64")),
        }
    }

    pub fn native() -> Self {
        Self::for_triple(&target_lexicon::HOST)
    }
}

#[derive(Debug, Clone)]
pub struct CompilerInputs {
    pub options: crate::CompilerOptions,
    pub project_root: PathBuf,
    pub target: TargetCapabilities,
}

impl CompilerInputs {
    pub fn native(options: crate::CompilerOptions, project_root: PathBuf) -> Self {
        Self {
            options,
            project_root,
            target: TargetCapabilities::native(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capabilities_follow_target_instead_of_host() {
        for (triple, supported) in [
            ("x86_64-unknown-linux-gnu", true),
            ("aarch64-unknown-linux-gnu", true),
            ("x86_64-apple-darwin", true),
            ("aarch64-apple-darwin", true),
            ("x86_64-pc-windows-msvc", true),
            ("aarch64-pc-windows-msvc", false),
            ("x86_64-pc-windows-gnu", false),
            ("x86_64-unknown-linux-musl", false),
            ("wasm32-unknown-unknown", false),
        ] {
            assert_eq!(
                TargetCapabilities::for_triple(&triple.parse().unwrap()).sync_stack_preemption,
                supported,
                "{triple}"
            );
        }
    }
}
