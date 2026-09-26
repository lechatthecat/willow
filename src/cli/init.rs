use super::agent::{ask, capabilities, detected, read_instructions, terminal};
use anyhow::{Context, Result};
use std::path::PathBuf;
use willow_compiler::project::{
    agent::{Agent, managed_range, render_agent_instructions},
    init::{Scaffold, atomic_replace, resolve_root},
};

#[derive(Debug)]
pub(super) struct InitCommand {
    root: PathBuf,
    name: Option<String>,
    ai: String,
    yes: bool,
}
impl InitCommand {
    pub(super) fn parse(args: &[String]) -> Result<Self> {
        let mut root = None;
        let mut name = None;
        let mut ai = None;
        let mut yes = false;
        let mut i = 0;
        while i < args.len() {
            let (key, inline) = args[i]
                .split_once('=')
                .map_or((args[i].as_str(), None), |(k, v)| (k, Some(v)));
            match key {
                "--name" | "--ai" => {
                    let value = if let Some(value) = inline {
                        value.to_owned()
                    } else {
                        i += 1;
                        args.get(i).context("missing init option value")?.clone()
                    };
                    let slot = if key == "--name" { &mut name } else { &mut ai };
                    anyhow::ensure!(slot.is_none(), "duplicate {key}");
                    *slot = Some(value);
                }
                "--yes" if inline.is_none() && !yes => yes = true,
                _ if !args[i].starts_with('-') && root.is_none() => {
                    root = Some(PathBuf::from(&args[i]))
                }
                _ => anyhow::bail!("unknown init argument `{}`", args[i]),
            }
            i += 1;
        }
        let ai = ai.unwrap_or_else(|| "auto".into());
        anyhow::ensure!(
            matches!(
                ai.as_str(),
                "auto" | "none" | "codex" | "claude" | "codex,claude" | "all"
            ),
            "invalid --ai selection"
        );
        Ok(Self {
            root: root.unwrap_or(std::env::current_dir()?),
            name,
            ai,
            yes,
        })
    }
    pub(super) fn execute(self) -> Result<()> {
        let root = resolve_root(&self.root)?;
        let mut scaffold = Scaffold::create(&root, self.name.as_deref())?;
        let tty = terminal();
        let detected = if self.ai == "auto" && (tty || self.yes) {
            detected()
        } else {
            [false; 2]
        };
        let mut replacements = Vec::new();
        for (index, agent) in [Agent::Claude, Agent::Codex].into_iter().enumerate() {
            let selected = match self.ai.as_str() {
                "none" => false,
                "all" | "codex,claude" => true,
                "auto" if self.yes => detected[index],
                "auto" => ask(
                    &mut std::io::stdin().lock(),
                    &mut std::io::stdout().lock(),
                    &instruction_prompt(agent, detected[index]),
                    detected[index],
                    tty,
                )?,
                name => name == agent.name(),
            };
            if !selected {
                continue;
            }
            let path = root.join(agent.file());
            let rendered = render_agent_instructions(agent, capabilities());
            match read_instructions(&path)? {
                None => scaffold.write_new(&path, rendered.as_bytes())?,
                Some(original) => {
                    if managed_range(&original)?.is_some() {
                        println!("{} already managed; use willow agent sync", agent.file());
                        continue;
                    }
                    // --yes only accepts detected agents, never grants append permission.
                    if ask(
                        &mut std::io::stdin().lock(),
                        &mut std::io::stdout().lock(),
                        &format!("Append Willow instructions to {}?", agent.file()),
                        false,
                        tty,
                    )? {
                        let mut updated = original.clone();
                        if !updated.is_empty() && !updated.ends_with('\n') {
                            updated.push('\n');
                        }
                        updated.push_str(&rendered);
                        replacements.push((path, original, updated));
                    }
                }
            }
        }
        // All prompts and new-file writes succeeded before touching existing text.
        for (index, (path, original, updated)) in replacements.iter().enumerate() {
            let result = (|| -> Result<()> {
                anyhow::ensure!(
                    read_instructions(path)?.as_deref() == Some(original.as_str()),
                    "instructions changed during confirmation"
                );
                atomic_replace(path, updated.as_bytes())
            })();
            if let Err(error) = result {
                for (path, original, updated) in replacements[..index].iter().rev() {
                    if std::fs::read(path).ok().as_deref() == Some(updated.as_bytes()) {
                        atomic_replace(path, original.as_bytes())
                            .context("failed to restore instruction file")?;
                    }
                }
                return Err(error);
            }
        }
        scaffold.commit();
        println!("Created Willow project");
        Ok(())
    }
}

fn instruction_prompt(agent: Agent, detected: bool) -> String {
    let name = match agent {
        Agent::Claude => "Claude Code",
        Agent::Codex => "Codex",
    };
    let status = if detected { "detected" } else { "not detected" };
    format!("{name} {status}.\nCreate {}?", agent.file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_prompts_show_detection_file_and_default() {
        for (agent, name, file) in [
            (Agent::Claude, "Claude Code", "CLAUDE.md"),
            (Agent::Codex, "Codex", "AGENTS.md"),
        ] {
            for detected in [false, true] {
                let mut output = Vec::new();
                assert_eq!(
                    ask(
                        &mut "\n".as_bytes(),
                        &mut output,
                        &instruction_prompt(agent, detected),
                        detected,
                        true,
                    )
                    .unwrap(),
                    detected
                );
                let status = if detected { "detected" } else { "not detected" };
                let default = if detected { "[Y/n]" } else { "[y/N]" };
                assert_eq!(
                    String::from_utf8(output).unwrap(),
                    format!("{name} {status}.\nCreate {file}? {default} ")
                );
            }
        }
    }
}
