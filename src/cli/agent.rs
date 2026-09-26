use anyhow::{Context, Result};
use std::{
    io::{BufRead, IsTerminal, Write},
    path::Path,
};
use willow_compiler::project::{
    agent::{
        Agent, AgentCapabilities, managed_range, managed_version, render_agent_instructions,
        replace_managed,
    },
    init::atomic_replace,
};

pub(super) fn capabilities() -> AgentCapabilities {
    AgentCapabilities {
        machine_check: Some(super::protocol::CHECK_COMMAND),
        machine_build: Some(super::protocol::BUILD_COMMAND),
        snapshots: Some(super::analysis::SNAPSHOT_COMMAND),
        symbol_query: Some(super::analysis::QUERY_COMMAND),
        references: Some(super::analysis::QUERY_COMMAND),
        callers: Some(super::analysis::IMPACT_COMMAND),
        impact: Some(super::analysis::IMPACT_COMMAND),
        structured_edits: Some(super::edit::PREPARE_COMMAND),
    }
}

pub(super) fn terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}
pub(super) fn ask<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    prompt: &str,
    default: bool,
    tty: bool,
) -> Result<bool> {
    if !tty {
        return Ok(false);
    }
    loop {
        write!(
            output,
            "{prompt} {} ",
            if default { "[Y/n]" } else { "[y/N]" }
        )?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(false);
        }
        match line.trim() {
            "" => return Ok(default),
            "y" | "Y" | "yes" | "YES" => return Ok(true),
            "n" | "N" | "no" | "NO" => return Ok(false),
            _ => writeln!(output, "Please enter yes or no.")?,
        }
    }
}

#[derive(Debug)]
pub(super) enum AgentCommand {
    Instructions { agent: Agent, json: bool },
    Sync { yes: bool },
}
impl AgentCommand {
    pub(super) fn parse(args: &[String]) -> Result<Self> {
        match args.first().map(String::as_str) {
            Some("instructions") => {
                let agent = Agent::parse(args.get(1).context("missing agent")?)?;
                let format = match &args[2..] {
                    [] => "human",
                    [flag, value] if flag == "--format" => value,
                    [flag] if flag.starts_with("--format=") => &flag[9..],
                    _ => anyhow::bail!("expected --format human|json"),
                };
                anyhow::ensure!(
                    matches!(format, "human" | "json"),
                    "expected --format human|json"
                );
                Ok(Self::Instructions {
                    agent,
                    json: format == "json",
                })
            }
            Some("sync") => {
                anyhow::ensure!(
                    args.len() == 1 || (args.len() == 2 && args[1] == "--yes"),
                    "expected agent sync [--yes]"
                );
                Ok(Self::Sync {
                    yes: args.len() == 2,
                })
            }
            _ => anyhow::bail!("expected agent instructions or sync"),
        }
    }
    pub(super) fn execute(self) -> Result<()> {
        let caps = capabilities();
        match self {
            Self::Instructions { agent, json } => {
                let markdown = render_agent_instructions(agent, caps);
                if json {
                    println!(
                        "{}",
                        serde_json::json!({"schema_version":1,"agent":agent,"instruction_schema":caps.instruction_schema(),"capabilities":caps,"markdown":markdown})
                    );
                } else {
                    print!("{markdown}");
                }
            }
            Self::Sync { yes } => {
                // Validate every candidate before changing any file.
                let mut updates = Vec::new();
                for agent in [Agent::Claude, Agent::Codex] {
                    let path = Path::new(agent.file());
                    let Some(text) = read_instructions(path)? else {
                        continue;
                    };
                    let rendered = render_agent_instructions(agent, caps);
                    if let Some(updated) = replace_managed(&text, &rendered)? {
                        if updated == text {
                            println!("{} already current", agent.file());
                            continue;
                        }
                        let version = managed_version(&text)?;
                        println!(
                            "{} instruction schema {} -> {}",
                            agent.file(),
                            version.unwrap_or(0),
                            caps.instruction_schema()
                        );
                        if yes
                            || ask(
                                &mut std::io::stdin().lock(),
                                &mut std::io::stdout().lock(),
                                &format!("Update {}?", agent.file()),
                                true,
                                terminal(),
                            )?
                        {
                            updates.push((path.to_path_buf(), text, updated));
                        }
                    }
                }
                for (path, original, updated) in updates {
                    anyhow::ensure!(
                        read_instructions(&path)?.as_deref() == Some(&original),
                        "instructions changed during confirmation"
                    );
                    atomic_replace(&path, updated.as_bytes())?;
                }
            }
        }
        Ok(())
    }
}

pub(super) fn read_instructions(path: &Path) -> Result<Option<String>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            anyhow::ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "{} is not a regular instruction file",
                path.display()
            );
            let text = std::fs::read_to_string(path)?;
            managed_range(&text)?;
            Ok(Some(text))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prompt_defaults_grammar_retry_eof_and_non_tty() {
        for (input, default, expected) in [
            ("\n", true, true),
            ("\n", false, false),
            ("y\n", false, true),
            ("Y\n", false, true),
            ("yes\n", false, true),
            ("YES\n", false, true),
            ("n\n", true, false),
            ("N\n", true, false),
            ("no\n", true, false),
            ("NO\n", true, false),
            ("maybe\nyes\n", false, true),
            ("", true, false),
        ] {
            let mut output = Vec::new();
            assert_eq!(
                ask(&mut input.as_bytes(), &mut output, "Agent?", default, true).unwrap(),
                expected
            );
            assert!(String::from_utf8(output).unwrap().contains(if default {
                "[Y/n]"
            } else {
                "[y/N]"
            }));
        }
        let mut output = Vec::new();
        let mut input = "yes\n".as_bytes();
        assert!(!ask(&mut input, &mut output, "Agent?", true, false).unwrap());
        assert!(output.is_empty());
        assert_eq!(input, b"yes\n");
    }
    #[test]
    fn current_commands_are_from_protocol_and_parse() {
        let caps = capabilities();
        assert_eq!(
            caps.snapshots,
            Some(super::super::analysis::SNAPSHOT_COMMAND)
        );
        assert_eq!(caps.impact, Some(super::super::analysis::IMPACT_COMMAND));
        for command in [
            caps.machine_check,
            caps.machine_build,
            caps.snapshots,
            caps.symbol_query,
            caps.references,
            caps.callers,
            caps.impact,
            caps.structured_edits,
        ]
        .into_iter()
        .flatten()
        {
            let args: Vec<String> = command
                .split_whitespace()
                .skip(1)
                .map(str::to_owned)
                .collect();
            let (args, _, _) = super::super::protocol::options(args).unwrap();
            super::super::CliCommand::parse(&args).unwrap();
        }
    }
    #[test]
    fn golden_instructions_and_capability_omission() {
        let current = capabilities();
        let v0 = AgentCapabilities {
            machine_check: current.machine_check,
            machine_build: current.machine_build,
            ..Default::default()
        };
        for (name, caps) in [("v0", v0), ("snapshot", current)] {
            let codex = render_agent_instructions(Agent::Codex, caps);
            let claude = render_agent_instructions(Agent::Claude, caps);
            assert_eq!(
                codex.split_once("## Willow core").unwrap().1,
                claude.split_once("## Willow core").unwrap().1
            );
            for (agent, text) in [(Agent::Codex, codex), (Agent::Claude, claude)] {
                let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/agent")
                    .join(name)
                    .join(agent.file());
                if std::env::var_os("UPDATE_AGENT_GOLDENS").is_some() {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, &text).unwrap();
                }
                assert_eq!(text, std::fs::read_to_string(path).unwrap());
                if name == "v0" {
                    assert!(!text.contains("willow snapshot"));
                    assert!(!text.contains("willow impact"));
                } else {
                    for phrase in [
                        "Before semantic",
                        "old snapshot stale",
                        "new snapshot",
                        "willow impact",
                    ] {
                        assert!(text.contains(phrase));
                    }
                }
            }
        }
        let disabled = render_agent_instructions(Agent::Codex, AgentCapabilities::default());
        for command in [
            "willow check",
            "willow build",
            "willow snapshot",
            "willow query",
            "willow impact",
            "willow edit",
        ] {
            assert!(!disabled.contains(command));
        }
    }
}
