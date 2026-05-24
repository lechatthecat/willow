/// Holds the source text for a single file, enabling line/column lookups.
pub struct SourceMap {
    pub path: String,
    pub source: String,
    line_offsets: Vec<usize>,
}

impl SourceMap {
    pub fn new(path: impl Into<String>, source: impl Into<String>) -> Self {
        let source = source.into();
        let mut offsets = vec![0usize];
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                offsets.push(i + 1);
            }
        }
        Self {
            path: path.into(),
            source,
            line_offsets: offsets,
        }
    }

    /// Returns the text of line `line` (1-indexed). Empty string if out of range.
    pub fn line_text(&self, line: usize) -> &str {
        if line == 0 {
            return "";
        }
        let idx = line - 1;
        let start = match self.line_offsets.get(idx) {
            Some(&s) => s,
            None => return "",
        };
        let end = self
            .line_offsets
            .get(idx + 1)
            .map(|&e| e.saturating_sub(1))
            .unwrap_or(self.source.len());
        self.source
            .get(start..end)
            .unwrap_or("")
            .trim_end_matches('\r')
    }

    /// Returns the byte offset of the start of line `line` (1-indexed).
    pub fn line_start(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        self.line_offsets
            .get(line - 1)
            .copied()
            .unwrap_or(self.source.len())
    }

    pub fn total_lines(&self) -> usize {
        self.line_offsets.len()
    }
}
