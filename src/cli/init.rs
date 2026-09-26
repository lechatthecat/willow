use anyhow::{Context, Result};
use std::path::PathBuf;
use willow_compiler::project::init::{Scaffold, resolve_root};

#[derive(Debug)]
pub(super) struct InitCommand {
    root: PathBuf,
    name: Option<String>,
}
impl InitCommand {
    pub(super) fn parse(args: &[String]) -> Result<Self> {
        let mut root = None;
        let mut name = None;
        let mut i = 0;
        while i < args.len() {
            let (key, inline) = args[i]
                .split_once('=')
                .map_or((args[i].as_str(), None), |(k, v)| (k, Some(v)));
            match key {
                "--name" => {
                    let value = if let Some(value) = inline {
                        value.to_owned()
                    } else {
                        i += 1;
                        args.get(i).context("missing init option value")?.clone()
                    };
                    anyhow::ensure!(name.is_none(), "duplicate {key}");
                    name = Some(value);
                }
                _ if !args[i].starts_with('-') && root.is_none() => {
                    root = Some(PathBuf::from(&args[i]))
                }
                _ => anyhow::bail!("unknown init argument `{}`", args[i]),
            }
            i += 1;
        }
        Ok(Self {
            root: root.unwrap_or(std::env::current_dir()?),
            name,
        })
    }
    pub(super) fn execute(self) -> Result<()> {
        let root = resolve_root(&self.root)?;
        Scaffold::create(&root, self.name.as_deref())?.commit();
        println!("Created Willow project");
        Ok(())
    }
}
