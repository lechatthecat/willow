use std::path::PathBuf;

// Scaffolding for module-graph-centered resolution (willow-pz6q.6); not yet wired.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub path: PathBuf,
    pub source: String,
    pub module_name: String,
}

#[allow(dead_code)]
impl SourceFile {
    pub fn new(path: PathBuf, source: String) -> Self {
        let module_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        Self {
            path,
            source,
            module_name,
        }
    }
}
