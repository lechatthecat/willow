use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;
use willow_compiler::{CodegenOptions, compile, project};

#[derive(Debug)]
enum CliCommand {
    Build(BuildCommand),
    Run(RunCommand),
    Debug(DebugCommand),
}

#[derive(Debug)]
struct BuildCommand {
    source: Option<String>,
    project_dir: Option<PathBuf>,
    output: Option<String>,
    options: CodegenOptions,
}

#[derive(Debug)]
struct RunCommand {
    source: String,
    program_args: Vec<String>,
    options: CodegenOptions,
}

#[derive(Debug)]
struct DebugCommand {
    source: String,
    options: CodegenOptions,
}

#[derive(Default)]
struct CompilerFlags {
    debug: bool,
    release: bool,
    debug_info: bool,
    runtime_lib: Option<PathBuf>,
}

impl CompilerFlags {
    fn parse(&mut self, args: &[String], index: &mut usize) -> Result<bool> {
        let arg = &args[*index];
        match arg.as_str() {
            "--debug" => self.debug = true,
            "--release" => self.release = true,
            "--debug-info" => self.debug_info = true,
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

    fn finish(self) -> Result<CodegenOptions> {
        if self.debug && self.release {
            anyhow::bail!("`--debug` and `--release` cannot be used together");
        }
        if self.debug_info && !self.release {
            anyhow::bail!("`--debug-info` requires `--release`");
        }

        let mut options = if self.release {
            if self.debug_info {
                CodegenOptions::release_with_debug_info()
            } else {
                CodegenOptions::release()
            }
        } else {
            CodegenOptions::debug()
        };
        options.runtime_lib = self.runtime_lib;
        Ok(options)
    }
}

impl CliCommand {
    fn parse(args: &[String]) -> Result<Self> {
        let Some(command) = args.first() else {
            anyhow::bail!("missing command or source file\n\n{}", usage());
        };

        match command.as_str() {
            "build" => Ok(Self::Build(BuildCommand::parse(&args[1..])?)),
            "run" => Ok(Self::Run(RunCommand::parse(&args[1..])?)),
            "debug" => Ok(Self::Debug(DebugCommand::parse(&args[1..])?)),
            source if source.ends_with(".wi") => Ok(Self::Build(BuildCommand::parse(args)?)),
            unknown => anyhow::bail!("unknown command `{unknown}`\n\n{}", usage()),
        }
    }

    fn execute(self) -> Result<()> {
        match self {
            Self::Build(command) => command.execute(),
            Self::Run(command) => command.execute(),
            Self::Debug(command) => command.execute(),
        }
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
        Ok(Self {
            source,
            project_dir,
            output,
            options: flags.finish()?,
        })
    }

    fn execute(self) -> Result<()> {
        if let Some(source) = self.source {
            let output = self.output.unwrap_or_else(|| stem(&source));
            return compile(&source, &output, &self.options, None);
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
            "building project '{}' v{}",
            manifest.project.name, manifest.project.version
        );
        compile(
            entry.to_str().unwrap(),
            &output,
            &self.options,
            Some(project_root),
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
            if !arg.ends_with(".wi") {
                anyhow::bail!("unexpected run argument `{arg}`; expected a `.wi` source file");
            }
            if source.replace(arg.clone()).is_some() {
                anyhow::bail!("run accepts exactly one source file");
            }
            index += 1;
        }
        Ok(Self {
            source: source.ok_or_else(|| anyhow::anyhow!("no source file specified"))?,
            program_args,
            options: flags.finish()?,
        })
    }

    fn execute(self) -> Result<()> {
        let output = temp_path(format!("willow_run_{}", stem(&self.source)));
        compile(&self.source, &output, &self.options, None)?;
        let status = Command::new(&output)
            .args(&self.program_args)
            .status()
            .with_context(|| format!("failed to run {output}"))?;
        std::process::exit(status.code().unwrap_or(0));
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
        std::process::exit(status.code().unwrap_or(0));
    }
}

pub(super) fn run(args: Vec<String>) -> Result<()> {
    CliCommand::parse(&args)?.execute()
}

fn usage() -> &'static str {
    "Usage:\n  willowc build <source.wi|project-dir> [-o <output>] [--debug|--release] [--debug-info] [--runtime-lib <path>]\n  willowc run <source.wi> [--debug|--release] [--debug-info] [--runtime-lib <path>] [-- <args>...]\n  willowc debug <source.wi> [--runtime-lib <path>]"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
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
            (&["run"], false),                          // 17 missing run source
            (&["run", "project"], false),               // 18 non-source run arg
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
        assert!(command.options.emit_debug_info);
        assert_eq!(command.options.runtime_lib, Some(PathBuf::from("rt.a")));
    }
}
