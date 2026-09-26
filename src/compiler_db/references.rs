//! Compiler-owned symbol-use and reverse-reference queries. Coordinates are
//! payloads only; keys use semantic owners and declaration-local ordinals.
use super::incremental::SyntaxQueries;
use crate::ai::{Location, symbols::Reference};
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};

#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub(crate) struct SymbolId(pub String);
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub(crate) struct SymbolUseId {
    pub(crate) owner: SymbolId,
    pub(crate) ordinal: usize,
}

/// A sweep assigns innermost semantic owners in O((N+R) log(N+R)) time.
/// Offsets select ownership in this revision but are never returned as keys.
pub(crate) fn owners(points: &[Location], ranges: &[(Location, String)]) -> Vec<String> {
    let mut events = Vec::with_capacity(points.len() + 2 * ranges.len());
    for (i, (location, _)) in ranges.iter().enumerate() {
        events.push((location.path.as_str(), location.start, 1u8, i));
        events.push((location.path.as_str(), location.end, 0u8, i));
    }
    for (i, location) in points.iter().enumerate() {
        events.push((location.path.as_str(), location.start, 2u8, i));
    }
    events.sort_unstable();
    let mut active = BTreeMap::new();
    let mut path = "";
    let mut result = vec![String::new(); points.len()];
    for (current, _, kind, i) in events {
        if path != current {
            active.clear();
            path = current;
        }
        if kind == 2 {
            result[i] = serde_json::to_string(&(
                current,
                active
                    .first_key_value()
                    .map(|(_, owner)| *owner)
                    .unwrap_or("module"),
            ))
            .expect("owner serializes");
        } else {
            let (location, owner) = &ranges[i];
            let key = (std::cmp::Reverse(location.start), location.end, i);
            if kind == 1 {
                active.insert(key, owner.as_str());
            } else {
                active.remove(&key);
            }
        }
    }
    result
}

#[derive(Default)]
pub(crate) struct ReferenceQueries {
    tracking: std::cell::RefCell<std::rc::Weak<std::cell::RefCell<SyntaxQueries>>>,
}
impl ReferenceQueries {
    pub(crate) fn set_tracking(&self, syntax: &std::rc::Rc<std::cell::RefCell<SyntaxQueries>>) {
        *self.tracking.borrow_mut() = std::rc::Rc::downgrade(syntax);
    }
    pub(crate) fn capture(
        &self,
        references: Vec<Reference>,
        ranges: &[(Location, String)],
    ) -> Result<Vec<Reference>> {
        let Some(syntax) = self.tracking.borrow().upgrade() else {
            return Ok(references);
        };
        let points: Vec<_> = references.iter().map(|r| r.location.clone()).collect();
        let owners = owners(&points, ranges);
        let mut ordinals = HashMap::<String, usize>::new();
        let mut uses = Vec::new();
        let mut members = BTreeMap::<SymbolId, Vec<SymbolUseId>>::new();
        for (reference, owner) in references.into_iter().zip(owners) {
            let ordinal = ordinals.entry(owner.clone()).or_default();
            let id = SymbolUseId {
                owner: SymbolId(owner),
                ordinal: *ordinal,
            };
            *ordinal += 1;
            members
                .entry(SymbolId(reference.target.clone()))
                .or_default()
                .push(id.clone());
            uses.push((id, serde_json::to_value(reference)?));
        }
        let mut syntax = syntax.borrow_mut();
        syntax.capture_references(uses, &members)?;
        let mut result = Vec::new();
        for symbol in members.keys() {
            for reference in syntax.references(symbol.clone())? {
                result.push(serde_json::from_value::<Reference>(reference)?);
            }
        }
        result.sort_by(|a, b| {
            (
                &a.location.path,
                a.location.start,
                a.location.end,
                &a.target,
                &a.role,
            )
                .cmp(&(
                    &b.location.path,
                    b.location.start,
                    b.location.end,
                    &b.target,
                    &b.role,
                ))
        });
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn review_owner_identity_is_stable_across_offsets_and_nested_ranges() {
        let location = |path: &str, start, end| Location {
            path: path.into(),
            start,
            end,
        };
        for shift in [0, 10, 1000] {
            let ranges = vec![
                (location("a", shift, shift + 100), "class:C".into()),
                (
                    location("a", shift + 10, shift + 30),
                    "class:C/method:f".into(),
                ),
                (
                    location("a", shift + 40, shift + 80),
                    "class:C/method:g".into(),
                ),
                (location("b", shift, shift + 100), "function:f".into()),
            ];
            let points = vec![
                location("a", shift + 11, shift + 12),
                location("a", shift + 31, shift + 32),
                location("a", shift + 41, shift + 42),
                location("b", shift + 11, shift + 12),
            ];
            let identities = owners(&points, &ranges);
            assert_eq!(
                identities,
                vec![
                    r#"["a","class:C/method:f"]"#,
                    r#"["a","class:C"]"#,
                    r#"["a","class:C/method:g"]"#,
                    r#"["b","function:f"]"#
                ]
            );
        }
    }
}
