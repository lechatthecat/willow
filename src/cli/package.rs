use anyhow::{Context, Result, ensure};
use std::path::PathBuf;
use willow_compiler::{
    package::{PackageMutation, display_dependencies, inspect_packages, mutate_packages},
    project,
};

#[derive(Debug)]
pub(super) struct PackageCommand {
    directory: PathBuf,
    dry_run: bool,
    operation: Operation,
}
#[derive(Debug)]
enum Operation {
    Mutate(PackageMutation),
    Tree,
    Why(String),
}

impl PackageCommand {
    pub(super) fn parse(command: &str, args: &[String]) -> Result<Self> {
        let mut positional = Vec::new();
        let mut git = None;
        let mut path = None;
        let mut version = None;
        let mut directory = None;
        let mut dry_run = false;
        let mut breaking = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let (option, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(a, b)| (a, Some(b)));
            match option {
                "--dry-run" if command != "deps" && inline.is_none() => dry_run = true,
                "--breaking" if command == "update" && inline.is_none() => breaking = true,
                "--git" | "--path" | "--version" if command == "add" => {
                    let value = inline
                        .or_else(|| args.next().map(String::as_str))
                        .with_context(|| format!("missing {option} value"))?;
                    ensure!(
                        !value.is_empty() && !value.starts_with("--"),
                        "missing {option} value"
                    );
                    let slot = match option {
                        "--git" => &mut git,
                        "--path" => &mut path,
                        _ => &mut version,
                    };
                    ensure!(
                        slot.replace(value.to_owned()).is_none(),
                        "duplicate {option}"
                    );
                }
                "--project-dir" => {
                    let value = inline
                        .or_else(|| args.next().map(String::as_str))
                        .context("missing --project-dir value")?;
                    ensure!(
                        !value.is_empty() && !value.starts_with("--"),
                        "missing --project-dir value"
                    );
                    ensure!(
                        directory.replace(PathBuf::from(value)).is_none(),
                        "duplicate --project-dir"
                    );
                }
                _ if arg.starts_with('-') => anyhow::bail!("unknown {command} option `{arg}`"),
                _ => positional.push(arg.clone()),
            }
        }
        let operation = match command {
            "add" => {
                ensure!(positional.len() <= 1, "add accepts one alias or URL");
                let mut alias = positional.pop();
                if git.is_none() && path.is_none() {
                    let url = alias
                        .take()
                        .context("add requires a URL, --git URL, or --path DIR")?;
                    ensure!(
                        url.contains("://") || url.starts_with("git@"),
                        "expected URL shorthand; use --git or --path with an alias"
                    );
                    git = Some(url);
                }
                ensure!(
                    git.is_some() ^ path.is_some(),
                    "add requires exactly one --git or --path source"
                );
                ensure!(
                    path.is_none() || version.is_none(),
                    "--version requires --git"
                );
                if let Some(version) = &version {
                    semver::VersionReq::parse(version)?;
                }
                Operation::Mutate(PackageMutation::Add {
                    alias,
                    git,
                    path,
                    version,
                })
            }
            "remove" => {
                ensure!(
                    positional.len() == 1,
                    "remove requires one dependency alias"
                );
                Operation::Mutate(PackageMutation::Remove {
                    alias: positional.pop().unwrap(),
                })
            }
            "update" => {
                ensure!(
                    positional.len() <= 1,
                    "update accepts at most one dependency alias"
                );
                Operation::Mutate(PackageMutation::Update {
                    alias: positional.pop(),
                    breaking,
                })
            }
            "deps" => match positional.as_slice() {
                [tree] if tree == "tree" => Operation::Tree,
                [why, target] if why == "why" => Operation::Why(target.clone()),
                _ => anyhow::bail!("expected deps tree or deps why <alias|package-name>"),
            },
            _ => unreachable!(),
        };
        Ok(Self {
            directory: directory.unwrap_or_else(|| PathBuf::from(".")),
            dry_run,
            operation,
        })
    }

    pub(super) fn execute(self) -> Result<()> {
        // Canonicalizing first makes upward discovery work from relative paths
        // and from a child of the project, including the default current dir.
        let directory = std::fs::canonicalize(self.directory)?;
        let (_, root) =
            project::find_project_manifest(&directory).context("no project.toml found")?;
        let mut out = std::io::stdout().lock();
        match self.operation {
            Operation::Mutate(mutation) => mutate_packages(&root, mutation, self.dry_run, &mut out),
            Operation::Tree => display_dependencies(&inspect_packages(&root)?, None, &mut out),
            Operation::Why(target) => {
                display_dependencies(&inspect_packages(&root)?, Some(&target), &mut out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_incomplete_ambiguous_and_misplaced_options() {
        for (cmd, args) in [
            ("add", vec![]),
            ("add", vec!["alias"]),
            ("add", vec!["--git"]),
            ("add", vec!["--git", "--dry-run"]),
            ("add", vec!["--git=x", "--path=y"]),
            ("add", vec!["--path=x", "--version=1"]),
            ("add", vec!["--git=x", "--version=invalid"]),
            ("add", vec!["--path=x", "--path=y"]),
            ("remove", vec![]),
            ("remove", vec!["a", "b"]),
            ("remove", vec!["a", "--breaking"]),
            ("update", vec!["a", "b"]),
            ("update", vec!["--git=x"]),
            ("deps", vec!["why"]),
            ("deps", vec!["tree", "x"]),
            ("deps", vec!["tree", "--dry-run"]),
        ] {
            let args: Vec<_> = args.into_iter().map(str::to_owned).collect();
            assert!(PackageCommand::parse(cmd, &args).is_err(), "{cmd} {args:?}");
        }
    }
}
