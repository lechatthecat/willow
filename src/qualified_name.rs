//! `::`-qualified names.
//!
//! Every compiler layer spells a namespaced name (`module::Item`,
//! `Class::method`, `$local::shapes::Point`) as a single string, and that
//! spelling is the key of many maps, so it stays a `String`. This module owns
//! what used to be scattered `split("::")` / `rsplit_once("::")` /
//! `format!("{a}::{b}")` code: [`QualifiedName`] is a zero-cost borrowed view
//! that decomposes a spelling, and its associated constructors are the only
//! place that joins segments with the separator.
//!
//! The view distinguishes the two decompositions that are easy to confuse:
//! [`QualifiedName::split_first`] divides at the FIRST separator (the root
//! segment, e.g. a package or import alias, from the rest) and
//! [`QualifiedName::split_last`] at the LAST one (the qualifier from the item,
//! e.g. the class `shapes::Point` from the method `area`).

use std::fmt::Display;

/// The separator between the segments of a qualified name.
pub(crate) const SEPARATOR: &str = "::";

/// A borrowed `::`-qualified name such as `std::io::println` or `Point`.
///
/// An unqualified name is a qualified name with exactly one segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct QualifiedName<'a>(&'a str);

impl<'a> QualifiedName<'a> {
    /// View `name` as a qualified name.
    pub(crate) fn new<S: AsRef<str> + ?Sized>(name: &'a S) -> Self {
        Self(name.as_ref())
    }

    /// Whether the name has more than one segment.
    pub(crate) fn is_qualified(self) -> bool {
        self.0.contains(SEPARATOR)
    }

    /// The segments, first to last. An empty name has one empty segment.
    pub(crate) fn segments(self) -> std::str::Split<'a, &'static str> {
        self.0.split(SEPARATOR)
    }

    /// The first segment: the whole name when it is unqualified.
    pub(crate) fn first_segment(self) -> &'a str {
        self.split_first().map_or(self.0, |(first, _)| first)
    }

    /// The last segment (the item): the whole name when it is unqualified.
    pub(crate) fn last_segment(self) -> &'a str {
        self.split_last().map_or(self.0, |(_, last)| last)
    }

    /// Divide at the first separator: `(root, rest)`.
    pub(crate) fn split_first(self) -> Option<(&'a str, &'a str)> {
        self.0.split_once(SEPARATOR)
    }

    /// Divide at the last separator: `(qualifier, item)`.
    pub(crate) fn split_last(self) -> Option<(&'a str, &'a str)> {
        self.0.rsplit_once(SEPARATOR)
    }

    /// Everything before the last separator, if the name is qualified.
    pub(crate) fn qualifier(self) -> Option<&'a str> {
        self.split_last().map(|(qualifier, _)| qualifier)
    }

    /// The remainder after a leading `qualifier::`, if the name starts with it.
    ///
    /// `qualifier` may itself be qualified (`std::collections`).
    pub(crate) fn strip_qualifier(self, qualifier: &str) -> Option<&'a str> {
        self.0.strip_prefix(qualifier)?.strip_prefix(SEPARATOR)
    }

    /// Whether the name starts with `qualifier::`.
    pub(crate) fn has_qualifier(self, qualifier: &str) -> bool {
        self.strip_qualifier(qualifier).is_some()
    }

    /// The remainder before a trailing `::tail`, if the name ends with it.
    ///
    /// `tail` may itself be qualified (`Point::area`).
    pub(crate) fn strip_tail(self, tail: &str) -> Option<&'a str> {
        self.0.strip_suffix(tail)?.strip_suffix(SEPARATOR)
    }

    /// Every `(qualifier, rest)` division of the name, from the last
    /// separator to the first (longest qualifier first).
    pub(crate) fn splits_from_last(self) -> impl Iterator<Item = (&'a str, &'a str)> {
        let name = self.0;
        name.rmatch_indices(SEPARATOR)
            .map(move |(offset, _)| (&name[..offset], &name[offset + SEPARATOR.len()..]))
    }

    /// The segments, last to first.
    pub(crate) fn rev_segments(self) -> std::str::RSplit<'a, &'static str> {
        self.0.rsplit(SEPARATOR)
    }

    /// Whether the name is qualified and its last segment is `item`.
    pub(crate) fn has_item(self, item: &str) -> bool {
        self.split_last().is_some_and(|(_, last)| last == item)
    }

    /// Whether `run` occurs anywhere in the name (an exact substring match on
    /// the run's spelling; see [`SegmentRun`]).
    pub(crate) fn contains_run(self, run: &SegmentRun) -> bool {
        self.0.contains(run.0.as_str())
    }

    /// The name with every occurrence of `from` replaced by `to` (exact
    /// substring replacement, like [`str::replace`]).
    pub(crate) fn replace_run(self, from: &SegmentRun, to: &SegmentRun) -> String {
        self.0.replace(from.0.as_str(), to.0.as_str())
    }

    /// The spelling `qualifier::item`.
    pub(crate) fn join(qualifier: impl Display, item: impl Display) -> String {
        format!("{qualifier}{SEPARATOR}{item}")
    }

    /// The spelling `qualifier::item` as a lazily written [`Display`], for
    /// building a longer name or text without an intermediate `String`.
    pub(crate) fn joined<Q: Display, I: Display>(qualifier: Q, item: I) -> Joined<Q, I> {
        Joined(qualifier, item)
    }

    /// The spelling `qualifier::` that prefixes every name inside `qualifier`.
    pub(crate) fn prefix(qualifier: impl Display) -> String {
        format!("{qualifier}{SEPARATOR}")
    }

    /// The spelling of `segments` joined in order.
    pub(crate) fn from_segments<I>(segments: I) -> String
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let mut name = String::new();
        for (index, segment) in segments.into_iter().enumerate() {
            if index > 0 {
                name.push_str(SEPARATOR);
            }
            name.push_str(segment.as_ref());
        }
        name
    }
}

/// A run of inner segments: `::seg1::...::segN::` followed by `next_prefix`,
/// the start of the segment after the run (empty to match any segment).
///
/// The spelling is built once so that matching in a loop allocates nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SegmentRun(String);

impl SegmentRun {
    pub(crate) fn new<I>(segments: I, next_prefix: &str) -> Self
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let mut run = String::new();
        for segment in segments {
            run.push_str(SEPARATOR);
            run.push_str(segment.as_ref());
        }
        run.push_str(SEPARATOR);
        run.push_str(next_prefix);
        Self(run)
    }
}

/// A `qualifier::item` spelling written on demand; see [`QualifiedName::joined`].
pub(crate) struct Joined<Q, I>(Q, I);

impl<Q: Display, I: Display> Display for Joined<Q, I> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{SEPARATOR}{}", self.0, self.1)
    }
}

#[cfg(test)]
mod tests {
    use super::{QualifiedName, SEPARATOR, SegmentRun};

    fn q(name: &str) -> QualifiedName<'_> {
        QualifiedName::new(name)
    }

    #[test]
    fn qualified_name_decomposes_at_first_and_last_separator() {
        let name = q("shapes::Point::area");
        assert_eq!(name.split_first(), Some(("shapes", "Point::area")));
        assert_eq!(name.split_last(), Some(("shapes::Point", "area")));
        assert_eq!(name.first_segment(), "shapes");
        assert_eq!(name.last_segment(), "area");
        assert_eq!(name.qualifier(), Some("shapes::Point"));
        assert!(name.is_qualified());
        assert_eq!(
            name.segments().collect::<Vec<_>>(),
            ["shapes", "Point", "area"]
        );
    }

    #[test]
    fn qualified_name_unqualified_is_one_segment() {
        let name = q("Point");
        assert_eq!(name.split_first(), None);
        assert_eq!(name.split_last(), None);
        assert_eq!(name.first_segment(), "Point");
        assert_eq!(name.last_segment(), "Point");
        assert_eq!(name.qualifier(), None);
        assert!(!name.is_qualified());
        assert!(!name.has_item("Point"));
        assert_eq!(name.segments().collect::<Vec<_>>(), ["Point"]);
        assert_eq!(q("").segments().collect::<Vec<_>>(), [""]);
    }

    #[test]
    fn qualified_name_qualifier_matches_whole_segments_only() {
        assert_eq!(
            q("std::io::println").strip_qualifier("std"),
            Some("io::println")
        );
        assert_eq!(
            q("std::collections::Map").strip_qualifier("std::collections"),
            Some("Map")
        );
        assert_eq!(q("stdx::io").strip_qualifier("std"), None);
        assert_eq!(q("std").strip_qualifier("std"), None);
        assert!(q("$local::a::B").has_qualifier("$local"));
        assert!(!q("$locals::B").has_qualifier("$local"));
        assert!(q("Logger::init").has_item("init"));
        assert!(!q("Logger::reinit").has_item("init"));
    }

    #[test]
    fn qualified_name_tail_and_reverse_splits() {
        assert_eq!(q("a::Point::area").strip_tail("area"), Some("a::Point"));
        assert_eq!(q("a::Point::area").strip_tail("Point::area"), Some("a"));
        assert_eq!(q("a::Point::area").strip_tail("rea"), None);
        assert_eq!(q("area").strip_tail("area"), None);
        assert_eq!(
            q("a::b::C").rev_segments().collect::<Vec<_>>(),
            ["C", "b", "a"]
        );
        assert_eq!(
            q("a::b::C").splits_from_last().collect::<Vec<_>>(),
            [("a::b", "C"), ("a", "b::C")]
        );
        assert_eq!(q("C").splits_from_last().count(), 0);
    }

    #[test]
    fn qualified_name_segment_runs_match_and_replace_exactly() {
        let lambda = SegmentRun::new(["function"], "<lambda ");
        let method = SegmentRun::new(["method"], "");
        let dispatch = SegmentRun::new(["function"], "$dispatch$");
        assert!(q("m.wi::function::<lambda f@1:2>").contains_run(&lambda));
        assert!(!q("function::<lambda f@1:2>").contains_run(&lambda));
        assert!(!q("m.wi::method::<lambda f@1:2>").contains_run(&lambda));
        assert!(q("m.wi::method::C::f#0").contains_run(&method));
        assert!(!q("m.wi::methods::C").contains_run(&method));
        assert_eq!(
            q("m.wi::method::C::f#0").replace_run(&method, &dispatch),
            "m.wi::function::$dispatch$C::f#0"
        );
        assert_eq!(
            q("m.wi::method::C::f#0").replace_run(&method, &SegmentRun::new(["function"], "")),
            "m.wi::function::C::f#0"
        );
        assert_eq!(q("plain").replace_run(&method, &dispatch), "plain");
    }

    #[test]
    fn qualified_name_construction_round_trips() {
        assert_eq!(QualifiedName::join("a::b", "C"), "a::b::C");
        assert_eq!(QualifiedName::prefix("pkg"), format!("pkg{SEPARATOR}"));
        assert_eq!(QualifiedName::from_segments(["a", "b", "C"]), "a::b::C");
        assert_eq!(QualifiedName::from_segments(["C"]), "C");
        assert_eq!(QualifiedName::from_segments(Vec::<String>::new()), "");
        assert_eq!(
            QualifiedName::join(QualifiedName::joined("a", "b"), "C#0"),
            "a::b::C#0"
        );
        assert_eq!(QualifiedName::joined("m", 1).to_string(), "m::1");
        let joined = QualifiedName::join("m", "Class");
        assert_eq!(q(&joined).split_last(), Some(("m", "Class")));
    }
}
