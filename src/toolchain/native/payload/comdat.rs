//! Index COFF section selection once; ObjectComdat::sections scans the complete
//! symbol table per group in object, so it is unsuitable for large archives.
use super::*;
use object::{ObjectSection, RelocationTarget, SectionIndex, SymbolFlags};

#[derive(Debug, Clone)]
pub(in crate::toolchain::native) enum Definition {
    Strong,
    Selected {
        key: std::sync::Arc<str>,
        selection: u8,
        size: u64,
        fingerprint: String,
    },
}
impl Definition {
    pub fn compatible(&self, other: &Self) -> bool {
        let (
            Self::Selected {
                key,
                selection,
                size,
                fingerprint,
            },
            Self::Selected {
                key: bkey,
                selection: bselection,
                size: bsize,
                fingerprint: bfingerprint,
            },
        ) = (self, other)
        else {
            return false;
        };
        if key != bkey || selection != bselection {
            return false;
        }
        match *selection {
            2 | 6 | 7 => true, // ANY, LARGEST, NEWEST
            3 => size == bsize,
            4 => size == bsize && fingerprint == bfingerprint,
            _ => false,
        }
    }
}

pub(super) fn sections<'a>(file: &impl Object<'a>) -> Result<HashMap<SectionIndex, Definition>> {
    let mut flags = HashMap::new();
    let mut keys = HashMap::new();
    for symbol in file.symbols() {
        #[cfg(test)]
        SYMBOL_VISITS.with(|n| n.set(n.get() + 1));
        if let Some(section) = symbol.section_index() {
            if let SymbolFlags::CoffSection {
                selection,
                associative_section,
                ..
            } = symbol.flags()
            {
                if selection.0 != 0 {
                    flags.insert(section, (selection.0, associative_section));
                }
            } else if symbol.is_definition() && symbol.is_global() {
                keys.entry(section).or_insert(symbol.name()?.to_owned());
            }
        }
    }
    let mut roots = HashMap::new();
    for &start in flags.keys() {
        let mut current = start;
        let mut path = Vec::new();
        let mut visiting = std::collections::HashSet::new();
        let root = loop {
            if let Some(&root) = roots.get(&current) {
                break root;
            }
            ensure!(
                visiting.insert(current),
                "native_invalid_comdat: associative cycle"
            );
            path.push(current);
            let &(selection, parent) = flags
                .get(&current)
                .context("native_invalid_comdat: missing parent")?;
            if selection != 5 {
                break current;
            }
            current = parent.context("native_invalid_comdat: missing association")?;
        };
        for section in path {
            roots.insert(section, root);
        }
    }
    let mut primary = HashMap::new();
    for &root in roots.values() {
        if primary.contains_key(&root) {
            continue;
        }
        let selection = flags[&root].0;
        let section = file.section_by_index(root)?;
        let key = keys
            .get(&root)
            .cloned()
            .unwrap_or_else(|| section.name().unwrap_or("").to_owned());
        let mut h = Sha256::new();
        if selection == 4 {
            h.update(section.data()?);
            // Normalize relocation targets: symbol table indices are local to
            // each object and cannot establish exact-match equivalence.
            for (offset, relocation) in section.relocations() {
                h.update(offset.to_le_bytes());
                h.update(
                    format!(
                        "{:?}/{:?}/{}/{}/{:?}",
                        relocation.kind(),
                        relocation.encoding(),
                        relocation.size(),
                        relocation.addend(),
                        relocation.flags()
                    )
                    .as_bytes(),
                );
                let target = match relocation.target() {
                    RelocationTarget::Symbol(index) => {
                        file.symbol_by_index(index)?.name()?.to_owned()
                    }
                    RelocationTarget::Section(index) => {
                        file.section_by_index(index)?.name()?.to_owned()
                    }
                    other => format!("{other:?}"),
                };
                h.update((target.len() as u64).to_le_bytes());
                h.update(target.as_bytes());
            }
        }
        primary.insert(
            root,
            Definition::Selected {
                key: key.into(),
                selection,
                size: section.size(),
                fingerprint: format!("{:x}", crate::hex::Hex(&h.finalize())),
            },
        );
    }
    Ok(roots
        .into_iter()
        .map(|(section, root)| (section, primary[&root].clone()))
        .collect())
}

#[cfg(test)]
thread_local! { static SYMBOL_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
pub(in crate::toolchain::native) fn take_symbol_visits() -> usize {
    SYMBOL_VISITS.with(|n| n.replace(0))
}
