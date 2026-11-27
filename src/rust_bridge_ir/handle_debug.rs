//! Debug-only output traversal. Repeated record definitions share DAG nodes.
use super::{Record, RustBridgeSymbol, Scalar};
use std::collections::HashMap;
use willow_abi::handle_debug as op;

impl RustBridgeSymbol {
    pub(crate) fn handle_debug_descriptor(&self) -> Option<Vec<u64>> {
        let mut builder = Builder {
            words: vec![0],
            records: self.records.iter().map(|r| (r.name.as_str(), r)).collect(),
            record_nodes: HashMap::new(),
        };
        let root = builder.node(&self.output_type);
        builder.words[0] = root;
        (root != 0).then_some(builder.words)
    }
}

struct Builder<'a> {
    words: Vec<u64>,
    records: HashMap<&'a str, &'a Record>,
    record_nodes: HashMap<String, u64>,
}

impl Builder<'_> {
    fn append(&mut self, words: &[u64]) -> u64 {
        let index = self.words.len() as u64;
        self.words.extend_from_slice(words);
        index
    }

    fn node(&mut self, ty: &Scalar) -> u64 {
        match ty {
            Scalar::Opaque(_) => self.append(&[op::HANDLE]),
            Scalar::Array(child) | Scalar::Option(child) => {
                let node = self.node(child);
                if node == 0 {
                    0
                } else if matches!(ty, Scalar::Array(_)) {
                    self.append(&[op::ARRAY, node])
                } else if child.reference() && !matches!(**child, Scalar::Option(_)) {
                    self.append(&[op::NULLABLE, node])
                } else {
                    self.append(&[op::TAGGED, node, 0])
                }
            }
            Scalar::Result(ok, err) => {
                let a = self.node(ok);
                let b = self.node(err);
                if a == 0 && b == 0 {
                    0
                } else {
                    self.append(&[op::TAGGED, a, b])
                }
            }
            Scalar::Record(name) => {
                if let Some(&node) = self.record_nodes.get(name) {
                    return node;
                }
                let record = self.records[name.as_str()];
                let mut fields = Vec::new();
                let mut offset = 1;
                for (_, ty) in &record.fields {
                    let node = self.node(ty);
                    if node != 0 {
                        fields.extend([offset, node]);
                    }
                    offset += 1 + u64::from(ty.pair());
                }
                let node = if fields.is_empty() {
                    0
                } else {
                    let node = self.append(&[op::RECORD, (fields.len() / 2) as u64]);
                    self.words.extend(fields);
                    node
                };
                self.record_nodes.insert(name.clone(), node);
                node
            }
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_handle_descriptors_share_record_diamonds() {
        for depth in [1, 16, 128] {
            let mut records = Vec::new();
            let mut child = Scalar::Opaque("Handle".into());
            for level in 0..depth {
                let name = format!("Record{level}");
                records.push(Record {
                    name: name.clone(),
                    fields: vec![
                        ("left".into(), child.clone()),
                        ("right".into(), child.clone()),
                    ],
                });
                child = Scalar::Record(name);
            }
            let mut symbol = RustBridgeSymbol::new("make".into(), vec![], child);
            symbol.records = records.into();
            let words = symbol.handle_debug_descriptor().unwrap();
            assert_eq!(words.len(), 6 * depth + 3);
            eprintln!("record_depth={depth} descriptor_words={}", words.len());
        }
    }
}
