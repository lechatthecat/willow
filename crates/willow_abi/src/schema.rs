//! One entry point for the compiler/runtime ABI contract. Function rows retain
//! their mandatory effect classifications; native type rows retain mandatory
//! layout, trace and resource-ownership classifications. Existing public
//! constants remain compatibility views of these same tables.
use crate::{RuntimeSymbol, runtime_type_ids::RuntimeType};

pub struct RuntimeAbiSchema {
    pub symbols: &'static [RuntimeSymbol],
    pub types: &'static [RuntimeType],
    pub layouts: &'static [SharedLayout],
}

pub const RUNTIME_ABI: RuntimeAbiSchema = RuntimeAbiSchema {
    symbols: crate::RUNTIME_SYMBOLS,
    types: crate::runtime_type_ids::RUNTIME_TYPES,
    layouts: &[
        SharedLayout::ArrayHandle,
        SharedLayout::ArrayBufferHeader,
        SharedLayout::AsyncFrameHeader,
        SharedLayout::GcHeader,
        SharedLayout::GcDescriptor,
        SharedLayout::InterfaceBox,
        SharedLayout::Tlab,
    ],
};

/// Fixed portions of aggregates accessed across the compiler/runtime boundary.
/// Variable class, enum and frame tails continue to use WordLayout,
/// EnumVariantLayout and NativeFrameLayout. Opaque native payloads are described
/// in the type table, never assigned portable Rust field offsets here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SharedLayout {
    ArrayHandle,
    ArrayBufferHeader,
    AsyncFrameHeader,
    GcHeader,
    GcDescriptor,
    InterfaceBox,
    Tlab,
}

impl SharedLayout {
    pub const fn size(self, pointer_bytes: u32) -> u32 {
        use crate::{
            array_layout as a, async_frame as f, dispatch_layout as d, gc_header as g, tlab,
        };
        match self {
            Self::ArrayHandle => a::HANDLE_WORDS as u32 * a::WORD_BYTES as u32,
            Self::ArrayBufferHeader => a::BUFFER_HEADER_WORDS as u32 * a::WORD_BYTES as u32,
            Self::AsyncFrameHeader => f::header_bytes(pointer_bytes),
            Self::GcHeader => g::size(pointer_bytes),
            Self::GcDescriptor => 4 * 8,
            Self::InterfaceBox => d::interface_bytes(pointer_bytes),
            Self::Tlab => tlab::state_size(pointer_bytes),
        }
    }

    /// Field names in physical order. Unnamed padding is excluded.
    pub const fn fields(self) -> &'static [&'static str] {
        match self {
            Self::ArrayHandle => &["len", "cap", "is_ref", "buf"],
            Self::ArrayBufferHeader => &["len"],
            Self::AsyncFrameHeader => &["state", "slot_count", "status"],
            Self::GcHeader => &[
                "marked",
                "allocated",
                "generation",
                "age",
                "owned",
                "remembered",
                "descriptor",
            ],
            Self::GcDescriptor => &["type_id", "layout_id", "gc_ref_mask", "size"],
            Self::InterfaceBox => &["object", "vtable"],
            Self::Tlab => &["cursor", "limit", "start_bits"],
        }
    }

    pub const fn field_offset(self, field: usize, pointer_bytes: u32) -> Option<u32> {
        use crate::{
            array_layout as a, async_frame as f, dispatch_layout as d, gc_header as g, tlab,
        };
        if field >= self.fields().len() {
            return None;
        }
        Some(match self {
            Self::ArrayHandle => {
                a::handle_offset([a::H_LEN, a::H_CAP, a::H_IS_REF, a::H_BUF][field]) as u32
            }
            Self::ArrayBufferHeader => 0,
            Self::AsyncFrameHeader => f::header_word_offset(
                [f::STATE_WORD, f::SLOT_COUNT_WORD, f::STATUS_WORD][field],
                pointer_bytes,
            ),
            Self::GcHeader => [
                g::MARKED_OFFSET,
                g::ALLOCATED_OFFSET,
                g::GENERATION_OFFSET,
                g::AGE_OFFSET,
                g::OWNED_OFFSET,
                g::REMEMBERED_OFFSET,
                g::DESCRIPTOR_OFFSET,
            ][field],
            Self::GcDescriptor => field as u32 * 8,
            Self::InterfaceBox => [d::OBJECT_OFFSET, d::vtable_offset(pointer_bytes)][field],
            Self::Tlab => [
                tlab::CURSOR_OFFSET,
                tlab::limit_offset(pointer_bytes),
                tlab::start_bits_offset(pointer_bytes),
            ][field],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_type_ids::*;

    #[test]
    fn unified_schema_keeps_every_existing_row_and_effect() {
        assert!(std::ptr::eq(RUNTIME_ABI.symbols, crate::RUNTIME_SYMBOLS));
        assert_eq!(RUNTIME_ABI.types.len(), RUNTIME_TYPE_IDS.len());
        for (row, &(name, id)) in RUNTIME_ABI.types.iter().zip(RUNTIME_TYPE_IDS) {
            assert_eq!((row.name, row.type_id), (name, id));
            assert_eq!(runtime_type(id), Some(*row));
        }
        for id in [
            NO_TYPE_ID,
            GENERATED_TYPE_ID_MIN,
            GENERATED_TYPE_ID_MAX,
            u32::MAX,
        ] {
            assert_eq!(runtime_type(id), None);
        }
    }

    #[test]
    fn hook_classifications_reject_missing_and_extra_callbacks() {
        for row in RUNTIME_ABI.types {
            for trace in [false, true] {
                for finalizer in [false, true] {
                    let expected = match row.type_id {
                        ARRAY_REF_TYPE_ID => trace && !finalizer,
                        CANCELLATION_TOKEN_TYPE_ID | NETWORK_HANDLE_TYPE_ID => !trace && finalizer,
                        GC_BITMAP_TYPE_ID => false,
                        _ => trace && finalizer,
                    };
                    assert_eq!(
                        row.accepts_native_hooks(trace, finalizer),
                        expected,
                        "{}",
                        row.name
                    );
                }
            }
            assert_eq!(
                row.layout,
                match row.type_id {
                    ARRAY_REF_TYPE_ID => TypeLayout::ReferenceArrayBuffer,
                    GC_BITMAP_TYPE_ID => TypeLayout::BitmapPayload,
                    _ => TypeLayout::OpaqueNative,
                }
            );
        }
    }

    #[test]
    fn shared_layout_fields_are_ordered_bounded_and_width_aware() {
        for width in [4, 8] {
            for &layout in RUNTIME_ABI.layouts {
                let mut previous = None;
                for field in 0..layout.fields().len() {
                    let offset = layout.field_offset(field, width).unwrap();
                    assert!(offset < layout.size(width));
                    assert!(previous.is_none_or(|p| p < offset));
                    previous = Some(offset);
                }
                assert_eq!(layout.field_offset(layout.fields().len(), width), None);
                assert_eq!(layout.field_offset(usize::MAX, width), None);
            }
            assert_eq!(SharedLayout::ArrayHandle.size(width), 32);
            assert_eq!(SharedLayout::ArrayBufferHeader.size(width), 8);
            assert_eq!(SharedLayout::AsyncFrameHeader.size(width), 24);
            assert_eq!(SharedLayout::GcHeader.size(width), 8 + width);
            assert_eq!(SharedLayout::GcDescriptor.size(width), 32);
            assert_eq!(SharedLayout::InterfaceBox.size(width), 2 * width);
        }
    }
}
