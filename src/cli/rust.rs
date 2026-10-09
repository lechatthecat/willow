//! Rust command adapter. Cargo resolution and wrapper ownership stay in toolchain.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use willow_compiler::project::{self, ProjectManifest};
use willow_compiler::toolchain::rust_bridge::{BridgeOptions, build_bridge, inspect_bridge};
use willow_compiler::{BuildMode, CompilerOptions, toolchain::HostToolchain};

#[derive(Debug)]
pub(super) struct RustCommand {
    operation: String,
    directory: PathBuf,
    format: String,
    options: BridgeOptions,
}

impl RustCommand {
    pub(super) fn parse(command: &str, args: &[String]) -> Result<Self> {
        let (operation, args) = if command == "doctor" {
            ("doctor", args)
        } else {
            let operation = args
                .first()
                .context("expected rust check or rust metadata")?;
            ensure!(
                matches!(operation.as_str(), "check" | "metadata"),
                "unknown rust command `{operation}`"
            );
            (operation.as_str(), &args[1..])
        };
        let mut options = BridgeOptions::new(BuildMode::Debug)?;
        let mut directory = None;
        let mut format = None;
        let mut cache = None;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let (key, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
            match key {
                "--format" | "--project-dir" | "--cache-dir" => {
                    let value = inline
                        .or_else(|| args.next().map(String::as_str))
                        .with_context(|| format!("missing {key} value"))?;
                    ensure!(
                        !value.is_empty() && !value.starts_with('-'),
                        "missing {key} value"
                    );
                    match key {
                        "--format" => {
                            ensure!(
                                matches!(value, "human" | "json" | "ndjson"),
                                "invalid output format `{value}`"
                            );
                            ensure!(
                                format.replace(value.to_owned()).is_none(),
                                "duplicate --format"
                            );
                        }
                        "--project-dir" => ensure!(
                            directory.replace(PathBuf::from(value)).is_none(),
                            "duplicate project directory"
                        ),
                        _ => ensure!(
                            cache.replace(PathBuf::from(value)).is_none(),
                            "duplicate --cache-dir"
                        ),
                    }
                }
                "--offline" if inline.is_none() => options.offline = true,
                "--locked" if inline.is_none() => options.locked = true,
                "--frozen" if inline.is_none() => {
                    options.offline = true;
                    options.locked = true;
                }
                _ if arg.starts_with('-') => anyhow::bail!("unknown {operation} option `{arg}`"),
                _ => ensure!(
                    directory.replace(PathBuf::from(arg)).is_none(),
                    "duplicate project directory"
                ),
            }
        }
        if let Some(cache) = cache {
            options.cache_root = cache;
        }
        Ok(Self {
            operation: operation.into(),
            directory: directory.unwrap_or_else(|| ".".into()),
            format: format.unwrap_or_else(|| "human".into()),
            options,
        })
    }

    pub(super) fn execute(self) -> Result<()> {
        let directory = std::fs::canonicalize(&self.directory)?;
        let (_, root) =
            project::find_project_manifest(&directory).context("no project.toml found")?;
        let project = ProjectManifest::load(&root.join("project.toml"))?;
        let enabled = !project.rust_dependencies.is_empty();
        let value = if self.operation == "doctor" {
            let host = HostToolchain::new(&CompilerOptions::debug().target);
            let runtime = host.runtime_library_status();
            let linker = host.native_linker_status();
            let cargo = probe(&self.options.cargo, "--version", &root, enabled);
            let rustc = probe(&self.options.rustc, "-vV", &root, enabled);
            let ok = runtime["available"] == true
                && linker["available"] == true
                && (!enabled || (cargo["available"] == true && rustc["available"] == true));
            json!({"schema":1, "ok":ok, "kind":"doctor", "runtime":runtime,
                "native_linker":linker, "rust_interop":{"enabled":enabled}, "cargo":cargo, "rustc":rustc})
        } else {
            let build = if self.operation == "check" {
                build_bridge(&project, &root, &self.options, true)?
            } else {
                inspect_bridge(&project, &root, &self.options)?
            };
            let mut value = json!({"schema":1, "ok":true, "kind":format!("rust.{}", self.operation), "enabled":enabled,
                "toolchain":{"cargo":null,"rustc":null}, "direct_dependencies":[], "bridge":{"source":null,"artifact":null}, "diagnostics":[]});
            if let Some(build) = build {
                value["toolchain"] = json!({"cargo":build.toolchain.cargo_version, "rustc":build.toolchain.rustc_version});
                value["direct_dependencies"] = build.direct_dependencies;
                value["bridge"] = json!({"source":project.rust.as_ref().unwrap().resolve_bridge(&root)?, "artifact":build.staticlib,
                    "manifest":build.directory.join("Cargo.toml")});
                value["diagnostics"] = json!(build.messages.diagnostics);
            }
            value
        };
        if self.format == "human" {
            if self.operation == "doctor" {
                println!("Rust interop enabled: {enabled}");
                for name in ["runtime", "native_linker", "cargo", "rustc"] {
                    println!(
                        "{name}: {} ({})",
                        if value[name]["available"] == true {
                            "available"
                        } else {
                            "missing"
                        },
                        if value[name]["required"] == true {
                            "required"
                        } else {
                            "optional"
                        }
                    );
                }
            } else {
                println!(
                    "Rust {}: {}",
                    self.operation,
                    if enabled {
                        "ok"
                    } else {
                        "disabled (no Rust dependencies)"
                    }
                );
                for dependency in value["direct_dependencies"].as_array().unwrap() {
                    println!(
                        "{} {} ({})",
                        dependency["alias"].as_str().unwrap(),
                        dependency["version"].as_str().unwrap(),
                        dependency["name"].as_str().unwrap()
                    );
                }
            }
        } else {
            super::write_machine_output(&value)?;
        }
        if value["ok"] == false {
            std::process::exit(1);
        }
        Ok(())
    }
}

fn probe(program: &Path, argument: &str, root: &Path, required: bool) -> Value {
    match Command::new(program)
        .arg(argument)
        .current_dir(root)
        .output()
    {
        Ok(output) => {
            json!({"required":required, "available":output.status.success(), "program":program,
            "version":String::from_utf8_lossy(&output.stdout).trim(), "stderr":String::from_utf8_lossy(&output.stderr)})
        }
        Err(error) => {
            json!({"required":required,"available":false,"program":program,"error":error.to_string()})
        }
    }
}
