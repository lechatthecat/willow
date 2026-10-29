use super::*;
use willow_compiler::{
    ai::edit::{ChangeFormat, Request, Workspace},
    diagnostics::DiagnosticEmitter,
};
#[derive(Debug)]
pub(super) struct EditCommand {
    operation: String,
    root: PathBuf,
    entry: Option<PathBuf>,
    request: Option<PathBuf>,
    transaction: Option<String>,
    project: bool,
    baseline: Option<PathBuf>,
    output: Option<PathBuf>,
    manifest: Option<PathBuf>,
    changes: ChangeFormat,
}
impl EditCommand {
    pub(super) fn parse(args: &[String]) -> Result<Self> {
        let operation = args.first().context(
            "edit requires prepare, preview, verify, baseline, validate, apply or recover",
        )?;
        anyhow::ensure!(
            matches!(
                operation.as_str(),
                "prepare" | "preview" | "validate" | "verify" | "baseline" | "apply" | "recover"
            ),
            "unknown edit operation"
        );
        let mut command = Self {
            operation: operation.clone(),
            root: PathBuf::from("."),
            entry: None,
            request: None,
            transaction: None,
            project: false,
            baseline: None,
            output: None,
            manifest: None,
            changes: ChangeFormat::Full,
        };
        let mut args = args[1..].iter();
        let mut seen = std::collections::HashSet::new();
        while let Some(key) = args.next() {
            anyhow::ensure!(seen.insert(key), "duplicate edit option");
            if key == "--project" {
                command.project = true;
                continue;
            }
            let value = args.next().context("missing edit option value")?;
            match key.as_str() {
                "--manifest" => command.manifest = Some(value.into()),
                "--baseline" => command.baseline = Some(value.into()),
                "--output" => command.output = Some(value.into()),
                "--root" => command.root = value.into(),
                "--entry" => command.entry = Some(value.into()),
                "--requests" => command.request = Some(value.into()),
                "--transaction" => command.transaction = Some(value.clone()),
                "--changes" => {
                    command.changes = match value.as_str() {
                        "full" => ChangeFormat::Full,
                        "diff" => ChangeFormat::Diff,
                        _ => anyhow::bail!("--changes must be full or diff"),
                    }
                }
                _ => anyhow::bail!("unknown edit option {key}"),
            }
        }
        if command.operation == "baseline"
            || (command.operation == "verify" && command.baseline.is_some())
        {
            anyhow::ensure!(
                command.entry.is_some()
                    && command.request.is_none()
                    && command.transaction.is_none()
                    && command.changes == ChangeFormat::Full,
                "baseline/tree verification requires --entry and no transaction or requests"
            );
            anyhow::ensure!(
                if command.operation == "baseline" {
                    command.output.is_some()
                        && command.baseline.is_none()
                        && command.manifest.is_none()
                } else {
                    command.output.is_none() && command.manifest.is_some()
                },
                "baseline requires --output; tree verification requires --baseline and --manifest"
            );
            return Ok(command);
        }
        anyhow::ensure!(
            command.baseline.is_none() && command.output.is_none() && command.manifest.is_none(),
            "baseline/output/manifest option not valid for operation"
        );
        if command.operation == "prepare" {
            anyhow::ensure!(
                command.entry.is_some()
                    && command.request.is_some()
                    && command.transaction.is_none(),
                "prepare requires --entry and --requests"
            );
        } else {
            anyhow::ensure!(
                command.operation == "preview" || !seen.contains(&"--changes".to_string()),
                "--changes applies only to prepare and preview"
            );
            anyhow::ensure!(
                command.transaction.is_some()
                    && command.entry.is_none()
                    && command.request.is_none()
                    && !command.project,
                "operation requires --transaction only"
            );
        }
        Ok(command)
    }
    pub(super) fn execute(self, emitter: &mut dyn DiagnosticEmitter) -> Result<serde_json::Value> {
        if self.operation == "baseline" || self.baseline.is_some() {
            return if let Some(path) = &self.baseline {
                let baseline = willow_compiler::ai::verify::Baseline::load(path)?;
                let manifest = willow_compiler::ai::verify::EditManifest::load(
                    self.manifest.as_ref().unwrap(),
                )?;
                serde_json::to_value(baseline.verify_manifest(
                    &self.root.join(self.entry.as_ref().unwrap()),
                    self.project.then_some(self.root.as_path()),
                    &manifest,
                )?)
                .map_err(Into::into)
            } else {
                let current = willow_compiler::ai::verify::Baseline::capture(
                    &self.root.join(self.entry.as_ref().unwrap()),
                    self.project.then_some(self.root.as_path()),
                )?;
                current.save(self.output.as_ref().unwrap())?;
                Ok(
                    serde_json::json!({"kind":"semantic-baseline","schema_version":1,"revision":current.revision(),"complete":current.is_complete()}),
                )
            };
        }
        let workspace = Workspace::open(&self.root)?;
        let id = self.transaction.as_deref().unwrap_or("");
        match self.operation.as_str() {
            "prepare" => {
                let request: Request =
                    serde_json::from_slice(&std::fs::read(self.request.unwrap())?)?;
                workspace.prepare(
                    &self.root.join(self.entry.unwrap()),
                    self.project,
                    request,
                    self.changes,
                    emitter,
                )
            }
            "preview" => workspace.preview(id, self.changes),
            "validate" => workspace.validate(id, emitter),
            "verify" => Ok(serde_json::to_value(workspace.verify(id)?)?),
            "apply" => workspace.apply(id, emitter),
            "recover" => workspace.recover(id),
            _ => unreachable!(),
        }
    }
}

pub(super) const PREPARE_COMMAND: &str = "willow edit prepare --root . --entry src/main.wi --project --requests edits.json --changes diff --format ndjson --protocol-version 1";

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &str) -> Result<EditCommand> {
        let args: Vec<String> = args.split_whitespace().map(String::from).collect();
        EditCommand::parse(&args)
    }
    #[test]
    fn changes_option_selects_preview_format() {
        let prepare = "prepare --entry main.wi --requests edits.json";
        assert_eq!(parse(prepare).unwrap().changes, ChangeFormat::Full);
        let diff = parse(&format!("{prepare} --changes diff")).unwrap();
        assert_eq!(diff.changes, ChangeFormat::Diff);
        let full = parse(&format!("{prepare} --changes full")).unwrap();
        assert_eq!(full.changes, ChangeFormat::Full);
        let preview = parse("preview --transaction t --changes diff").unwrap();
        assert_eq!(preview.changes, ChangeFormat::Diff);
        let error = |args: &str| parse(args).unwrap_err().to_string();
        assert_eq!(
            error(&format!("{prepare} --changes patch")),
            "--changes must be full or diff"
        );
        assert_eq!(
            error(&format!("{prepare} --changes")),
            "missing edit option value"
        );
        assert_eq!(
            error(&format!("{prepare} --changes diff --changes full")),
            "duplicate edit option"
        );
        for operation in ["validate", "apply", "recover"] {
            assert_eq!(
                error(&format!("{operation} --transaction t --changes diff")),
                "--changes applies only to prepare and preview"
            );
            assert!(parse(&format!("{operation} --transaction t")).is_ok());
        }
    }
}

#[cfg(test)]
mod semantic_verify_tests {
    use super::*;
    fn parse(text: &str) -> Result<EditCommand> {
        EditCommand::parse(
            &text
                .split_whitespace()
                .map(String::from)
                .collect::<Vec<_>>(),
        )
    }
    #[test]
    fn semantic_verify_cli_contract() {
        for command in [
            "baseline --entry main.wi --output baseline.json",
            "verify --entry main.wi --baseline baseline.json --manifest edits.json",
            "verify --transaction abc",
        ] {
            assert!(parse(command).is_ok(), "{command}");
        }
        for command in [
            "verify --entry main.wi --baseline b",
            "baseline --entry main.wi --output b --manifest m",
            "verify --transaction t --manifest m",
            "baseline --entry main.wi",
            "baseline --output b",
            "verify --entry main.wi",
            "verify --baseline b",
            "verify --transaction t --baseline b",
            "verify --transaction t --output b",
            "baseline --entry main.wi --output b --requests r",
            "verify --entry main.wi --baseline b --changes diff",
        ] {
            assert!(parse(command).is_err(), "{command}");
        }
    }
}
