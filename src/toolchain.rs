//! Host object-file, runtime-library, linker, and sidecar artifact handling.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use anyhow::{Context, Result};

use crate::{BuildMode, TargetOptions};

/// Platform boundary used by the compiler after native object generation.
pub trait Toolchain {
    fn write_object(&self, output: &str, bytes: &[u8]) -> Result<PathBuf>;
    fn resolve_runtime_library(&self) -> Result<PathBuf>;
    fn link(&self, object: &Path, runtime: &Path, output: &str) -> Result<ExitStatus>;
    fn update_source_map(&self, output: &str, contents: Option<&str>) -> Result<()>;
}

/// Toolchain for the host Rust target.
pub struct HostToolchain {
    target: TargetOptions,
}

impl HostToolchain {
    pub fn new(target: &TargetOptions) -> Self {
        Self {
            target: target.clone(),
        }
    }

    fn object_path(&self, output: &str) -> PathBuf {
        if cfg!(all(windows, target_env = "msvc")) {
            PathBuf::from(format!("{output}.obj"))
        } else {
            PathBuf::from(format!("{output}.o"))
        }
    }

    fn default_runtime_library_path(&self) -> PathBuf {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let target_dir = self
            .target
            .cargo_target_dir
            .clone()
            .unwrap_or_else(|| manifest_dir.join("target"));
        let profile = if self.target.build_mode == BuildMode::Release {
            "release"
        } else {
            "debug"
        };
        target_dir.join(profile).join(if cfg!(target_env = "msvc") {
            "willow_runtime.lib"
        } else {
            "libwillow_runtime.a"
        })
    }

    fn build_default_runtime_library(&self) -> Result<()> {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut args = vec!["build", "-p", "willow_runtime"];
        if self.target.build_mode == BuildMode::Release {
            args.push("--release");
        }

        let mut command = Command::new("cargo");
        command.args(args).current_dir(&manifest_dir);
        if let Some(target_dir) = &self.target.cargo_target_dir {
            command.env("CARGO_TARGET_DIR", target_dir);
        }
        let status = command
            .status()
            .with_context(|| "failed to run cargo to build willow_runtime")?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("cargo failed to build willow_runtime")
        }
    }
}

impl Toolchain for HostToolchain {
    fn write_object(&self, output: &str, bytes: &[u8]) -> Result<PathBuf> {
        let path = self.object_path(output);
        std::fs::write(&path, bytes)?;
        Ok(path)
    }

    fn resolve_runtime_library(&self) -> Result<PathBuf> {
        if let Some(path) = &self.target.runtime_lib {
            return validate_runtime_library(path);
        }
        let path = self.default_runtime_library_path();
        self.build_default_runtime_library()?;
        validate_runtime_library(path)
    }

    fn link(&self, object: &Path, runtime: &Path, output: &str) -> Result<ExitStatus> {
        #[cfg(all(windows, target_env = "msvc"))]
        {
            let target = if cfg!(target_arch = "x86_64") {
                "x86_64-pc-windows-msvc"
            } else if cfg!(target_arch = "aarch64") {
                "aarch64-pc-windows-msvc"
            } else if cfg!(target_arch = "x86") {
                "i686-pc-windows-msvc"
            } else {
                anyhow::bail!("unsupported Windows MSVC target architecture");
            };
            let mut command = cc::windows_registry::find_tool(target, "cl.exe")
                .ok_or_else(|| anyhow::anyhow!("failed to find MSVC cl.exe"))?
                .to_command();
            command
                .arg("/nologo")
                .arg(object)
                .arg(runtime)
                .arg("/link")
                .arg(format!("/OUT:{output}"))
                .arg("/SUBSYSTEM:CONSOLE")
                .arg("legacy_stdio_definitions.lib")
                .arg("kernel32.lib")
                .arg("ntdll.lib")
                .arg("userenv.lib")
                .arg("ws2_32.lib")
                .arg("dbghelp.lib")
                .arg("/defaultlib:msvcrt");
            command
                .status()
                .with_context(|| "failed to run MSVC compiler driver")
        }

        #[cfg(not(all(windows, target_env = "msvc")))]
        {
            let mut command = Command::new("cc");
            command
                .arg(object)
                .arg(runtime)
                .arg("-o")
                .arg(output)
                .arg("-no-pie")
                .arg("-lm")
                .arg("-lpthread")
                .arg("-ldl");
            if self.target.strip_symbols {
                command.arg("-s");
            }
            command.status().with_context(|| "failed to run linker")
        }
    }

    fn update_source_map(&self, output: &str, contents: Option<&str>) -> Result<()> {
        let path = source_map_path(output);
        if let Some(contents) = contents {
            std::fs::write(path, contents)?;
        } else {
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }
}

fn validate_runtime_library(path: impl Into<PathBuf>) -> Result<PathBuf> {
    let path = path.into();
    if path.is_file() {
        Ok(path)
    } else {
        anyhow::bail!("{} does not exist or is not a file", path.display())
    }
}

fn source_map_path(output: &str) -> PathBuf {
    PathBuf::from(format!("{output}.wsmap"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_extension_matches_host_abi() {
        let toolchain = HostToolchain::new(&crate::CompilerOptions::debug().target);
        let path = toolchain.object_path("program");
        if cfg!(all(windows, target_env = "msvc")) {
            assert_eq!(path, PathBuf::from("program.obj"));
        } else {
            assert_eq!(path, PathBuf::from("program.o"));
        }
    }

    #[test]
    fn runtime_library_path_uses_profile_and_platform_name() {
        let mut target = crate::CompilerOptions::release().target;
        target.cargo_target_dir = Some(PathBuf::from("custom-target"));
        let path = HostToolchain::new(&target).default_runtime_library_path();
        assert!(path.starts_with("custom-target"));
        assert!(path.components().any(|part| part.as_os_str() == "release"));
        assert_eq!(
            path.file_name().unwrap(),
            if cfg!(target_env = "msvc") {
                "willow_runtime.lib"
            } else {
                "libwillow_runtime.a"
            }
        );
    }

    #[test]
    fn source_map_update_writes_and_removes_sidecar() {
        let output =
            std::env::temp_dir().join(format!("willow_toolchain_test_{}", std::process::id()));
        let output = output.to_string_lossy();
        let toolchain = HostToolchain::new(&crate::CompilerOptions::debug().target);
        toolchain
            .update_source_map(&output, Some("metadata"))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(source_map_path(&output)).unwrap(),
            "metadata"
        );
        toolchain.update_source_map(&output, None).unwrap();
        assert!(!source_map_path(&output).exists());
    }
}
