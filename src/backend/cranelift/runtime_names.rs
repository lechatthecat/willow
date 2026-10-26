//! Render diagnostic strings at compile time; linker identities stay unchanged.
use super::{Codegen, UnitCodegenContext};
use std::collections::HashMap;

impl Codegen {
    pub(crate) fn set_runtime_names(&mut self, names: HashMap<String, String>) {
        self.output.runtime_names = names;
    }
}

impl UnitCodegenContext<'_> {
    pub(super) fn runtime_name(&self, name: &str) -> String {
        render(name, &self.output.runtime_names)
    }
}

#[cfg(test)]
thread_local! {
    static LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn render(name: &str, instances: &HashMap<String, String>) -> String {
    let mut result = String::with_capacity(name.len());
    for (index, segment) in name.split("::").enumerate() {
        if index != 0 {
            result.push_str("::");
        }
        #[cfg(test)]
        LOOKUPS.with(|count| count.set(count.get() + 1));
        result.push_str(instances.get(segment).map_or(segment, String::as_str));
    }
    crate::diagnostics::diagnostic::source_names(result, &HashMap::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_source_names_lookup_scaling() {
        for count in [1, 16, 128, 1024] {
            let instances: HashMap<_, _> = (0..count)
                .map(|i| (format!("f{i}$mono${i:064x}"), format!("f{i}<i64>")))
                .collect();
            LOOKUPS.with(|n| n.set(0));
            for i in 0..count {
                let name = format!("$local::module::f{i}$mono${i:064x}");
                assert_eq!(render(&name, &instances), format!("module::f{i}<i64>"));
            }
            assert_eq!(LOOKUPS.with(|n| n.get()), 3 * count);
            LOOKUPS.with(|n| n.set(0));
            let deep = std::iter::repeat_n("module", count)
                .collect::<Vec<_>>()
                .join("::");
            assert_eq!(render(&deep, &instances), deep);
            assert_eq!(LOOKUPS.with(|n| n.get()), count);
        }
    }

    #[test]
    fn runtime_source_names_spellings() {
        let mono = format!("$mono${:064x}", 1);
        let pkg = format!("$pkg{:032x}", 2);
        let instances = HashMap::from([
            (format!("a{mono}"), "a<i64>".into()),
            (format!("Box{mono}"), "Box<Array<String>>".into()),
        ]);
        for (input, expected) in [
            ("".into(), ""),
            ("main".into(), "main"),
            ("日本語".into(), "日本語"),
            ("Scope::init".into(), "Scope::init"),
            ("$receiver::$local::Scope::init".into(), "Scope::init"),
            (format!("$receiver::{pkg}::Scope::init"), "Scope::init"),
            ("math::Scope::get".into(), "math::Scope::get"),
            ("$local::Scope::init".into(), "Scope::init"),
            ("$local::value::Scope::init".into(), "value::Scope::init"),
            (format!("{pkg}::value::Scope::init"), "value::Scope::init"),
            (format!("a{mono}"), "a<i64>"),
            (format!("math::a{mono}"), "math::a<i64>"),
            (format!("$local::math::a{mono}"), "math::a<i64>"),
            (format!("{pkg}::math::a{mono}"), "math::a<i64>"),
            (format!("Box{mono}::get"), "Box<Array<String>>::get"),
            (format!("Box{mono}::init"), "Box<Array<String>>::init"),
            (format!("{pkg}::Box{mono}::get"), "Box<Array<String>>::get"),
            (format!("$local::Box{mono}::get"), "Box<Array<String>>::get"),
            (format!("other{mono}"), "other"),
            (format!("other{mono}::get"), "other::get"),
            ("a$mono$short".into(), "a$mono$short"),
            ("$pkgshort::a".into(), "$pkgshort::a"),
            ("a<i64, String>".into(), "a<i64, String>"),
            ("Box<value::Item>::get".into(), "Box<value::Item>::get"),
        ] {
            assert_eq!(render(&input, &instances), expected, "{input}");
        }
    }
}
