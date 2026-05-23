use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub path: PathBuf,
    pub source: String,
    pub module_name: String,
}

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
