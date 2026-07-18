/// Stable diagnostic codes (`E####` errors, `W####` warnings).
///
/// SINGLE SOURCE OF TRUTH: the `error_codes!` invocation below lists every
/// code once and generates the `ErrorCode` enum, `as_str` (the variant name),
/// and `ErrorCode::ALL` (used by the exhaustive test).
macro_rules! error_codes {
    ($($variant:ident),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum ErrorCode {
            $($variant),*
        }

        impl ErrorCode {
            /// The textual code (e.g. `E0809`), identical to the variant name.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(ErrorCode::$variant => stringify!($variant)),*
                }
            }

            /// Every defined code, for exhaustive tests.
            #[cfg(test)]
            pub const ALL: &'static [ErrorCode] = &[$(ErrorCode::$variant),*];
        }
    };
}

error_codes! {
    // Generic
    E0001,
    // Lexer E005x
    E0050,
    E0051,
    E0052, // integer literal out of range for i64
    E0053, // unterminated block comment
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
    // Interface E041x (type checker: conformance + table)
    E0410, // unknown interface in `implements`
    E0411, // class implements a non-interface type
    E0412, // class extends an interface
    E0413, // cannot instantiate an interface
    E0414, // duplicate implemented interface
    E0415, // class does not implement a required interface method
    E0416, // interface method parameter mismatch
    E0417, // interface method return type mismatch
    E0418, // unknown method on an interface-typed value
    E0419, // private module type accessed from another module
    // Interface E042x (parser stage)
    E0420, // interface method must not have a body
    E0421, // interface fields are not allowed
    E0422, // wrong number of generic type arguments for an interface
    E0423, // cyclic interface inheritance (`extends` cycle)
    E0424, // reserved: previously "multiple interface inheritance unsupported"
    E0425, // ambiguous default method from two implemented interfaces
    // Class/visibility E050x
    E0501,
    E0502,
    E0503,
    // self receiver E055x
    E0550,
    E0551,
    E0552,
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
    E0808, // reserved: async loop backedges are now preemptible
    E0809, // async fn return type must be the awaited value, not a task handle
    E0810, // looping synchronous helper is not preemptible in task context
    W0801, // async frame exceeds the large-frame warning threshold
    // Static members / implicit self E083x (willow-qsqf)
    E0830, // static property requires an initializer
    E0831, // `self` not available in static method (or explicit self on static)
    E0832, // cannot assign to immutable static property
    E0833, // cannot call mutating method on immutable static property
    E0834, // static member accessed through an instance
    E0835, // instance member accessed through a type
    E0836, // static interface members are not supported
    E0837, // `self` not available in static property initializer
    E0838, // static property used before it is initialized
    E0839, // static member hides inherited static member
    // Constructors / `new` / `init` E084x (willow-scq2)
    E0840, // constructor `init` must not declare a return type
    E0841, // constructor `init` cannot return a value
    E0842, // field not initialized by constructor
    E0843, // constructor cannot be called directly (use `new`)
    E0844, // unknown class in `new`
    E0845, // constructor argument count mismatch
    E0846, // constructor is not visible
    E0847, // object literal is deprecated/rejected — use `new`
    E0848, // subclass constructor requires unsupported base initialization
    E0849, // constructor `init` must declare explicit self
    E0850, // constructor `init` cannot use `static`/`fn` method syntax
    // Ternary E09xx
    E0901,
    E0902,
    E0903,
    E0904,
    E0905,
    // Lambda E10xx
    E1001,
    E1002,
    // Command-line / entry point E13xx
    E1301,
    E1302,
    E1303,
    // Formatting E14xx
    E1401,
    E1402,
    // Option/Result type inference E180x
    E1801,
    // Option/Result exhaustiveness E180x
    E1802,
    E1803,
    E1804,
    E1805,
    // ? operator E180x
    E1806,
    E1807,
    // Pass-by-reference / & and &mut E17xx
    E1701,
    E1702,
    E1703,
    E1704,
    E1705,
    E1706,
    E1707,
    E1708,
    // Match / enum E12xx
    E1201,
    E1202,
    E1205,
    E1206,
    E1207,
    E1208,
    E1209,
    // Match warnings W12xx (stored as Error severity with W prefix conceptually — use E code)
    W1201,
    // Standard library imports E20xx
    E2001, // cannot find type (needs import from std)
    E2002, // cannot find type (needs import from std) — Map variant
    E2003, // name defined multiple times (import vs local declaration)
    E2004, // import alias defined multiple times
    E2005, // package name `std` is reserved
    E2006, // no such item in a known std module
    E2007, // unknown std module
    // User module declarations E20xx
    E2008, // module declaration must appear before imports and items
    E2009, // duplicate module declaration
    E2010, // `std` cannot be a user module namespace
    E2011, // module declaration does not match import path
    // Send / Sync marker interfaces & data-race policy E24xx (willow-dgwo)
    E2401, // `Send`/`Sync` are compiler-known markers and cannot be implemented manually
    E2402, // cannot pass a non-Sync GC reference (or non-Send value) to an async call
    E2403, // channel item type must be Send (it crosses task/worker boundaries)
    E2404, // interface value crossing a task boundary is not Send
    E2405, // interface value crossing a task boundary is not Sync
    // Standard library import warnings W20xx
    W2002, // duplicate import
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── as_str: 新規追加コード ────────────────────────────────────────────────

    #[test]
    fn e1707_as_str_returns_correct_code() {
        assert_eq!(ErrorCode::E1707.as_str(), "E1707");
    }

    #[test]
    fn e1708_as_str_returns_correct_code() {
        assert_eq!(ErrorCode::E1708.as_str(), "E1708");
    }

    // ── as_str: 隣接コードとの混同がないことを確認 ────────────────────────────

    #[test]
    fn e1707_and_e1708_as_str_are_distinct() {
        assert_ne!(ErrorCode::E1707.as_str(), ErrorCode::E1708.as_str());
    }

    #[test]
    fn e1706_as_str_is_not_confused_with_e1707() {
        assert_ne!(ErrorCode::E1706.as_str(), ErrorCode::E1707.as_str());
    }

    // ── as_str: 既存コードが変更されていないことを確認 ───────────────────────

    #[test]
    fn existing_reference_error_codes_as_str_unchanged() {
        assert_eq!(ErrorCode::E1701.as_str(), "E1701");
        assert_eq!(ErrorCode::E1702.as_str(), "E1702");
        assert_eq!(ErrorCode::E1703.as_str(), "E1703");
        assert_eq!(ErrorCode::E1704.as_str(), "E1704");
        assert_eq!(ErrorCode::E1705.as_str(), "E1705");
        assert_eq!(ErrorCode::E1706.as_str(), "E1706");
    }

    // ── derive: PartialEq ────────────────────────────────────────────────────

    #[test]
    fn e1707_equals_itself() {
        assert_eq!(ErrorCode::E1707, ErrorCode::E1707);
    }

    #[test]
    fn e1708_equals_itself() {
        assert_eq!(ErrorCode::E1708, ErrorCode::E1708);
    }

    #[test]
    fn e1707_not_equal_to_e1708() {
        assert_ne!(ErrorCode::E1707, ErrorCode::E1708);
    }

    // ── derive: Clone / Copy ─────────────────────────────────────────────────

    #[test]
    fn e1707_can_be_cloned() {
        let code = ErrorCode::E1707;
        let cloned = code;
        assert_eq!(code, cloned);
    }

    #[test]
    fn e1708_can_be_copied() {
        let code = ErrorCode::E1708;
        let copy = code; // Copy トレイトによる暗黙コピー
        assert_eq!(code, copy);
    }

    // ── as_str: 全コードが自分自身の文字列表現と一致する ────────────────────

    #[test]
    fn all_error_codes_are_unique_and_well_formed() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for &code in ErrorCode::ALL {
            let s = code.as_str();
            // as_str is generated as stringify!(variant), so it always equals the
            // variant name; assert the E####/W#### shape and global uniqueness.
            assert!(
                s.starts_with('E') || s.starts_with('W'),
                "{code:?}.as_str() = {s:?} is not an E/W code"
            );
            assert!(seen.insert(s), "duplicate as_str {s:?}");
        }
        assert!(ErrorCode::ALL.len() >= 100);
        assert_eq!(ErrorCode::E0809.as_str(), "E0809");
        assert_eq!(ErrorCode::W2002.as_str(), "W2002");
    }
}
