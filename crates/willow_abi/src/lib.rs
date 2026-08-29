//! Target-independent contracts shared by the Willow compiler and runtime.
//!
//! Layouts are expressed in pointer-sized words, fixed-width discriminants,
//! and semantic slot kinds. A consumer supplies its target pointer width only
//! when converting a descriptor to byte offsets or allocation sizes. This
//! keeps cross-compilation independent of the host Rust process layout.

/// Maximum number of payload words represented by the inline GC reference mask.
pub const GC_REF_MASK_BITS: usize = u64::BITS as usize;

/// Target-independent representation class for one C-ABI parameter/return.
///
/// `Word` is deliberately distinct from both `I64` and `Ptr`: it is a
/// Willow one-word value whose bits may hold a GC handle, a tagged value, or a
/// scalar payload.  It remains a 64-bit Willow word even when a consumer is
/// inspecting another target's native pointer width.  `Ptr` is reserved
/// for addresses dereferenced by the runtime (including function pointers and
/// out-parameters).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AbiTy {
    Word,
    I64,
    I32,
    I8,
    F64,
    Ptr,
}

/// Scheduler, GC, and recoverable-panic effects attached to a runtime ABI row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeEffects(u8);

impl RuntimeEffects {
    pub const NONE: Self = Self(0);
    pub const MAY_ALLOCATE: Self = Self(1 << 0);
    pub const MAY_BLOCK: Self = Self(1 << 1);
    pub const MAY_SUSPEND: Self = Self(1 << 2);
    pub const MAY_PREEMPT: Self = Self(1 << 3);
    pub const NO_PREEMPT_REGION: Self = Self(1 << 4);
    pub const MAY_PANIC: Self = Self(1 << 5);

    /// Every effect at once. The fail-closed default for a fact the compiler
    /// cannot see: an unanalyzed callee is assumed to do everything.
    pub const ALL: Self = Self((1 << 6) - 1);

    /// Number of distinct effect bits, i.e. the exclusive upper bound of the
    /// bit indices [`RuntimeEffects::bits`] can set.
    pub const BIT_COUNT: u32 = 6;

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Effects present in both sets. Used to mask what a call edge transmits:
    /// an eager `async` call, for instance, passes allocation to its caller but
    /// not suspension.
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Effects in `self` that are not in `other`.
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    pub const fn contains(self, effect: Self) -> bool {
        self.0 & effect.0 == effect.0
    }

    /// True when any effect in `effect` is present. Distinct from
    /// [`RuntimeEffects::contains`], which requires all of them.
    pub const fn intersects(self, effect: Self) -> bool {
        self.0 & effect.0 != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The raw bit set, so an effect-indexed side table can be keyed by bit.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The single effect at bit index `index`, or [`RuntimeEffects::NONE`] when
    /// the index is outside [`RuntimeEffects::BIT_COUNT`].
    pub const fn from_bit(index: u32) -> Self {
        if index >= Self::BIT_COUNT {
            return Self::NONE;
        }
        Self(1 << index)
    }
}

/// Result returned by every scheduler poll entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum RuntimePollResult {
    Pending = 0,
    Ready = 1,
    Yield = 2,
    Preempted = 3,
    Panicked = 4,
    BlockedSyscall = 5,
}

impl RuntimePollResult {
    pub const fn as_i32(self) -> i32 {
        self as i32
    }
}

/// Terminal code stored in the low bits of an async frame status word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i64)]
pub enum FrameTerminalStatus {
    Pending = 0,
    Completed = 1,
    Cancelled = 2,
    Panicked = 3,
}

pub mod frame_status {
    pub const TERMINAL_MASK: i64 = 0b111;
    pub const CANCEL_REQUESTED: i64 = 1 << 8;
}

/// Status returned by scheduler-aware lock acquisition and polling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum LockAcquireStatus {
    Cancelled = -3,
    Lost = -2,
    Recursive = -1,
    Pending = 0,
    Acquired = 1,
}

impl LockAcquireStatus {
    pub const fn as_i32(self) -> i32 {
        self as i32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum LockStatusPhase {
    Acquire = 0,
    Poll = 1,
}

/// Cross-ABI object-shape categories used to derive opaque layout ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u64)]
pub enum GcObjectKind {
    Class = 1,
    Enum = 2,
    InterfaceBox = 3,
    Range = 4,
    AsyncFrame = 5,
    ArrayHandle = 6,
    ArrayBuffer = 7,
    Map = 8,
    String = 9,
    Channel = 10,
    AtomicCell = 11,
    LockHandle = 12,
}

/// Destination category supplied to the structural write-barrier hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i64)]
pub enum GcStoreDestination {
    ObjectField = 1,
    ArrayElement = 2,
    MapValue = 3,
    EnumPayload = 4,
    InterfaceObject = 5,
    AsyncFrameSlot = 6,
    IndirectReference = 7,
    GlobalStatic = 8,
    ContainerInternal = 9,
    AsyncMutexCell = 10,
    AsyncRwLockCell = 11,
}

/// Derive the opaque layout fingerprint used by generated and native objects.
pub const fn gc_layout_id(
    kind: GcObjectKind,
    payload_size: i64,
    runtime_type_id: i64,
    gc_ref_mask: u64,
) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let words = [
        kind as u64,
        payload_size as u64,
        runtime_type_id as u64,
        gc_ref_mask,
    ];
    let mut index = 0;
    while index < words.len() {
        hash ^= words[index];
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }
    if hash == 0 { 1 } else { hash }
}

/// GC header ABI. Offsets are fixed-width and independent of Rust field lookup.
pub mod gc_header {
    pub const MARKED_OFFSET: u32 = 0;
    pub const ALLOCATED_OFFSET: u32 = 1;
    pub const GENERATION_OFFSET: u32 = 2;
    pub const AGE_OFFSET: u32 = 3;
    pub const TYPE_ID_OFFSET: u32 = 4;
    pub const LAYOUT_ID_OFFSET: u32 = 8;
    pub const REF_MASK_OFFSET: u32 = 16;
    pub const SIZE_OFFSET: u32 = 24;

    pub const fn next_offset(pointer_bytes: u32) -> u32 {
        SIZE_OFFSET + pointer_bytes
    }

    pub const fn size(pointer_bytes: u32) -> u32 {
        SIZE_OFFSET + 2 * pointer_bytes
    }
}

/// Generated-code-facing thread-local allocation-buffer state ABI.
pub mod tlab {
    pub const CURSOR_OFFSET: u32 = 0;
    pub const LIMIT_OFFSET: u32 = 8;
    pub const FAST_ALLOCATIONS_OFFSET: u32 = 16;
    pub const FAST_BYTES_OFFSET: u32 = 24;
    pub const STATE_SIZE: u32 = 32;
    pub const MAX_OBJECT_SIZE: u32 = 4 * 1024;
}

/// Fixed async-frame header ABI.
pub mod async_frame {
    pub const HEADER_WORDS: u32 = 3;
    pub const STATE_WORD: u32 = 0;
    pub const SLOT_COUNT_WORD: u32 = 1;
    pub const STATUS_WORD: u32 = 2;

    pub const fn header_bytes(pointer_bytes: u32) -> u32 {
        HEADER_WORDS * pointer_bytes
    }

    pub const fn data_slot_offset(slot: u32, pointer_bytes: u32) -> u32 {
        (HEADER_WORDS + slot) * pointer_bytes
    }
}

/// Representation of one target word in a runtime-owned aggregate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlotKind {
    /// Scalar, tag, task id, or other non-reference Willow word.
    Word,
    /// GC-managed Willow reference. It contributes a bit to the GC mask.
    GcRef,
    /// Native address or function pointer. It is never traced as a Willow ref.
    NativePtr,
}

/// A target-independent payload descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WordLayout<'a> {
    pub slots: &'a [SlotKind],
}

impl<'a> WordLayout<'a> {
    pub const fn new(slots: &'a [SlotKind]) -> Self {
        Self { slots }
    }

    pub const fn word_count(self) -> u32 {
        self.slots.len() as u32
    }

    pub const fn byte_size(self, pointer_bytes: u32) -> u32 {
        self.word_count() * pointer_bytes
    }

    pub const fn gc_ref_mask(self, first_payload_word: u32) -> u64 {
        let mut mask = 0u64;
        let mut index = 0usize;
        while index < self.slots.len() {
            if matches!(self.slots[index], SlotKind::GcRef) {
                let bit = first_payload_word as usize + index;
                assert!(bit < GC_REF_MASK_BITS, "layout exceeds inline GC mask");
                mask |= 1u64 << bit;
            }
            index += 1;
        }
        mask
    }
}

/// Physical layout of one instantiated enum variant.
///
/// Boxed Willow enums store a tag word followed by the variant payload. The
/// descriptor is instantiated from the semantic payload types, so
/// `Result<i64, E>` and `Result<String, E>` do not share an incorrect mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnumVariantLayout<'a> {
    pub tag: u32,
    pub payload: WordLayout<'a>,
}

impl<'a> EnumVariantLayout<'a> {
    pub const TAG_WORDS: u32 = 1;

    pub const fn new(tag: u32, payload: &'a [SlotKind]) -> Self {
        Self {
            tag,
            payload: WordLayout::new(payload),
        }
    }

    pub const fn payload_word_offset(self) -> u32 {
        Self::TAG_WORDS
    }

    pub const fn payload_byte_offset(self, pointer_bytes: u32) -> u32 {
        self.payload_word_offset() * pointer_bytes
    }

    pub const fn payload_bytes(self, pointer_bytes: u32) -> u32 {
        (Self::TAG_WORDS + self.payload.word_count()) * pointer_bytes
    }

    pub const fn gc_ref_mask(self) -> u64 {
        self.payload.gc_ref_mask(Self::TAG_WORDS)
    }
}

/// Layout of a runtime-created async frame's data slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFrameLayout<'a> {
    pub slots: WordLayout<'a>,
}

impl<'a> NativeFrameLayout<'a> {
    pub const fn new(slots: &'a [SlotKind]) -> Self {
        Self {
            slots: WordLayout::new(slots),
        }
    }

    pub const fn slot_count(self) -> u32 {
        self.slots.word_count()
    }

    pub const fn payload_bytes(self, pointer_bytes: u32) -> u32 {
        async_frame::header_bytes(pointer_bytes) + self.slots.byte_size(pointer_bytes)
    }

    pub const fn gc_ref_mask(self) -> u64 {
        self.slots.gc_ref_mask(async_frame::HEADER_WORDS)
    }

    /// GC mask in the data-slot numbering accepted by
    /// `willow_async_frame_alloc` (slot 0 is bit 0).
    pub const fn data_gc_ref_mask(self) -> u64 {
        self.slots.gc_ref_mask(0)
    }

    pub const fn slot_offset(self, slot: u32, pointer_bytes: u32) -> u32 {
        assert!(slot < self.slot_count(), "native frame slot out of bounds");
        async_frame::data_slot_offset(slot, pointer_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_layout_is_instantiated_from_payload_reference_shape() {
        const SCALAR: EnumVariantLayout<'_> = EnumVariantLayout::new(0, &[SlotKind::Word]);
        const STRING: EnumVariantLayout<'_> = EnumVariantLayout::new(0, &[SlotKind::GcRef]);
        assert_eq!(SCALAR.payload_bytes(8), 16);
        assert_eq!(STRING.payload_bytes(8), 16);
        assert_eq!(SCALAR.gc_ref_mask(), 0);
        assert_eq!(STRING.gc_ref_mask(), 0b10);
    }

    #[test]
    fn native_frame_layout_includes_header_and_shifts_reference_mask() {
        const LAYOUT: NativeFrameLayout<'_> =
            NativeFrameLayout::new(&[SlotKind::Word, SlotKind::NativePtr, SlotKind::GcRef]);
        assert_eq!(LAYOUT.slot_count(), 3);
        assert_eq!(LAYOUT.payload_bytes(8), 48);
        assert_eq!(LAYOUT.slot_offset(2, 8), 40);
        assert_eq!(LAYOUT.gc_ref_mask(), 1 << 5);
    }

    #[test]
    fn target_pointer_width_is_supplied_by_the_consumer() {
        const LAYOUT: NativeFrameLayout<'_> =
            NativeFrameLayout::new(&[SlotKind::Word, SlotKind::GcRef]);
        assert_eq!(LAYOUT.payload_bytes(4), 20);
        assert_eq!(LAYOUT.payload_bytes(8), 40);
        assert_eq!(LAYOUT.gc_ref_mask(), 1 << 4);
    }

    #[test]
    fn shared_discriminants_are_stable() {
        assert_eq!(GcObjectKind::Class as u64, 1);
        assert_eq!(GcObjectKind::LockHandle as u64, 12);
        assert_eq!(GcStoreDestination::ObjectField as i64, 1);
        assert_eq!(GcStoreDestination::AsyncRwLockCell as i64, 11);
        assert_eq!(RuntimePollResult::Pending as i32, 0);
        assert_eq!(RuntimePollResult::BlockedSyscall as i32, 5);
        assert_eq!(FrameTerminalStatus::Cancelled as i64, 2);
        assert_eq!(frame_status::CANCEL_REQUESTED, 1 << 8);
        assert_eq!(LockAcquireStatus::Cancelled as i32, -3);
        assert_eq!(LockAcquireStatus::Acquired as i32, 1);
        assert_eq!(LockStatusPhase::Poll as i32, 1);
    }
}
