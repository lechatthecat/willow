pub type GcRootSlot = usize;

pub trait GcTrace {
    fn trace(&self, visitor: &mut GcVisitor);
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GcVisitor {
    roots: Vec<GcRootSlot>,
}

impl GcVisitor {
    pub fn mark_root(&mut self, root: GcRootSlot) {
        if root != 0 {
            self.roots.push(root);
        }
    }

    pub fn roots(&self) -> &[GcRootSlot] {
        &self.roots
    }

    pub fn into_roots(self) -> Vec<GcRootSlot> {
        self.roots
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcRootSet {
    slots: Vec<GcRootSlot>,
}

impl GcRootSet {
    pub fn push(&mut self, slot: GcRootSlot) {
        self.slots.push(slot);
    }

    pub fn pop(&mut self) -> Option<GcRootSlot> {
        self.slots.pop()
    }

    pub fn slots(&self) -> &[GcRootSlot] {
        &self.slots
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

impl GcTrace for GcRootSet {
    fn trace(&self, visitor: &mut GcVisitor) {
        for &slot in &self.slots {
            visitor.mark_root(slot);
        }
    }
}

impl<T: GcTrace> GcTrace for Option<T> {
    fn trace(&self, visitor: &mut GcVisitor) {
        if let Some(value) = self {
            value.trace(visitor);
        }
    }
}

impl<T: GcTrace> GcTrace for Vec<T> {
    fn trace(&self, visitor: &mut GcVisitor) {
        for value in self {
            value.trace(visitor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestRoot(GcRootSlot);

    impl GcTrace for TestRoot {
        fn trace(&self, visitor: &mut GcVisitor) {
            visitor.mark_root(self.0);
        }
    }

    #[test]
    fn visitor_skips_null_roots() {
        let mut visitor = GcVisitor::default();
        visitor.mark_root(0);
        visitor.mark_root(42);
        assert_eq!(visitor.roots(), &[42]);
    }

    #[test]
    fn root_set_reports_all_non_null_roots() {
        let mut roots = GcRootSet::default();
        roots.push(10);
        roots.push(0);
        roots.push(20);

        let mut visitor = GcVisitor::default();
        roots.trace(&mut visitor);

        assert_eq!(visitor.roots(), &[10, 20]);
    }

    #[test]
    fn option_and_vec_trace_nested_values() {
        let values = vec![TestRoot(1), TestRoot(2)];
        let maybe = Some(TestRoot(3));
        let mut visitor = GcVisitor::default();

        values.trace(&mut visitor);
        maybe.trace(&mut visitor);

        assert_eq!(visitor.roots(), &[1, 2, 3]);
    }
}
