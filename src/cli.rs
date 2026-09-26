use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};
use willow_compiler::{CompilerOptions, compile, emit_hir_text, emit_lir_text, project};

mod analysis;
mod daemon;
mod edit;
mod package;
mod protocol;

#[derive(Debug)]
enum CliCommand {
    Analysis(Box<analysis::AnalysisCommand>),
    Edit(edit::EditCommand),
    Build(BuildCommand),
    Check(BuildCommand),
    Run(RunCommand),
    Debug(DebugCommand),
    Fetch(FetchCommand),
    Verify(FetchCommand),
    Package(package::PackageCommand),
}

#[derive(Debug)]
struct BuildCommand {
    source: Option<String>,
    project_dir: Option<PathBuf>,
    output: Option<String>,
    emit_hir: bool,
    emit_lir: bool,
    options: CompilerOptions,
}

#[derive(Debug)]
struct RunCommand {
    source: String,
    program_args: Vec<String>,
    options: CompilerOptions,
}

#[derive(Debug)]
struct DebugCommand {
    source: String,
    options: CompilerOptions,
}

#[derive(Default)]
struct CompilerFlags {
    debug: bool,
    locked: bool,
    offline: bool,
    release: bool,
    debug_info: bool,
    emit_hir: bool,
    emit_lir: bool,
    runtime_lib: Option<PathBuf>,
}

impl CompilerFlags {
    fn parse(&mut self, args: &[String], index: &mut usize) -> Result<bool> {
        let arg = &args[*index];
        match arg.as_str() {
            "--debug" => self.debug = true,
            "--locked" => self.locked = true,
            "--offline" => self.offline = true,
            "--frozen" => {
                self.locked = true;
                self.offline = true;
            }
            "--release" => self.release = true,
            "--debug-info" => self.debug_info = true,
            "--emit-hir" => self.emit_hir = true,
            "--emit-lir" => self.emit_lir = true,
            "--runtime-lib" => {
                *index += 1;
                let path = args
                    .get(*index)
                    .ok_or_else(|| anyhow::anyhow!("missing value for `--runtime-lib`"))?;
                self.runtime_lib = Some(PathBuf::from(path));
            }
            _ => {
                if let Some(path) = arg.strip_prefix("--runtime-lib=") {
                    if path.is_empty() {
                        anyhow::bail!("missing value for `--runtime-lib`");
                    }
                    self.runtime_lib = Some(PathBuf::from(path));
                } else {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn finish(self) -> Result<CompilerOptions> {
        if self.debug && self.release {
            anyhow::bail!("`--debug` and `--release` cannot be used together");
        }
        if self.debug_info && !self.release {
            anyhow::bail!("`--debug-info` requires `--release`");
        }

        let mut options = if self.release {
            if self.debug_info {
                CompilerOptions::release_with_debug_info()
            } else {
                CompilerOptions::release()
            }
        } else {
            CompilerOptions::debug()
        };
        options.target.runtime_lib = self.runtime_lib;
        options.locked = self.locked;
        options.offline = self.offline;
        Ok(options)
    }
}

impl CliCommand {
    fn parse(args: &[String]) -> Result<Self> {
        let Some(command) = args.first() else {
            anyhow::bail!("missing command or source file\n\n{}", usage());
        };

        match command.as_str() {
            "edit" => Ok(Self::Edit(edit::EditCommand::parse(&args[1..])?)),
            "impact" | "snapshot" | "risk" | "query" => Ok(Self::Analysis(Box::new(
                analysis::AnalysisCommand::parse(args)?,
            ))),
            "add" | "remove" | "update" | "deps" | "metadata" => Ok(Self::Package(
                package::PackageCommand::parse(command, &args[1..])?,
            )),
            "check" => {
                let command = BuildCommand::parse(&args[1..])?;
                anyhow::ensure!(
                    !command.emit_hir && !command.emit_lir && command.output.is_none(),
                    "check does not emit artifacts"
                );
                Ok(Self::Check(command))
            }
            "build" => Ok(Self::Build(BuildCommand::parse(&args[1..])?)),
            "run" => Ok(Self::Run(RunCommand::parse(&args[1..])?)),
            "package" if args.get(1).is_some_and(|arg| arg == "verify") => {
                let command = FetchCommand::parse_named(&args[2..], "package verify")?;
                anyhow::ensure!(
                    !command.locked && !command.offline,
                    "package verify resolves independently of project.lock"
                );
                Ok(Self::Verify(command))
            }
            "fetch" => Ok(Self::Fetch(FetchCommand::parse(&args[1..])?)),
            "debug" => Ok(Self::Debug(DebugCommand::parse(&args[1..])?)),
            source if source.ends_with(".wi") => Ok(Self::Build(BuildCommand::parse(args)?)),
            unknown => anyhow::bail!("unknown command `{unknown}`\n\n{}", usage()),
        }
    }

    fn execute(self) -> Result<()> {
        match self {
            Self::Edit(command) => {
                println!(
                    "{}",
                    command.execute(&mut willow_compiler::diagnostics::HumanEmitter)?
                );
                Ok(())
            }
            Self::Analysis(command) => {
                let value = command.execute(&mut willow_compiler::diagnostics::HumanEmitter)?;
                println!("{}", serde_json::to_string_pretty(&value)?);
                Ok(())
            }
            Self::Package(command) => command.execute(),
            Self::Build(command) => command.execute(),
            Self::Check(command) => {
                command.execute_with(&mut willow_compiler::diagnostics::HumanEmitter, false)
            }
            Self::Run(command) => command.execute(),
            Self::Debug(command) => command.execute(),
            Self::Fetch(command) => command.execute(),
            Self::Verify(command) => {
                let report = willow_compiler::package::verify_package(&command.directory);
                if command.format == "human" {
                    print!("{}", report.human());
                } else {
                    println!("{}", serde_json::to_string(&report)?);
                }
                if !report.ok {
                    std::process::exit(1);
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug)]
struct FetchCommand {
    directory: PathBuf,
    locked: bool,
    offline: bool,
    format: String,
}
impl FetchCommand {
    fn parse(args: &[String]) -> Result<Self> {
        Self::parse_named(args, "fetch")
    }
    fn parse_named(args: &[String], command: &str) -> Result<Self> {
        let mut directory = None;
        let mut locked = false;
        let mut offline = false;
        let mut format = "human".to_string();
        let mut format_seen = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--locked" => locked = true,
                "--offline" => offline = true,
                "--frozen" => {
                    locked = true;
                    offline = true;
                }
                value if value == "--format" || value.starts_with("--format=") => {
                    anyhow::ensure!(!format_seen, "duplicate --format");
                    format_seen = true;
                    format = if value == "--format" {
                        args.next().context("missing --format value")?.clone()
                    } else {
                        value[9..].into()
                    };
                }
                option if option.starts_with('-') => {
                    anyhow::bail!("unknown {command} option `{option}`")
                }
                value => {
                    anyhow::ensure!(
                        directory.replace(PathBuf::from(value)).is_none(),
                        "{command} accepts one project directory"
                    );
                }
            }
        }
        anyhow::ensure!(
            matches!(format.as_str(), "human" | "json" | "ndjson"),
            "invalid {command} format `{format}`"
        );
        Ok(Self {
            directory: directory.unwrap_or_else(|| PathBuf::from(".")),
            locked,
            offline,
            format,
        })
    }
    fn execute(self) -> Result<()> {
        let result = (|| {
            let (_, root) = project::find_project_manifest(&self.directory).ok_or_else(|| {
                willow_compiler::package::CommandError::new(
                    "not_willow_package",
                    "no project.toml found",
                )
            })?;
            willow_compiler::package::fetch_packages(&root, self.locked, self.offline)
        })();
        let graph = result?;
        if self.format == "human" {
            println!("Fetched {} dependency packages", graph.packages.len() - 1);
        } else {
            let mut metadata = graph.metadata();
            metadata.kind = "package.fetch";
            let mut value = serde_json::to_value(&metadata)?;
            value["dependencies"] = serde_json::json!(graph.packages.len() - 1);
            println!("{value}");
        }
        Ok(())
    }
}

impl BuildCommand {
    fn parse(args: &[String]) -> Result<Self> {
        let mut flags = CompilerFlags::default();
        let mut input = None;
        let mut output = None;
        let mut index = 0;

        while index < args.len() {
            if flags.parse(args, &mut index)? {
                index += 1;
                continue;
            }
            match args[index].as_str() {
                "-o" => {
                    index += 1;
                    output = Some(
                        args.get(index)
                            .ok_or_else(|| anyhow::anyhow!("missing value for `-o`"))?
                            .clone(),
                    );
                }
                option if option.starts_with('-') => {
                    anyhow::bail!("unknown build option `{option}`")
                }
                value => {
                    if input.replace(value.to_string()).is_some() {
                        anyhow::bail!("build accepts only one source file or project directory");
                    }
                }
            }
            index += 1;
        }

        let (source, project_dir) = match input {
            Some(value) if value.ends_with(".wi") => (Some(value), None),
            Some(value) => (None, Some(PathBuf::from(value))),
            None => (None, None),
        };
        let emit_hir = flags.emit_hir;
        let emit_lir = flags.emit_lir;
        anyhow::ensure!(
            !((flags.locked || flags.offline) && source.is_some()),
            "`--locked` requires project mode (also --offline/--frozen)"
        );
        anyhow::ensure!(
            !(emit_hir || emit_lir) || source.is_some(),
            "`--emit-hir`/`--emit-lir` require a `.wi` source file"
        );
        Ok(Self {
            source,
            project_dir,
            output,
            emit_hir,
            emit_lir,
            options: flags.finish()?,
        })
    }

    fn execute(self) -> Result<()> {
        self.execute_with(&mut willow_compiler::diagnostics::HumanEmitter, true)
    }

    fn execute_with(
        self,
        emitter: &mut dyn willow_compiler::diagnostics::DiagnosticEmitter,
        build: bool,
    ) -> Result<()> {
        if (self.options.locked || self.options.offline) && self.source.is_some() {
            anyhow::bail!("`--locked` requires project mode (also --offline/--frozen)");
        }
        if self.emit_hir || self.emit_lir {
            let Some(source) = self.source.as_deref() else {
                anyhow::bail!("`--emit-hir`/`--emit-lir` require a `.wi` source file");
            };
            if self.emit_hir {
                print!("{}", emit_hir_text(source)?);
            }
            if self.emit_lir {
                print!("{}", emit_lir_text(source)?);
            }
            return Ok(());
        }
        if let Some(source) = self.source {
            let output = self.output.unwrap_or_else(|| stem(&source));
            return execute_compiler(&source, &output, &self.options, None, emitter, build);
        }

        let search_dir = self
            .project_dir
            .unwrap_or(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let (manifest_path, project_root) = project::find_project_manifest(&search_dir)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no source file or project.toml found (searched from {})",
                    search_dir.display()
                )
            })?;
        let manifest = project::ProjectManifest::load(&manifest_path)?;
        let entry = manifest.entry_point(&project_root);
        if !entry.exists() {
            anyhow::bail!(
                "entry point not found: {} (declared in {})",
                entry.display(),
                manifest_path.display()
            );
        }

        let output = self.output.unwrap_or_else(|| manifest.project.name.clone());
        eprintln!(
            "{} project '{}' v{}",
            if build { "building" } else { "checking" },
            manifest.project.name,
            manifest.project.version
        );
        execute_compiler(
            entry.to_str().context("entry path is not UTF-8")?,
            &output,
            &self.options,
            Some(project_root),
            emitter,
            build,
        )
    }
}

impl RunCommand {
    fn parse(args: &[String]) -> Result<Self> {
        let separator = args.iter().position(|arg| arg == "--");
        let (compiler_args, program_args) = match separator {
            Some(index) => (&args[..index], args[index + 1..].to_vec()),
            None => (args, vec![]),
        };
        let mut flags = CompilerFlags::default();
        let mut source = None;
        let mut index = 0;
        while index < compiler_args.len() {
            if flags.parse(compiler_args, &mut index)? {
                index += 1;
                continue;
            }
            let arg = &compiler_args[index];
            if arg.starts_with('-') {
                anyhow::bail!("unknown run option `{arg}`");
            }
            if source.replace(arg.clone()).is_some() {
                anyhow::bail!("run accepts only one source file or project directory");
            }
            index += 1;
        }
        Ok(Self {
            source: source.unwrap_or_else(|| ".".into()),
            program_args,
            options: flags.finish()?,
        })
    }

    fn execute(self) -> Result<()> {
        let temporary = RunDirectory::create()?;
        let output = temporary.0.join(if cfg!(windows) {
            "program.exe"
        } else {
            "program"
        });
        let output = output
            .to_str()
            .context("run output path is not UTF-8")?
            .to_owned();
        if self.source.ends_with(".wi") {
            anyhow::ensure!(
                !(self.options.locked || self.options.offline),
                "`--locked` requires project mode (also --offline/--frozen)"
            );
            compile(&self.source, &output, &self.options, None)?;
        } else {
            BuildCommand {
                source: None,
                project_dir: Some(PathBuf::from(&self.source)),
                output: Some(output.clone()),
                emit_hir: false,
                emit_lir: false,
                options: self.options,
            }
            .execute()?;
        }
        let status = Command::new(&output)
            .args(&self.program_args)
            .status()
            .with_context(|| format!("failed to run {output}"))?;
        drop(temporary);
        std::process::exit(child_exit_code(status));
    }
}

/// Isolate concurrent project runs and clean compiler artifacts before exit.
struct RunDirectory(PathBuf);
impl RunDirectory {
    fn create() -> Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        loop {
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("willow_run_{}_{sequence}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}
impl Drop for RunDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl DebugCommand {
    fn parse(args: &[String]) -> Result<Self> {
        let mut flags = CompilerFlags::default();
        let mut source = None;
        let mut index = 0;
        while index < args.len() {
            if flags.parse(args, &mut index)? {
                index += 1;
                continue;
            }
            let arg = &args[index];
            if arg.starts_with('-') {
                anyhow::bail!("unknown debug option `{arg}`");
            }
            if !arg.ends_with(".wi") {
                anyhow::bail!("unexpected debug argument `{arg}`; expected a `.wi` source file");
            }
            if source.replace(arg.clone()).is_some() {
                anyhow::bail!("debug accepts exactly one source file");
            }
            index += 1;
        }
        if flags.locked || flags.offline {
            anyhow::bail!("debug command does not accept --locked/--offline/--frozen");
        }
        if flags.release || flags.debug_info {
            anyhow::bail!("debug command does not accept release-mode options");
        }
        Ok(Self {
            source: source.ok_or_else(|| anyhow::anyhow!("no source file specified"))?,
            options: flags.finish()?,
        })
    }

    fn execute(self) -> Result<()> {
        let output = temp_path(format!("willow_debug_{}", stem(&self.source)));
        compile(&self.source, &output, &self.options, None)?;
        eprintln!("note: interactive debugger not yet implemented");
        eprintln!("running in debug mode: {output}");
        let status = Command::new(&output)
            .status()
            .with_context(|| format!("failed to run {output}"))?;
        std::process::exit(child_exit_code(status));
    }
}

fn child_exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if let Some(signal) = status.signal() {
            // Preserve signal failure using the shell exit-code convention.
            return 128 + signal;
        }
    }
    status.code().unwrap_or(0)
}

pub(super) fn run(args: Vec<String>) -> Result<()> {
    if protocol::requested(&args) {
        let code = protocol::run(args)?;
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    let machine_package = args.first().is_some_and(|s| {
        matches!(
            s.as_str(),
            "add" | "remove" | "update" | "deps" | "metadata" | "fetch" | "package"
        )
    }) && args.iter().enumerate().any(|(i, arg)| {
        matches!(arg.as_str(), "--format=json" | "--format=ndjson")
            || (arg == "--format"
                && args
                    .get(i + 1)
                    .is_some_and(|v| matches!(v.as_str(), "json" | "ndjson")))
    });
    let result = CliCommand::parse(&args).and_then(CliCommand::execute);
    if machine_package && let Err(error) = &result {
        println!("{}", willow_compiler::package::package_error_json(error));
        std::process::exit(1);
    }
    result
}

fn usage() -> &'static str {
    "Usage:\n  willow metadata [project-dir] [--format human|json|ndjson]\n  Package commands accept --format human|json|ndjson (default human).\n  willow edit prepare --root DIR --entry main.wi --requests edits.json\n  willow edit <preview|validate|apply|recover> --root DIR --transaction ID\n  willow daemon <source.wi|project-dir>\n  willow risk --before baseline.json --after current.json\n  willow query <source.wi|project-dir> --requests queries.json\n  willow check <source.wi|project-dir> [--format human|ndjson]\n  willow build <source.wi|project-dir> [--format human|ndjson] [--protocol-version 1]\n  willow add [alias] --git URL [--version REQ] [--dry-run] [--project-dir DIR]\n  willow add [alias] --path DIR [--dry-run] [--project-dir DIR]\n  willow add URL [--dry-run] [--project-dir DIR]\n  willow remove alias [--dry-run] [--project-dir DIR]\n  willow update [alias] [--breaking] [--dry-run] [--project-dir DIR]\n  willow deps tree [--project-dir DIR]\n  willow deps why <alias|package-name> [--project-dir DIR]\n  willow package verify [PATH] [--format human|json|ndjson]\n  willow build <source.wi|project-dir> [-o <output>] [--locked|--offline|--frozen] [--debug|--release] [--debug-info] [--emit-hir] [--emit-lir] [--runtime-lib <path>]\n  willow run [source.wi|project-dir] [--locked|--offline|--frozen] [--debug|--release] [--debug-info] [--runtime-lib <path>] [-- <args>...]\n  willow fetch [project-dir] [--locked|--offline|--frozen] [--format human|json|ndjson]\n  willow debug <source.wi> [--runtime-lib <path>]"
}

fn stem(path: &str) -> String {
    PathBuf::from(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("a")
        .to_string()
}

fn temp_path(path: impl AsRef<std::path::Path>) -> String {
    std::env::temp_dir()
        .join(path)
        .to_string_lossy()
        .into_owned()
}

fn execute_compiler(
    src: &str,
    out: &str,
    options: &CompilerOptions,
    root: Option<PathBuf>,
    emitter: &mut dyn willow_compiler::diagnostics::DiagnosticEmitter,
    build: bool,
) -> Result<()> {
    let session = willow_compiler::CompilerSession::new(src, out, options, root);
    if build {
        session.run_with_emitter(emitter)
    } else {
        session.check_with_emitter(emitter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[cfg(unix)]
    #[test]
    fn child_exit_code_preserves_normal_exits() {
        use std::os::unix::process::ExitStatusExt;

        for code in [0, 1, 42, 255] {
            assert_eq!(child_exit_code(ExitStatus::from_raw(code << 8)), code);
        }
    }

    #[cfg(unix)]
    #[test]
    fn child_exit_code_reports_signal_deaths() {
        use std::os::unix::process::ExitStatusExt;

        // SIGABRT, SIGKILL, SIGSEGV, SIGTERM, and a core-dumping SIGSEGV.
        for raw in [6, 9, 11, 15, 11 | 0x80] {
            let status = ExitStatus::from_raw(raw);
            assert_eq!(child_exit_code(status), 128 + (raw & 0x7f));
        }
    }

    #[test]
    fn typed_cli_parser_covers_twenty_one_perspectives() {
        let cases: &[(&[&str], bool)] = &[
            (&["build", "main.wi"], true),              // 01 source build
            (&["build"], true),                         // 02 current-dir project
            (&["build", "project"], true),              // 03 explicit project dir
            (&["build", "main.wi", "-o", "app"], true), // 04 output
            (&["build", "main.wi", "--debug"], true),   // 05 explicit debug
            (&["build", "main.wi", "--release"], true), // 06 release
            (&["build", "main.wi", "--release", "--debug-info"], true), // 07 release debug info
            (&["build", "main.wi", "--runtime-lib", "rt.a"], true), // 08 runtime path
            (&["build", "main.wi", "--runtime-lib=rt.a"], true), // 09 equals runtime path
            (&["build", "main.wi", "--debug", "--release"], false), // 10 conflicting modes
            (&["build", "main.wi", "-o"], false),       // 11 missing output
            (&["build", "main.wi", "--runtime-lib"], false), // 12 missing runtime
            (&["build", "main.wi", "--wat"], false),    // 13 unknown option
            (&["build", "a.wi", "b.wi"], false),        // 14 duplicate input
            (&["run", "main.wi"], true),                // 15 run source
            (&["run", "main.wi", "--", "x", "--flag"], true), // 16 program args
            (&["run"], true),                           // 17 current-dir project run
            (&["run", "project"], true),                // 18 explicit project run
            (&["debug", "main.wi"], true),              // 19 debug source
            (&["debug", "main.wi", "--release"], false), // 20 invalid debug mode
            (&["main.wi", "-o", "app"], true),          // 21 legacy build
        ];
        for (case, expected) in cases {
            assert_eq!(
                CliCommand::parse(&args(case)).is_ok(),
                *expected,
                "case: {case:?}"
            );
        }
    }

    #[test]
    fn package_flags_and_fetch_parser() {
        for name in ["build", "run"] {
            let command = CliCommand::parse(&args(&[name, "--frozen"])).unwrap();
            let options = match command {
                CliCommand::Build(c) => c.options,
                CliCommand::Run(c) => c.options,
                _ => unreachable!(),
            };
            assert!(options.locked && options.offline);
        }
        let CliCommand::Fetch(fetch) =
            CliCommand::parse(&args(&["fetch", "dir", "--frozen", "--format=json"])).unwrap()
        else {
            unreachable!()
        };
        assert!(fetch.locked && fetch.offline);
        assert_eq!(fetch.directory, PathBuf::from("dir"));
        assert_eq!(fetch.format, "json");
        for input in [
            &["fetch", "--format"][..],
            &["fetch", "--format", "xml"],
            &["fetch", "a", "b"],
            &["fetch", "--release"],
            &["debug", "main.wi", "--offline"],
        ] {
            assert!(CliCommand::parse(&args(input)).is_err(), "{input:?}");
        }
    }

    #[test]
    fn verify_parser_rejects_extra_paths_flags_and_formats() {
        for input in [
            &["package", "verify", "a", "b"][..],
            &["package", "verify", "--format"],
            &["package", "verify", "--format=xml"],
            &["package", "verify", "--locked"],
            &["package", "verify", "--offline"],
            &["package", "verify", "--frozen"],
            &["package", "verify", "--release"],
            &["package", "unknown"],
        ] {
            assert!(CliCommand::parse(&args(input)).is_err(), "{input:?}");
        }
    }

    #[test]
    fn run_command_preserves_arguments_after_separator() {
        let command =
            CliCommand::parse(&args(&["run", "main.wi", "--", "a", "--release"])).unwrap();
        let CliCommand::Run(command) = command else {
            panic!("expected run command");
        };
        assert_eq!(command.source, "main.wi");
        assert_eq!(command.program_args, ["a", "--release"]);
    }

    #[test]
    fn build_command_materializes_release_options() {
        let command = CliCommand::parse(&args(&[
            "build",
            "main.wi",
            "--release",
            "--debug-info",
            "--runtime-lib=rt.a",
        ]))
        .unwrap();
        let CliCommand::Build(command) = command else {
            panic!("expected build command");
        };
        assert!(command.options.target.emit_debug_info);
        assert_eq!(
            command.options.target.runtime_lib,
            Some(PathBuf::from("rt.a"))
        );
    }
}
