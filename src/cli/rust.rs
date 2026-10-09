//! Rust command adapter. Cargo resolution and wrapper ownership stay in toolchain.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use willow_compiler::project::{self, ProjectManifest, RustDependencySpec};
use willow_compiler::toolchain::rust_bridge::commands::{self, Mutation};
use willow_compiler::toolchain::rust_bridge::{BridgeOptions, build_bridge, inspect_bridge};
use willow_compiler::{BuildMode, CompilerOptions, toolchain::HostToolchain};

#[derive(Debug)]
pub(super) struct RustCommand {
    operation: String,
    selector: Option<String>,
    directory: PathBuf,
    format: String,
    options: BridgeOptions,
    mutation: Option<Box<Mutation>>,
}

impl RustCommand {
    pub(super) fn parse(command: &str, args: &[String]) -> Result<Self> {
        let (operation, args) = if command == "doctor" {
            ("doctor", args)
        } else {
            let operation = args
                .first()
                .context("expected rust add, remove, update, tree, check or metadata")?;
            ensure!(
                matches!(
                    operation.as_str(),
                    "check" | "metadata" | "tree" | "add" | "remove" | "update" | "rust-bridge"
                ),
                "unknown rust command `{operation}`"
            );
            (operation.as_str(), &args[1..])
        };
        let mut options = BridgeOptions::new(BuildMode::Debug)?;
        let mut directory = None;
        let mut format = None;
        let mut cache = None;
        let mut name = None;
        let mut spec = RustDependencySpec::default();
        let mut breaking = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let (key, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(k, v)| (k, Some(v)));
            match key {
                "--dry-run" if operation == "update" && inline.is_none() => {
                    ensure!(!options.read_only, "duplicate --dry-run");
                    options.read_only = true;
                }

                "--breaking" if operation == "update" && inline.is_none() => {
                    ensure!(!breaking, "duplicate --breaking");
                    breaking = true;
                }
                "--version" | "--git" | "--rev" | "--tag" | "--path" if operation == "add" => {
                    let value = inline
                        .or_else(|| args.next().map(String::as_str))
                        .context("missing dependency option value")?;
                    ensure!(
                        !value.is_empty() && !value.starts_with('-'),
                        "missing {key} value"
                    );
                    let slot = match key {
                        "--version" => &mut spec.version,
                        "--git" => &mut spec.git,
                        "--rev" => &mut spec.rev,
                        "--tag" => &mut spec.tag,
                        _ => &mut spec.path,
                    };
                    ensure!(slot.replace(value.to_owned()).is_none(), "duplicate {key}");
                }
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
                _ if matches!(operation, "add" | "remove" | "update" | "rust-bridge") => {
                    ensure!(
                        name.replace(arg.clone()).is_none(),
                        "too many dependency names"
                    );
                }
                _ => ensure!(
                    directory.replace(PathBuf::from(arg)).is_none(),
                    "duplicate project directory"
                ),
            }
        }
        if let Some(cache) = cache {
            options.cache_root = cache;
        }
        let selector = name.clone();
        let mutation = match operation {
            "add" => {
                let mut name = name.context("rust add requires a dependency name")?;
                if let Some((package, version)) = name.split_once('@') {
                    ensure!(
                        spec.version.replace(version.to_owned()).is_none(),
                        "duplicate version requirement"
                    );
                    name = package.to_owned();
                }
                spec.normalize(&name)?;
                Some(Mutation::Add { name, spec })
            }
            "remove" => Some(Mutation::Remove {
                name: name.context("rust remove requires a dependency name")?,
            }),
            "update" => Some(Mutation::Update { name, breaking }),
            _ => None,
        };
        Ok(Self {
            operation: operation.into(),
            selector,
            directory: directory.unwrap_or_else(|| ".".into()),
            format: format.unwrap_or_else(|| "human".into()),
            options,
            mutation: mutation.map(Box::new),
        })
    }

    pub(super) fn execute(mut self) -> Result<()> {
        let directory = std::fs::canonicalize(&self.directory)?;
        let (_, root) =
            project::find_project_manifest(&directory).context("no project.toml found")?;
        let project = ProjectManifest::load(&root.join("project.toml"))?;
        let mut enabled = !project.rust_dependencies.is_empty();
        let entry = project.entry_point(&root);
        let mut snapshot = if (self.operation == "check"
            || self.operation == "rust-bridge"
            || self.options.read_only)
            && (entry.try_exists()?
                || project.project.entry.is_some()
                || root.join("src").try_exists()?)
        {
            std::fs::metadata(&entry)
                .with_context(|| format!("cannot read {}", entry.display()))?;
            let mut options = CompilerOptions::debug();
            options.locked = self.options.locked;
            options.offline = self.options.offline;
            let snapshot = willow_compiler::CompilerSession::new(
                entry.to_str().context("non UTF-8 entry path")?,
                "",
                &options,
                Some(root.clone()),
            )
            .analysis_for_edit_with_emitter(&mut willow_compiler::diagnostics::HumanEmitter)?;
            self.options.symbols = snapshot.semantic.rust_bridges.clone();
            Some(snapshot)
        } else {
            None
        };
        if self.operation == "rust-bridge" {
            self.options.read_only = true;
            let graph = &mut snapshot
                .as_mut()
                .context("query rust-bridge requires Willow sources")?
                .semantic
                .interop;
            if enabled {
                let build = inspect_bridge(&project, &root, &self.options)?
                    .context("Rust bridge missing")?;
                graph.resolved_crates(&build.direct_dependencies)?;
            }
            let value = graph.query(self.selector.as_deref());
            super::write_machine_output(&value)?;
            return Ok(());
        }
        let mut value = if let Some(mutation) = self.mutation {
            let mut value = commands::mutate(&root, &self.options, *mutation)?;
            if self.options.read_only {
                if let Some(snapshot) = &mut snapshot {
                    snapshot.semantic.interop.resolved_crates(&value["from"])?;
                    let graph = &snapshot.semantic.interop;
                    let bridges: Vec<_> = graph.bridges.iter().collect();
                    let index = willow_compiler::ai::interop::CallerIndex::new(snapshot);
                    let mut impact = index.affected(&bridges);
                    let mut bridge_query = graph.query(None);
                    value["bridge_symbols"] = bridge_query["bridges"].take();
                    value["crate_candidates"] = bridge_query["crate_candidates"].take();
                    value["affected_willow_callers"] = impact
                        .as_object_mut()
                        .unwrap()
                        .remove("affected_willow_callers")
                        .unwrap();
                    value["impact"] = impact;
                } else {
                    value["bridge_symbols"] = json!([]);
                    value["affected_willow_callers"] = json!([]);
                    value["coverage"] = json!("no Willow sources available");
                }
            }
            value
        } else if self.operation == "doctor" {
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
                build_bridge(&project, &root, &self.options, true).map_err(|mut error| {
                    if let Some(details) =
                        error.downcast_mut::<willow_compiler::package::CommandError>()
                        && let Some(snapshot) = &snapshot
                    {
                        let graph = &snapshot.semantic.interop;
                        details
                            .fields
                            .insert("bridge_declarations".into(), graph.query(None));
                        let bridges: Vec<_> = graph.bridges.iter().collect();
                        details.fields.insert(
                            "willow_callers".into(),
                            willow_compiler::ai::interop::CallerIndex::new(snapshot)
                                .affected(&bridges),
                        );
                    }
                    error
                })?
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
                if self.operation == "tree" {
                    value["tree"] = commands::tree(&build.metadata);
                }
            }
            value
        };
        enabled = value["enabled"].as_bool().unwrap_or(enabled);
        // Explicit inspection surfaces carry the notice on every invocation;
        // build/run retain their streaming diagnostic protocol unchanged.
        let notice = enabled && matches!(self.operation.as_str(), "doctor" | "metadata");
        if notice {
            value["rust_dependency_build_execution_notice"] = json!({"build_time_code_execution_possible":true,
                "message":"Cargo builds may execute dependency build.rs scripts and procedural macros."});
        }
        if self.format == "human" {
            if notice {
                println!(
                    "note: Cargo builds may execute dependency build.rs scripts and procedural macros."
                );
            }
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
                if self.operation == "tree" && !value["tree"].is_null() {
                    print!("{}", commands::display_tree(&value["tree"])?);
                }
                for diagnostic in value["diagnostics"].as_array().into_iter().flatten() {
                    if diagnostic["kind"] == "rust_bridge_unused_declaration_candidate" {
                        println!(
                            "note: {} ({})",
                            diagnostic["message"].as_str().unwrap(),
                            diagnostic["dependency"].as_str().unwrap()
                        );
                    }
                }
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
