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
    changes: ChangeFormat,
}
impl EditCommand {
    pub(super) fn parse(args: &[String]) -> Result<Self> {
        let operation = args
            .first()
            .context("edit requires prepare, preview, validate, apply or recover")?;
        anyhow::ensure!(
            matches!(
                operation.as_str(),
                "prepare" | "preview" | "validate" | "apply" | "recover"
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
