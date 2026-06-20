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
