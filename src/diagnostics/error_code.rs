#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    // Generic
    E0001,
    // Lexer E005x
    E0050,
    E0051,
    // Parser E010x
    E0101,
    E0102,
    E0103,
    E0104,
    E0105,
    E0106,
    E0107,
    E0108,
    // Type checker E02xx
    E0201,
    E0202,
    E0203,
    // Mutability E030x
    E0301,
    E0302,
    // Name resolution E035x
    E0350,
    E0351,
    // Import E040x
    E0401,
    E0402,
    E0403,
    // Class/visibility E050x
    E0501,
    E0502,
    // Linker E07xx
    E0700,
    // Internal compiler error E08xx
    E0800,
    // Ternary E09xx
    E0901,
    E0902,
    E0903,
    // Lambda E10xx
    E1001,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::E0001 => "E0001",
            ErrorCode::E0050 => "E0050",
            ErrorCode::E0051 => "E0051",
            ErrorCode::E0101 => "E0101",
            ErrorCode::E0102 => "E0102",
            ErrorCode::E0103 => "E0103",
            ErrorCode::E0104 => "E0104",
            ErrorCode::E0105 => "E0105",
            ErrorCode::E0106 => "E0106",
            ErrorCode::E0107 => "E0107",
            ErrorCode::E0108 => "E0108",
            ErrorCode::E0201 => "E0201",
            ErrorCode::E0202 => "E0202",
            ErrorCode::E0203 => "E0203",
            ErrorCode::E0301 => "E0301",
            ErrorCode::E0302 => "E0302",
            ErrorCode::E0350 => "E0350",
            ErrorCode::E0351 => "E0351",
            ErrorCode::E0401 => "E0401",
            ErrorCode::E0402 => "E0402",
            ErrorCode::E0403 => "E0403",
            ErrorCode::E0501 => "E0501",
            ErrorCode::E0502 => "E0502",
            ErrorCode::E0700 => "E0700",
            ErrorCode::E0800 => "E0800",
            ErrorCode::E0901 => "E0901",
            ErrorCode::E0902 => "E0902",
            ErrorCode::E0903 => "E0903",
            ErrorCode::E1001 => "E1001",
        }
    }
}
