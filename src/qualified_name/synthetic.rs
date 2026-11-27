//! Creation-time provenance for generated name segments.
//!
//! The symbol owner stores this registry. Rendering consults exact registered
//! segments; it never infers a role from the spelling of a prefix or suffix.

use std::collections::HashMap;

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
pub enum SyntheticKind {
    #[default]
    Source,
    InterfaceDefault,
    ContractArity,
    ContractInto,
    StaticInitializer,
    Dispatch,
    Indirect {
        slot: u8,
        wildcard: bool,
    },
    Monomorph,
    NominalAlias,
    CompoundArray,
    ForIndex,
    PackageNamespace,
    LocalNamespace,
    ForeignReceiver,
    StdNamespace,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SyntheticName {
    pub(crate) kind: SyntheticKind,
    pub(crate) source: String,
    pub(crate) specialization: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct NameMetadata {
    names: HashMap<String, SyntheticName>,
}

#[derive(Clone, Copy)]
enum Projection {
    Diagnostic,
    Source,
}

impl NameMetadata {
    pub(crate) fn register(&mut self, spelling: &str, source: &str, kind: SyntheticKind) {
        self.insert(spelling, source, kind, None);
    }

    pub(crate) fn register_monomorph(
        &mut self,
        spelling: &str,
        source: &str,
        specialization: &str,
    ) {
        self.insert(
            spelling,
            source,
            SyntheticKind::Monomorph,
            Some(specialization),
        );
    }

    fn insert(
        &mut self,
        spelling: &str,
        source: &str,
        kind: SyntheticKind,
        specialization: Option<&str>,
    ) {
        let metadata = SyntheticName {
            kind,
            source: source.to_owned(),
            specialization: specialization.map(str::to_owned),
        };
        self.names.insert(spelling.to_owned(), metadata);
        if let Some((_, segment)) = super::QualifiedName::new(spelling).split_last() {
            let source_segment = super::QualifiedName::new(source).last_segment();
            self.names.insert(
                segment.to_owned(),
                SyntheticName {
                    kind,
                    source: source_segment.to_owned(),
                    specialization: specialization.map(str::to_owned),
                },
            );
        }
    }

    pub(crate) fn get(&self, spelling: &str) -> Option<&SyntheticName> {
        self.names.get(spelling)
    }

    pub(crate) fn kind(&self, spelling: &str) -> SyntheticKind {
        self.get(spelling)
            .map_or(SyntheticKind::Source, |metadata| metadata.kind)
    }

    pub(crate) fn source(&self, spelling: &str) -> Option<&str> {
        self.get(spelling).map(|metadata| metadata.source.as_str())
    }

    pub(crate) fn specialization(&self, spelling: &str) -> Option<&str> {
        self.get(spelling)?.specialization.as_deref()
    }

    /// Project registered names inside prose in one pass. Each name segment
    /// performs one indexed lookup, independent of the number of registered
    /// declarations. Punctuation and unknown names retain their exact bytes.
    pub(crate) fn project(&self, text: &str, packages: &HashMap<String, String>) -> String {
        self.project_with_visits(text, packages).0
    }

    /// Source graph identities retain bundled-standard-library and foreign
    /// receiver namespaces. Only canonical package/local qualification and
    /// specialization segments are erased, using source `::` separators.
    pub(crate) fn source_spelling(&self, text: &str) -> String {
        self.project_in_mode(text, &HashMap::new(), Projection::Source)
            .0
    }

    fn project_with_visits(
        &self,
        text: &str,
        packages: &HashMap<String, String>,
    ) -> (String, usize) {
        self.project_in_mode(text, packages, Projection::Diagnostic)
    }

    fn project_in_mode(
        &self,
        text: &str,
        packages: &HashMap<String, String>,
        mode: Projection,
    ) -> (String, usize) {
        let mut output = String::with_capacity(text.len());
        let mut start = None;
        let mut copied = 0;
        let mut visits = 0;
        for (offset, character) in text.char_indices() {
            let name_character =
                character.is_alphanumeric() || matches!(character, '_' | '$' | ':' | '.');
            if name_character {
                start.get_or_insert(offset);
            } else if let Some(start) = start.take() {
                output.push_str(&text[copied..start]);
                self.project_token(
                    &text[start..offset],
                    packages,
                    mode,
                    &mut output,
                    &mut visits,
                );
                copied = offset;
            }
        }
        if let Some(start) = start {
            output.push_str(&text[copied..start]);
            self.project_token(&text[start..], packages, mode, &mut output, &mut visits);
        } else {
            output.push_str(&text[copied..]);
        }
        (output, visits)
    }

    fn project_token(
        &self,
        token: &str,
        packages: &HashMap<String, String>,
        mode: Projection,
        output: &mut String,
        visits: &mut usize,
    ) {
        if matches!(mode, Projection::Source) {
            self.source_token(token, output, visits);
            return;
        }
        let mut start = 0;
        while start < token.len() {
            let (end, next) = segment_end(token, start);
            let segment = &token[start..end];
            let separator = &token[end..next];
            *visits += 1;
            match self.get(segment) {
                Some(metadata) => match metadata.kind {
                    SyntheticKind::LocalNamespace if !separator.is_empty() => {}
                    SyntheticKind::ForeignReceiver if separator == "::" => {}
                    SyntheticKind::PackageNamespace if !separator.is_empty() => {
                        if let Some(package) = packages.get(segment) {
                            output.push_str(package);
                        }
                    }
                    SyntheticKind::StdNamespace if separator == "::" => {
                        let (module_end, after_module) = segment_end(token, next);
                        if &token[module_end..after_module] == "::" {
                            start = after_module;
                            continue;
                        }
                        output.push_str(segment);
                        output.push_str(separator);
                    }
                    SyntheticKind::Monomorph => {
                        output.push_str(&metadata.source);
                        output.push_str(separator);
                    }
                    _ => {
                        output.push_str(segment);
                        output.push_str(separator);
                    }
                },
                None => {
                    output.push_str(segment);
                    output.push_str(separator);
                }
            }
            start = next;
        }
    }

    fn source_token(&self, token: &str, output: &mut String, visits: &mut usize) {
        let mut retained = false;
        for segment in super::QualifiedName::new(token).segments() {
            *visits += 1;
            let source = match self.get(segment) {
                Some(metadata) => match metadata.kind {
                    SyntheticKind::PackageNamespace | SyntheticKind::LocalNamespace => continue,
                    SyntheticKind::Monomorph => metadata.source.as_str(),
                    _ => segment,
                },
                None => segment,
            };
            if retained {
                output.push_str(super::SEPARATOR);
            }
            output.push_str(source);
            retained = true;
        }
    }
}

/// A structural separator is either `::` or the backend's `.` spelling.
fn segment_end(token: &str, start: usize) -> (usize, usize) {
    let bytes = token.as_bytes();
    let mut offset = start;
    while offset < bytes.len() {
        if bytes[offset] == b'.' {
            return (offset, offset + 1);
        }
        if bytes[offset] == b':' && bytes.get(offset + 1) == Some(&b':') {
            return (offset, offset + 2);
        }
        offset += 1;
    }
    (offset, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> NameMetadata {
        let mut names = NameMetadata::default();
        names.register("$local", "", SyntheticKind::LocalNamespace);
        names.register("$receiver", "", SyntheticKind::ForeignReceiver);
        names.register("$std", "", SyntheticKind::StdNamespace);
        names.register("$pkg0123", "", SyntheticKind::PackageNamespace);
        names.register_monomorph("Box$mono$0123", "Box", "0123");
        names
    }

    #[test]
    fn creation_metadata_controls_kind_independently_of_prefixes() {
        let mut names = metadata();
        for spelling in ["$default$x", "$contract$arity0", "$mono$x", "$pkgunknown"] {
            assert_eq!(names.kind(spelling), SyntheticKind::Source);
            assert_eq!(names.project(spelling, &HashMap::new()), spelling);
        }
        names.register("ordinary", "method", SyntheticKind::InterfaceDefault);
        assert_eq!(names.kind("ordinary"), SyntheticKind::InterfaceDefault);
        assert_eq!(names.source("ordinary"), Some("method"));
        assert_eq!(names.specialization("ordinary"), None);
    }

    #[test]
    fn registered_projections_preserve_punctuation_and_unknown_names() {
        let names = metadata();
        let packages = HashMap::from([("$pkg0123".to_owned(), "dep::".to_owned())]);
        for (input, expected) in [
            ("", ""),
            ("ordinary::Name", "ordinary::Name"),
            ("日本語", "日本語"),
            ("$local::a::Cell", "a::Cell"),
            ("$local.a.Cell", "a.Cell"),
            ("$localish::Keep", "$localish::Keep"),
            ("$local", "$local"),
            ("$receiver::$local::Cell", "Cell"),
            ("$receiverish::Cell", "$receiverish::Cell"),
            ("$std::collections::Array", "Array"),
            ("$std::collections", "$std::collections"),
            ("$pkg0123::a::Cell", "dep::a::Cell"),
            ("$pkg0123.a.Cell", "dep::a.Cell"),
            ("$pkgunknown::Cell", "$pkgunknown::Cell"),
            ("Box$mono$0123::get", "Box::get"),
            ("Other$mono$0123::get", "Other$mono$0123::get"),
            ("Box$mono$0123x", "Box$mono$0123x"),
            ("Box$mono$", "Box$mono$"),
            ("Array<$local::a::Box$mono$0123>", "Array<a::Box>"),
            (
                "expected `$local::日本語`, found fn(Box$mono$0123) -> $receiver::Cell;",
                "expected `日本語`, found fn(Box) -> Cell;",
            ),
        ] {
            assert_eq!(names.project(input, &packages), expected, "{input}");
        }
    }

    #[test]
    fn monomorph_metadata_retains_complete_source_and_specialization() {
        let mut names = metadata();
        names.register_monomorph("$local::m::Item$mono$abc", "$local::m::Item", "abc");
        assert_eq!(
            names.source("$local::m::Item$mono$abc"),
            Some("$local::m::Item")
        );
        assert_eq!(names.source("Item$mono$abc"), Some("Item"));
        assert_eq!(
            names.specialization("$local::m::Item$mono$abc"),
            Some("abc")
        );
        assert_eq!(
            names.project("$local::m::Item$mono$abc::get", &HashMap::new()),
            "m::Item::get"
        );
    }

    #[test]
    fn source_projection_preserves_its_own_namespace_and_separator_rules() {
        let names = metadata();
        for (input, expected) in [
            (
                "$std::collections::Box$mono$0123::push",
                "$std::collections::Box::push",
            ),
            ("$receiver::Box$mono$0123", "$receiver::Box"),
            (
                "$receiver::$local::module::Box$mono$0123",
                "$receiver::module::Box",
            ),
            ("$pkg0123::module::Box$mono$0123::get", "module::Box::get"),
            ("$local::module::Box$mono$0123::get", "module::Box::get"),
            ("$local.module.Box$mono$0123", "$local.module.Box$mono$0123"),
            ("$pkg0123.module.Box", "$pkg0123.module.Box"),
            ("$std.collections.Box", "$std.collections.Box"),
            ("$receiver.Box", "$receiver.Box"),
            ("<lambda Box$mono$0123@10:20>", "<lambda Box@10:20>"),
            (
                "<lambda $local::module::Box$mono$0123@30:40>",
                "<lambda module::Box@30:40>",
            ),
            ("$local", ""),
            ("module::$local", "module"),
            ("module::$local::", "module::"),
            ("$std::", "$std::"),
            ("", ""),
        ] {
            assert_eq!(names.source_spelling(input), expected, "{input}");
        }
        assert_eq!(
            names.project("$std::collections::Box$mono$0123", &HashMap::new()),
            "Box"
        );
        assert_eq!(
            names.project("$local.module.Box$mono$0123", &HashMap::new()),
            "module.Box"
        );
    }

    #[test]
    fn projection_work_is_linear_in_segments_independent_of_registry_size() {
        for size in [1, 16, 128, 1024] {
            let mut names = metadata();
            for i in 0..size {
                names.register_monomorph(&format!("Item$mono${i}"), "Item", &i.to_string());
            }
            let input = std::iter::repeat_n("$local::m::Item$mono$0", size)
                .collect::<Vec<_>>()
                .join(", ");
            let (output, visits) = names.project_with_visits(&input, &HashMap::new());
            assert_eq!(visits, 3 * size);
            assert_eq!(output.matches("m::Item").count(), size);
            let (source, source_visits) =
                names.project_in_mode(&input, &HashMap::new(), Projection::Source);
            assert_eq!(source, output);
            assert_eq!(source_visits, 3 * size);
            let deep = std::iter::repeat_n("module", size)
                .collect::<Vec<_>>()
                .join("::");
            assert_eq!(
                names.project_with_visits(&deep, &HashMap::new()),
                (deep, size)
            );
        }
    }
}
