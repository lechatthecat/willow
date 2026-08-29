/// Stable source-file identity within one compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct FileId(pub u32);

impl FileId {
    pub const ENTRY: Self = Self(0);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub file_id: FileId,
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub col: usize,
}

impl Span {
    pub fn new(start: usize, end: usize, line: usize, col: usize) -> Self {
        Self::in_file(FileId::ENTRY, start, end, line, col)
    }

    pub fn in_file(file_id: FileId, start: usize, end: usize, line: usize, col: usize) -> Self {
        Self {
            file_id,
            start,
            end,
            line,
            col,
        }
    }

    /// Extend this span so it ends where `end` ends, keeping THIS span's file.
    ///
    /// Merging two token spans with [`Span::new`] would stamp the merged span
    /// with [`FileId::ENTRY`], so every construct whose span spans more than one
    /// token — a class, a method, a match arm — would report against the entry
    /// file even when it was parsed from an imported module (willow-3eo1).
    pub fn to(self, end: Span) -> Span {
        Self {
            file_id: self.file_id,
            start: self.start,
            end: end.end,
            line: self.line,
            col: self.col,
        }
    }

    /// Whether `other` lies within this span. Spans from different files never
    /// contain one another, however their byte offsets compare.
    pub fn contains(&self, other: Span) -> bool {
        self.file_id == other.file_id && self.start <= other.start && other.end <= self.end
    }

    pub fn dummy() -> Self {
        Self {
            file_id: FileId::ENTRY,
            start: 0,
            end: 0,
            line: 0,
            col: 0,
        }
    }
}
