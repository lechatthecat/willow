use std::path::PathBuf;

use super::source_file::SourceFile;

/// Module graph built from the entry file and all reachable imports.
/// Populated by the import resolver (willow-m6a); for now it just holds the entry file.
#[derive(Debug, Default)]
pub struct ModuleGraph {
    pub root: PathBuf,
    pub files: Vec<SourceFile>,
}

impl ModuleGraph {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            files: Vec::new(),
        }
    }

    pub fn add_file(&mut self, file: SourceFile) {
        self.files.push(file);
    }
}
