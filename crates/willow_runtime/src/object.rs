use std::collections::HashMap;

pub const NO_BASE_TYPE_ID: u32 = 0;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeObjectHeader {
    pub type_id: u32,
    pub field_count: u32,
    pub base_type_id: u32,
    pub vtable: usize,
}

impl RuntimeObjectHeader {
    pub fn new(type_id: u32, field_count: u32, base_type_id: Option<u32>) -> Self {
        Self {
            type_id,
            field_count,
            base_type_id: base_type_id.unwrap_or(NO_BASE_TYPE_ID),
            vtable: 0,
        }
    }

    pub fn has_base(&self) -> bool {
        self.base_type_id != NO_BASE_TYPE_ID
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeTypeInfo {
    pub type_id: u32,
    pub class_name: String,
    pub field_count: usize,
    pub gc_ref_fields: Vec<bool>,
    pub base_type_id: Option<u32>,
    pub vtable: Option<usize>,
}

impl RuntimeTypeInfo {
    pub fn new(
        type_id: u32,
        class_name: impl Into<String>,
        field_count: usize,
        gc_ref_fields: Vec<bool>,
        base_type_id: Option<u32>,
    ) -> Self {
        Self {
            type_id,
            class_name: class_name.into(),
            field_count,
            gc_ref_fields,
            base_type_id,
            vtable: None,
        }
    }

    pub fn field_is_gc_ref(&self, index: usize) -> bool {
        self.gc_ref_fields.get(index).copied().unwrap_or(false)
    }
}

#[derive(Debug, Default)]
pub struct RuntimeTypeRegistry {
    by_id: HashMap<u32, RuntimeTypeInfo>,
    by_name: HashMap<String, u32>,
}

impl RuntimeTypeRegistry {
    pub fn register(&mut self, info: RuntimeTypeInfo) {
        self.by_name.insert(info.class_name.clone(), info.type_id);
        self.by_id.insert(info.type_id, info);
    }

    pub fn by_id(&self, type_id: u32) -> Option<&RuntimeTypeInfo> {
        self.by_id.get(&type_id)
    }

    pub fn by_name(&self, name: &str) -> Option<&RuntimeTypeInfo> {
        self.by_name
            .get(name)
            .and_then(|type_id| self.by_id.get(type_id))
    }

    pub fn is_subtype(&self, child_type_id: u32, parent_type_id: u32) -> bool {
        if child_type_id == parent_type_id {
            return true;
        }

        let mut current = self.by_id(child_type_id);
        while let Some(info) = current {
            let Some(base_type_id) = info.base_type_id else {
                return false;
            };
            if base_type_id == parent_type_id {
                return true;
            }
            current = self.by_id(base_type_id);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_header_records_type_and_base_ids() {
        let header = RuntimeObjectHeader::new(2, 3, Some(1));
        assert_eq!(header.type_id, 2);
        assert_eq!(header.field_count, 3);
        assert!(header.has_base());
    }

    #[test]
    fn type_info_tracks_gc_reference_fields() {
        let info = RuntimeTypeInfo::new(1, "Node", 2, vec![false, true], None);
        assert!(!info.field_is_gc_ref(0));
        assert!(info.field_is_gc_ref(1));
        assert!(!info.field_is_gc_ref(2));
    }

    #[test]
    fn registry_resolves_inheritance_chain() {
        let mut registry = RuntimeTypeRegistry::default();
        registry.register(RuntimeTypeInfo::new(1, "Animal", 0, vec![], None));
        registry.register(RuntimeTypeInfo::new(2, "Dog", 0, vec![], Some(1)));
        registry.register(RuntimeTypeInfo::new(3, "Puppy", 0, vec![], Some(2)));

        assert!(registry.is_subtype(3, 1));
        assert!(registry.is_subtype(2, 1));
        assert!(!registry.is_subtype(1, 2));
        assert_eq!(registry.by_name("Dog").unwrap().type_id, 2);
    }
}
