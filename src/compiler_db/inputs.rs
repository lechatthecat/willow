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
    pub(crate) capture_analysis: bool,
    pub options: crate::CompilerOptions,
    pub project_root: PathBuf,
    /// An explicitly selected project, including legacy manifests without packages.
    pub(crate) project_mode: bool,
    pub target: TargetCapabilities,
    pub package_graph: Option<std::sync::Arc<crate::package::PackageGraph>>,
    pub root_package: Option<crate::package::PackageId>,
}

impl CompilerInputs {
    pub(crate) fn resolve_project(
        mut self,
        root: Option<&std::path::Path>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            root.is_some() || !(self.options.locked || self.options.offline),
            "`--locked` requires project mode (also --offline/--frozen)"
        );
        if let Some(root) = root {
            self.project_mode = true;
            // Lock all projects, while preserving legacy compiler identities.
            if let Some(graph) = crate::package::resolve_project_packages(
                root,
                self.options.locked,
                self.options.offline,
            )? {
                self = self.with_packages(std::sync::Arc::new(graph));
            }
        }
        Ok(self)
    }

    pub fn with_packages(mut self, graph: std::sync::Arc<crate::package::PackageGraph>) -> Self {
        self.project_mode = true;
        self.root_package = Some(graph.root);
        self.package_graph = Some(graph);
        self
    }

    pub fn native(options: crate::CompilerOptions, project_root: PathBuf) -> Self {
        Self {
            capture_analysis: false,
            options,
            project_root,
            project_mode: false,
            target: TargetCapabilities::native(),
            package_graph: None,
            root_package: None,
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
