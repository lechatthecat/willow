use super::*;
use willow_compiler::ai::verify::semantic_diff::{Options, SemanticDiff, resolve_base, verify};
fn parse(args: &[String]) -> Result<(Options, bool, bool)> {
    let mut options = Options {
        root: PathBuf::from("."),
        entry: None,
        project: false,
        base: String::new(),
        fail_on_growth: false,
    };
    let mut json = false;
    let mut explain = false;
    let mut seen = std::collections::BTreeSet::new();
    let mut args = args.iter();
    while let Some(key) = args.next() {
        anyhow::ensure!(seen.insert(key), "duplicate option: {key}");
        if key == "--explain" {
            explain = true;
            continue;
        }
        if key == "--project" {
            options.project = true;
            continue;
        }
        let value = args.next().context("missing option value")?;
        match key.as_str() {
            "--base" => options.base.clone_from(value),
            "--root" => options.root = value.into(),
            "--entry" => {
                options.entry = Some(value.into());
            }
            "--format" => {
                anyhow::ensure!(
                    matches!(value.as_str(), "json" | "text"),
                    "format must be json or text"
                );
                json = value == "json";
            }
            "--fail-on" => {
                anyhow::ensure!(value == "effect-growth", "fail-on must be effect-growth");
                options.fail_on_growth = true;
            }
            _ => anyhow::bail!("unknown option: {key}"),
        }
    }
    anyhow::ensure!(!options.base.is_empty(), "--base is required");
    if let Some(entry) = &options.entry {
        anyhow::ensure!(
            !entry.as_os_str().is_empty()
                && entry
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_))),
            "entry must be workspace-relative"
        );
    }
    options.base = resolve_base(&options.root, &options.base).context("invalid base revision")?;
    Ok((options, json, explain))
}
pub(super) fn run(args: &[String]) -> Result<i32> {
    if args == ["--help"] || args == ["-h"] {
        println!(
            "Usage: willow verify-edit --base REV [--root DIR] [--entry FILE] [--project] [--format json|text] [--explain] [--fail-on effect-growth]"
        );
        return Ok(0);
    }
    let (options, json, explain) = match parse(args) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("verify-edit: {error:#}");
            return Ok(2);
        }
    };
    let report =
        verify(&options).unwrap_or_else(|error| SemanticDiff::incomplete(format!("{error:#}")));
    use std::io::Write;
    let output = if json {
        format!("{}\n", serde_json::to_string(&report)?)
    } else {
        report.human(explain)
    };
    if std::io::stdout()
        .lock()
        .write_all(output.as_bytes())
        .is_err()
    {
        return Ok(3);
    }
    Ok(report.exit_code())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn semantic_diff_invalid_cli_arguments() {
        for input in [
            "",
            "--base",
            "--base HEAD --base HEAD",
            "--base HEAD --format xml",
            "--base HEAD --fail-on anything",
            "--base HEAD --entry ../escape.wi",
            "--base HEAD --entry /absolute.wi",
            "--base HEAD --max-output 1",
            "--base HEAD --unknown value",
        ] {
            let args: Vec<_> = input.split_whitespace().map(String::from).collect();
            assert!(parse(&args).is_err(), "{input}");
        }
    }
}
