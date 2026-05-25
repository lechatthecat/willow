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
    E0701,
    E0702,
    E0703,
    E0704,
    // Internal compiler error E08xx
    E0800,
    // Concurrency / async E08xx
    E0801,
    E0802,
    E0803,
    E0804,
    E0805,
    E0806,
    E0807,
    // Ternary E09xx
    E0901,
    E0902,
    E0903,
    // Lambda E10xx
    E1001,
    // Command-line / entry point E13xx
    E1301,
    E1302,
    E1303,
    // Formatting E14xx
    E1401,
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
            ErrorCode::E0701 => "E0701",
            ErrorCode::E0702 => "E0702",
            ErrorCode::E0703 => "E0703",
            ErrorCode::E0704 => "E0704",
            ErrorCode::E0800 => "E0800",
            ErrorCode::E0801 => "E0801",
            ErrorCode::E0802 => "E0802",
            ErrorCode::E0803 => "E0803",
            ErrorCode::E0804 => "E0804",
            ErrorCode::E0805 => "E0805",
            ErrorCode::E0806 => "E0806",
            ErrorCode::E0807 => "E0807",
            ErrorCode::E0901 => "E0901",
            ErrorCode::E0902 => "E0902",
            ErrorCode::E0903 => "E0903",
            ErrorCode::E1001 => "E1001",
            ErrorCode::E1301 => "E1301",
            ErrorCode::E1302 => "E1302",
            ErrorCode::E1303 => "E1303",
            ErrorCode::E1401 => "E1401",
        }
    }
}
