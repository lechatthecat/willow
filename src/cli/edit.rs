use super::*;
use willow_compiler::{
    ai::edit::{Request, Workspace},
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
                    emitter,
                )
            }
            "preview" => workspace.preview(id),
            "validate" => workspace.validate(id, emitter),
            "apply" => workspace.apply(id, emitter),
            "recover" => workspace.recover(id),
            _ => unreachable!(),
        }
    }
}

pub(super) const PREPARE_COMMAND: &str = "willow edit prepare --root . --entry src/main.wi --project --requests edits.json --format ndjson --protocol-version 1";
