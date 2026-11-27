use super::*;

#[test]
fn unit_symbols_01_every_runtime_abi_symbol_is_reserved() {
    for symbol in abi::RUNTIME_SYMBOLS {
        assert!(
            is_reserved_symbol(symbol.name),
            "runtime symbol `{}` must be reserved",
            symbol.name
        );
    }
}

/// Perspective 2: the reservation covers the whole `willow_` namespace, not
/// only the symbols that exist today. A runtime symbol added later must not
/// silently start colliding with user code that already compiled.
#[test]
fn unit_symbols_02_willow_prefix_is_reserved_even_when_unused() {
    assert!(is_reserved_symbol("willow_not_a_real_runtime_symbol"));
    assert!(is_reserved_symbol("willow_x"));
}

/// Perspective 3: the compiler's own generated data lives in `__willow_`.
#[test]
fn unit_symbols_03_compiler_internal_namespace_is_reserved() {
    assert!(is_reserved_symbol("__willow_static_init"));
    assert!(is_reserved_symbol("__willow_str_0"));
    assert!(is_reserved_symbol("__willow_gc_tlab_state"));
}

/// Perspective 4: `fn main` is what the compiler assigns
/// `willow_user_main`, so reserving that name would reject every program.
#[test]
fn unit_symbols_04_user_main_symbol_is_not_reserved() {
    assert!(!is_reserved_symbol(USER_MAIN_SYMBOL));
}

/// Perspective 5: `willow__` with two underscores is user space. No runtime
/// symbol has that shape, so a free function spelled that way stays legal —
/// the reservation must not swallow it.
#[test]
fn unit_symbols_05_double_underscore_after_willow_is_user_space() {
    assert!(!is_reserved_symbol("willow__foo"));
    assert!(!is_reserved_symbol("willow__static__x"));
    assert!(!is_reserved_symbol("willow__Inner__method"));
}

/// Perspective 6: the reservation is prefix-anchored, not a substring
/// search. A name that merely contains `willow_` is user space.
#[test]
fn unit_symbols_06_reservation_is_anchored_at_the_start() {
    assert!(!is_reserved_symbol("my_willow_helper"));
    assert!(!is_reserved_symbol("Wrapper__willow_thing"));
}

/// Perspective 7: case matters. `Willow_` is a perfectly ordinary user
/// name.
#[test]
fn unit_symbols_07_reservation_is_case_sensitive() {
    assert!(!is_reserved_symbol("Willow_helper"));
    assert!(!is_reserved_symbol("WILLOW_HELPER"));
}

/// Perspective 8: a name that shares a prefix with `willow` but is not
/// followed by the separator is unrelated.
#[test]
fn unit_symbols_08_prefix_requires_the_separator() {
    assert!(!is_reserved_symbol("willowfoo"));
    assert!(!is_reserved_symbol("willow"));
}

/// Perspective 9: a single leading underscore is not the compiler's
/// namespace; only `__willow_` is.
#[test]
fn unit_symbols_09_single_underscore_prefix_is_user_space() {
    assert!(!is_reserved_symbol("_willow_thing"));
    assert!(!is_reserved_symbol("_private_helper"));
}

/// Perspective 10: ordinary user symbols, including the mangled shapes the
/// backend produces for modules, classes, and static properties, are never
/// reserved. A false positive here would reject working programs.
#[test]
fn unit_symbols_10_ordinary_mangled_shapes_are_not_reserved() {
    for symbol in [
        "main",
        "add",
        "math.add",
        "Point.area",
        "command_line_args.run",
        "Config.default_size$static",
        "Shape$as$Named$vtable",
    ] {
        assert!(!is_reserved_symbol(symbol), "`{symbol}` must stay usable");
    }
}

/// Perspective 51: a mangled symbol is never reserved, even when its first
/// component reads like a runtime name (shared compiler infrastructure, catalog item 8 phase
/// 2). A runtime symbol is always a bare C identifier, so a joined symbol
/// cannot be one — and before the scheme became injective, a class named
/// `willow_box` was rejected for a collision that could not happen.
#[test]
fn unit_symbols_51_mangled_symbols_are_outside_the_reserved_namespace() {
    for symbol in [
        "willow_box.get",
        "willow_array_new.helper",
        "willow_gc_collect$vtable",
        "__willow_static_init.x",
        "willow_runtime.Value.print",
    ] {
        assert!(
            !is_reserved_symbol(symbol),
            "`{symbol}` carries a separator and cannot be a runtime symbol"
        );
    }
}

/// Perspective 52: the narrowing in perspective 51 is exactly that — a
/// narrowing. The bare forms of the same names are still reserved, and a
/// bare name is what an entry-file free function actually produces.
#[test]
fn unit_symbols_52_bare_runtime_names_are_still_reserved() {
    assert!(is_reserved_symbol(rt::willow_array_new));
    assert!(is_reserved_symbol(rt::willow_gc_collect));
    assert!(is_reserved_symbol("willow_box"));
    assert!(is_reserved_symbol("__willow_static_init"));
}

/// Perspective 11: the empty string is not reserved. Nothing produces it,
/// but the predicate must not panic or over-match on it.
#[test]
fn unit_symbols_11_empty_symbol_is_not_reserved() {
    assert!(!is_reserved_symbol(""));
}
