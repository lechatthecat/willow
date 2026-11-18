//! Every link and path reference in a published file (tracked, or untracked
//! but not ignored) must reach another published file.
//!
//! Checked forms:
//! - Markdown inline links `[text](target)` and reference definitions
//!   `[label]: target` outside fenced code blocks;
//! - backtick-quoted `.wi` paths in the example and regression catalogs'
//!   Markdown (relative to the document or to the repository root);
//! - `example/...` and `tests/regressions/...` path mentions in any tracked
//!   text file;
//! - any mention of the unpublished `docs/` tree in a tracked text file.
//!
//! The published set comes from `git ls-files` (tracked plus untracked,
//! not-ignored files), so a link into an ignored directory such as `docs/`,
//! which exists only in a local checkout, is reported too.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

struct Tracked {
    files: BTreeSet<String>,
    dirs: BTreeSet<String>,
}

impl Tracked {
    fn load(root: &Path) -> Self {
        let output = Command::new("git")
            // Untracked files that are not ignored are publishable changes.
            .args([
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ])
            .current_dir(root)
            .output()
            .expect("the link check needs `git` to list published files");
        assert!(output.status.success(), "git ls-files failed");
        let mut files = BTreeSet::new();
        let mut dirs = BTreeSet::new();
        for path in output.stdout.split(|byte| *byte == 0) {
            if path.is_empty() {
                continue;
            }
            let path = String::from_utf8(path.to_vec()).expect("tracked paths are UTF-8");
            // A tracked path deleted in the working tree is not published by
            // the change under test.
            if !root.join(&path).exists() {
                continue;
            }
            let mut end = path.len();
            while let Some(slash) = path[..end].rfind('/') {
                dirs.insert(path[..slash].to_string());
                end = slash;
            }
            files.insert(path);
        }
        Self { files, dirs }
    }

    fn contains(&self, path: &str) -> bool {
        let path = path.trim_end_matches('/');
        path.is_empty() || self.files.contains(path) || self.dirs.contains(path)
    }
}

/// Joins `target` onto directory `base` (both repository-relative) and
/// removes `.` and `..` segments. `None` when the path leaves the repository.
fn normalize(base: &str, target: &str) -> Option<String> {
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        base.split('/').filter(|part| !part.is_empty()).collect()
    };
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn parent(path: &str) -> &str {
    path.rfind('/').map_or("", |slash| &path[..slash])
}

fn is_external(target: &str) -> bool {
    target.is_empty()
        || target.starts_with('#')
        || target.contains("://")
        || target.starts_with("mailto:")
}

/// Markdown link targets outside fenced code blocks, with 1-based lines.
fn markdown_links(text: &str) -> Vec<(usize, String)> {
    let mut links = Vec::new();
    let mut fenced = false;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        // Reference definition: `[label]: target`.
        if let Some(rest) = trimmed.strip_prefix('[')
            && let Some(close) = rest.find("]:")
            && !rest[..close].contains('[')
        {
            if let Some(target) = rest[close + 2..].split_whitespace().next() {
                links.push((index + 1, target.to_string()));
            }
            continue;
        }
        let mut rest = line;
        while let Some(open) = rest.find("](") {
            let after = &rest[open + 2..];
            let Some(close) = after.find(')') else { break };
            let target = after[..close]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches(|c| c == '<' || c == '>');
            links.push((index + 1, target.to_string()));
            rest = &after[close..];
        }
    }
    links
}

/// Backtick-quoted `.wi` paths in a Markdown document, outside fences.
fn markdown_source_mentions(text: &str) -> Vec<(usize, String)> {
    let mut mentions = Vec::new();
    let mut fenced = false;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        for (position, span) in line.split('`').enumerate() {
            // Odd positions lie between a pair of backticks.
            if position % 2 == 1
                && span.ends_with(".wi")
                && span
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-./".contains(c))
            {
                mentions.push((index + 1, span.to_string()));
            }
        }
    }
    mentions
}

fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_-./".contains(c)
}

/// Path tokens in `text` that begin with `prefix` at a token boundary
/// (optionally after `../` segments), with 1-based lines.
fn path_mentions(text: &str, prefix: &str) -> Vec<(usize, String)> {
    let mut mentions = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let mut from = 0;
        while let Some(found) = line[from..].find(prefix) {
            let start = from + found;
            from = start + prefix.len();
            // Extend backwards over `../` segments; any other path character
            // means `prefix` is only the tail of a longer component.
            let mut begin = start;
            while line[..begin].ends_with("../") {
                begin -= 3;
            }
            if line[..begin].chars().next_back().is_some_and(is_path_char) {
                continue;
            }
            let end = line[start..]
                .find(|c: char| !is_path_char(c))
                .map_or(line.len(), |offset| start + offset);
            let next = line[end..].chars().next();
            // Templates and globs (`example/{name}.wi`, `example/*.wi`) are
            // not concrete references.
            if matches!(next, Some('{' | '*' | '<' | '$')) {
                continue;
            }
            let token = line[begin..end].trim_end_matches(['.', '-']);
            mentions.push((index + 1, token.to_string()));
        }
    }
    mentions
}

#[test]
fn published_files_link_only_to_published_files() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tracked = Tracked::load(root);
    let mut broken = Vec::new();
    let mut checked = 0usize;
    for file in &tracked.files {
        // This file's own scanner fixtures are not references.
        if file == "tests/public_links.rs" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(root.join(file)) else {
            continue; // binary content has no textual links
        };
        let base = parent(file);
        if file.ends_with(".md") {
            for (line, target) in markdown_links(&text) {
                if is_external(&target) {
                    continue;
                }
                let target = target.split(['#', '?']).next().unwrap_or("");
                checked += 1;
                match normalize(base, target) {
                    Some(path) if tracked.contains(&path) => {}
                    _ => broken.push(format!("{file}:{line}: link `{target}`")),
                }
            }
            let catalog = file.starts_with("example/") || file.starts_with("tests/regressions/");
            for (line, mention) in markdown_source_mentions(&text)
                .into_iter()
                .filter(|_| catalog)
            {
                checked += 1;
                let local = normalize(base, &mention);
                let rooted = normalize("", &mention);
                if ![local, rooted]
                    .iter()
                    .flatten()
                    .any(|path| tracked.contains(path))
                {
                    broken.push(format!("{file}:{line}: source `{mention}`"));
                }
            }
        }
        for prefix in ["example/", "tests/regressions/"] {
            for (line, mention) in path_mentions(&text, prefix) {
                checked += 1;
                let rooted = normalize("", &mention);
                let local = normalize(base, &mention);
                if mention.starts_with("../") {
                    if !local.is_some_and(|path| tracked.contains(&path)) {
                        broken.push(format!("{file}:{line}: path `{mention}`"));
                    }
                } else if ![rooted, local]
                    .iter()
                    .flatten()
                    .any(|path| tracked.contains(path))
                {
                    broken.push(format!("{file}:{line}: path `{mention}`"));
                }
            }
        }
        for (line, mention) in path_mentions(&text, "docs/") {
            // Naming the tree itself (`docs/`) is not a reference into it.
            if mention == "docs/" {
                continue;
            }
            checked += 1;
            let rooted = normalize(base, &mention).filter(|path| tracked.contains(path));
            if rooted.is_none() && !tracked.contains(&mention) {
                broken.push(format!("{file}:{line}: unpublished `{mention}`"));
            }
        }
    }
    assert!(
        checked > 100,
        "the link check found only {checked} references"
    );
    assert!(
        broken.is_empty(),
        "{} references point outside the published tree:\n{}",
        broken.len(),
        broken.join("\n")
    );
}

#[test]
fn link_scanner_recognizes_each_reference_form() {
    let markdown = "See [a](x/a.md#part) and [b](https://example.com).\n\
                    ```\n[c](skipped.md)\n```\n\
                    [ref]: ../up.md \"title\"\n\
                    Run `demo.wi` or `dir/main.wi`, not `fn main()`.\n";
    let links = markdown_links(markdown);
    assert_eq!(
        links,
        [
            (1, "x/a.md#part".to_string()),
            (1, "https://example.com".to_string()),
            (5, "../up.md".to_string()),
        ]
    );
    assert_eq!(
        markdown_source_mentions(markdown),
        [(6, "demo.wi".to_string()), (6, "dir/main.wi".to_string())]
    );
    let mentions = path_mentions(
        "x example/a.wi, ../example/b/ my_example/c.wi example/{name}.wi docs/x.md.",
        "example/",
    );
    assert_eq!(
        mentions,
        [
            (1, "example/a.wi".to_string()),
            (1, "../example/b/".to_string())
        ]
    );
    assert_eq!(
        path_mentions("see docs/x.md.", "docs/"),
        [(1, "docs/x.md".to_string())]
    );
    assert_eq!(normalize("a/b", "../c.md").as_deref(), Some("a/c.md"));
    assert_eq!(normalize("", "../c.md"), None);
    assert!(is_external("#anchor"));
}
