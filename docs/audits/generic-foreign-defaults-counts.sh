#!/usr/bin/env bash
# generic-foreign-defaults: deterministic counts for cross-module GENERIC interface defaults.
# Usage: docs/audits/generic-foreign-defaults-counts.sh <willow binary>
# N receiving classes implement util::Get<i64> or util::Get<String> (two
# distinct instantiations), in the entry and in a second receiving module.
# Prints per-copy LIR compilations and query computations.
set -euo pipefail
willow=$(realpath "${1:?willow binary}")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cat > "$work/util.wi" <<'WI'
module util;
fn helper(x: i64) -> i64 { return x * 2; }
fn keep<T>(x: T) -> T { return x; }
pub interface Get<T> {
    fn raw(self) -> T;
    fn answer(self) -> i64 { let f = |x: i64| -> i64 { return helper(x); }; return f(21); }
    fn echo(self) -> T { let w: T = keep(self.raw()); return w; }
}
WI
printf 'N\tdefault_lir\tlambda_lir\ttyped_body\tlir_body\tchecked_unit\n'
for n in 1 2 4 8 16 32; do
    cls() { # $1 prefix, $2 index
        if [ $(($2 % 2)) -eq 0 ]; then
            echo "pub class $1$2 implements util::Get<i64> { pub fn raw(self) -> i64 { return 1; } }"
        else
            echo "pub class $1$2 implements util::Get<String> { pub fn raw(self) -> String { return \"s\"; } }"
        fi
    }
    {
        echo "module shapes; import util;"
        for i in $(seq 1 "$n"); do cls S "$i"; done
        echo "pub fn total() -> i64 { let mut t = 0;"
        for i in $(seq 1 "$n"); do echo "t = t + new S$i().answer(); new S$i().echo();"; done
        echo "return t; }"
    } > "$work/shapes.wi"
    {
        echo "import util; import shapes;"
        for i in $(seq 1 "$n"); do cls C "$i"; done
        echo "fn main() { let mut t = shapes::total();"
        for i in $(seq 1 "$n"); do echo "t = t + new C$i().answer(); new C$i().echo();"; done
        echo "println(t); }"
    } > "$work/main.wi"
    out=$(cd "$work" && WILLOW_QUERY_STATS=1 WILLOW_LIR_LOG=1 "$willow" build main.wi -o app 2>&1)
    default_lir=$(grep -c '^\[lir\] compiling `.*::\(answer\|echo\)`' <<<"$out" || true)
    lambda_lir=$(grep -c '^\[lir\] compiling `.*\$lambda' <<<"$out" || true)
    stat() { grep -o "$1\[calls=[0-9]*,hits=[0-9]*,computations=[0-9]*" <<<"$out" | sed 's/.*computations=//'; }
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$n" "$default_lir" "$lambda_lir" \
        "$(stat typed_body)" "$(stat lir_body)" "$(stat checked_unit)"
    actual=$("$work/app")
    [ "$actual" = "$((2 * n * 42))" ] || { echo "unexpected output $actual" >&2; exit 1; }
done
