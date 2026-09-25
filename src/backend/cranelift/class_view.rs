//! Alias-aware backend reads of the session's frozen class and interface
//! layouts.
//!
//! The backend used to mirror every completed layout, base edge, runtime id,
//! slot table and interface composition into per-`Codegen` maps. A
//! [`ClassView`] answers the same questions straight from
//! [`LayoutQueries`], resolving the unit's aliases through its [`TypeScope`]
//! the way the old maps did, so a class is stored once per session and every
//! `Codegen` sharing the queries sees one truth.

use std::sync::Arc;

use super::type_index::{TypeLookup, TypeScope};
use super::vtable_layout::IfaceShapes;
use super::{InterfaceInfo, Type, TypeId};
use crate::compiler_db::layout::{LayoutQueries, ObjectLayout, TargetLayoutKey};
use crate::semantic::method_slots::MethodSlots;

/// What LIR generation asks about classes. Production answers through a
/// [`ClassView`]; `lir_gen` tests answer from hand-built tables.
pub(super) trait ClassTables {
    /// The completed field layout of `class` (alias-resolved), parent first.
    fn class_fields(&self, class: &TypeId) -> Option<Arc<Vec<(String, Type)>>>;
    fn is_class(&self, class: &TypeId) -> bool {
        self.class_fields(class).is_some()
    }
    /// The runtime type id of `class` (alias-resolved).
    fn class_type_id(&self, class: &TypeId) -> Option<i64>;
    /// The direct base of `class` (alias-resolved), as registered.
    fn class_base(&self, class: &TypeId) -> Option<TypeId>;
}

#[derive(Clone, Copy)]
pub(super) struct ClassView<'a> {
    scope: &'a TypeScope,
    layouts: &'a LayoutQueries,
}

impl<'a> ClassView<'a> {
    pub(super) fn new(scope: &'a TypeScope, layouts: &'a LayoutQueries) -> Self {
        Self { scope, layouts }
    }

    /// The declaration identity `name` refers to in this unit.
    pub(super) fn resolve<Q: TypeLookup + ?Sized>(&self, name: &Q) -> TypeId {
        self.scope.resolve(&name.type_id())
    }

    pub(super) fn fields<Q: TypeLookup + ?Sized>(
        &self,
        name: &Q,
    ) -> Option<Arc<Vec<(String, Type)>>> {
        self.layouts.fields(self.resolve(name))
    }

    /// Whether `name` is a class with a completed layout.
    pub(super) fn is_class<Q: TypeLookup + ?Sized>(&self, name: &Q) -> bool {
        self.layouts.has_layout(self.resolve(name))
    }

    pub(super) fn slots<Q: TypeLookup + ?Sized>(&self, name: &Q) -> Option<Arc<MethodSlots>> {
        self.layouts.slots(self.resolve(name))
    }

    pub(super) fn base<Q: TypeLookup + ?Sized>(&self, name: &Q) -> Option<TypeId> {
        self.layouts.base(self.resolve(name))
    }

    pub(super) fn type_id<Q: TypeLookup + ?Sized>(&self, name: &Q) -> Option<i64> {
        self.layouts.runtime_id(self.resolve(name))
    }

    /// [`Self::base`] of a DECLARATION identity, ignoring whatever aliases
    /// this unit has installed over it (willow-kd1v): for build-wide walks
    /// over names the tables themselves recorded.
    pub(super) fn base_canonical(&self, id: &TypeId) -> Option<TypeId> {
        self.layouts.base(*id)
    }

    /// [`Self::type_id`] of a DECLARATION identity, ignoring aliases.
    pub(super) fn type_id_canonical(&self, id: &TypeId) -> Option<i64> {
        self.layouts.runtime_id(*id)
    }

    /// Every class with a runtime type id, in no particular order.
    pub(super) fn runtime_classes(&self) -> Vec<(TypeId, i64)> {
        self.layouts.runtime_classes()
    }

    pub(super) fn interface<Q: TypeLookup + ?Sized>(&self, name: &Q) -> Option<Arc<InterfaceInfo>> {
        self.layouts.interface(self.resolve(name))
    }

    pub(super) fn is_interface<Q: TypeLookup + ?Sized>(&self, name: &Q) -> bool {
        self.layouts.has_interface(self.resolve(name))
    }

    /// The byte layout of class `name` for a `pointer_bytes`-wide target.
    pub(super) fn object_layout<Q: TypeLookup + ?Sized>(
        &self,
        name: &Q,
        pointer_bytes: u32,
    ) -> Option<Arc<ObjectLayout>> {
        self.layouts
            .object_layout(TargetLayoutKey::new(self.resolve(name), pointer_bytes))
    }
}

impl ClassTables for ClassView<'_> {
    fn class_fields(&self, class: &TypeId) -> Option<Arc<Vec<(String, Type)>>> {
        self.fields(class)
    }
    fn is_class(&self, class: &TypeId) -> bool {
        ClassView::is_class(self, class)
    }
    fn class_type_id(&self, class: &TypeId) -> Option<i64> {
        self.type_id(class)
    }
    fn class_base(&self, class: &TypeId) -> Option<TypeId> {
        self.base(class)
    }
}

impl IfaceShapes for ClassView<'_> {
    fn canonical(&self, iface: &TypeId) -> TypeId {
        self.interface(iface)
            .map(|info| info.name)
            .unwrap_or_else(|| *iface)
    }
    fn supers(&self, iface: &TypeId) -> Vec<TypeId> {
        self.interface(iface)
            .map(|info| info.extends.clone())
            .unwrap_or_default()
    }
    fn method_slot(&self, iface: &TypeId, method: &str) -> Option<usize> {
        self.interface(iface)?.method_order.slot_of(method)
    }
    fn method_count(&self, iface: &TypeId) -> usize {
        self.interface(iface)
            .map_or(0, |info| info.method_order.len())
    }
    fn methods(&self, iface: &TypeId) -> Vec<String> {
        self.interface(iface)
            .map(|info| info.method_order.as_slice().to_vec())
            .unwrap_or_default()
    }
}
