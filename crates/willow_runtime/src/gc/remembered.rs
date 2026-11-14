//! Remembered membership plus per-card ownership counts. Removing a swept
//! owner updates its card in constant expected time, without a final full scan
//! or accidentally clearing the card of a surviving neighbor.
//!
//! Large reference-array buffers are remembered per slot card instead of as a
//! whole owner: their header `remembered` flag stays clear, so every young
//! store into them reaches the barrier and records only the written card. A
//! minor collection then scans the recorded cards rather than the complete
//! buffer, keeping its work proportional to the slots written since the last
//! minor instead of the retained array length.
use super::*;
use std::ops::Deref;
use std::sync::atomic::AtomicU64;

/// Slots per remembered card of a reference-array buffer.
pub(crate) const ARRAY_CARD_SLOTS: usize = GC_CARD_SIZE / GC_STORAGE_WORD_BYTES;
const _: () = assert!(GC_CARD_SIZE == willow_abi::GC_ARRAY_CARD_BYTES);
/// Reference-array buffers with at least this many element slots use card
/// remembering. Smaller buffers keep whole-owner remembering: their full scan
/// is bounded by this constant, and the header flag lets stores skip the call.
pub(super) const ARRAY_CARD_MIN_SLOTS: usize = 4 * ARRAY_CARD_SLOTS;

/// Element capacity of `object` when it is a reference-array buffer remembered
/// per card, else `None`.
pub(super) fn card_array_capacity(object: HeapObject) -> Option<usize> {
    let metadata = object.trace_metadata();
    if metadata.type_id != willow_abi::runtime_type_ids::ARRAY_REF_TYPE_ID
        || metadata.gc_ref_mask != 0
    {
        return None;
    }
    let slots = (metadata.payload_size / GC_STORAGE_WORD_BYTES)
        .checked_sub(willow_abi::array_layout::BUFFER_HEADER_WORDS)?;
    (slots >= ARRAY_CARD_MIN_SLOTS).then_some(slots)
}

/// Card of a slot address inside the card-remembered buffer `owner`.
pub(super) fn array_slot_card(owner: usize, slot: usize) -> usize {
    let first = owner + willow_abi::array_layout::BUFFER_HEADER_WORDS * GC_STORAGE_WORD_BYTES;
    (slot - first) / GC_STORAGE_WORD_BYTES / ARRAY_CARD_SLOTS
}

/// Advanced whenever recorded cards may be forgotten: a minor takes the set,
/// a reset clears it, a card-remembered owner becomes whole-remembered, or a
/// sweep removes a card-remembered owner (whose address may then be reused). Barrier dedup caches are valid only within
/// one value. Minors and resets run stopped; a swept owner is unreachable, so
/// no mutator can store into its address until after reallocation, which
/// follows this release increment.
///
/// The exported symbol is the process-default runtime's epoch. Every other
/// runtime owns its own cell, so one runtime's minors never invalidate another
/// runtime's barrier caches.
#[unsafe(export_name = "willow_gc_card_epoch")]
static CARD_EPOCH: AtomicU64 = AtomicU64::new(1);

/// The process-default runtime's card epoch: the exported ABI data symbol.
#[inline]
pub(crate) const fn default_card_epoch() -> &'static AtomicU64 {
    &CARD_EPOCH
}

/// One runtime's card-cache epoch, shared by its collector state and its
/// lock-free barrier readers.
#[derive(Clone, Debug)]
pub(crate) enum CardEpoch {
    DefaultAbi,
    Owned(std::sync::Arc<AtomicU64>),
}

/// Bits below each runtime's epoch space. A thread's card cache and context
/// proofs are keyed by `(owner, card, epoch)` and survive switching the served
/// runtime, so epochs of different runtimes must never compare equal even when
/// a released owner address is reused by another runtime's heap.
const CARD_EPOCH_SPACE_BITS: u32 = 40;

impl Default for CardEpoch {
    fn default() -> Self {
        // Space 0 is the default runtime's exported epoch.
        static NEXT_SPACE: AtomicU64 = AtomicU64::new(1);
        let space = NEXT_SPACE.fetch_add(1, Ordering::Relaxed);
        assert!(
            space < 1 << (u64::BITS - CARD_EPOCH_SPACE_BITS),
            "card epoch spaces exhausted"
        );
        Self::Owned(std::sync::Arc::new(AtomicU64::new(
            (space << CARD_EPOCH_SPACE_BITS) | 1,
        )))
    }
}

impl CardEpoch {
    #[inline]
    pub(crate) fn cell(&self) -> &AtomicU64 {
        match self {
            Self::DefaultAbi => &CARD_EPOCH,
            Self::Owned(epoch) => epoch,
        }
    }
}

/// The epoch of the runtime this thread serves; card caches are thread-local
/// and filled only for that runtime.
pub(super) fn card_epoch() -> u64 {
    crate::runtime::current()
        .card_epoch()
        .load(Ordering::Acquire)
}

#[derive(Debug, Default)]
pub(super) struct RememberedSet {
    epoch: CardEpoch,
    owners: IdSet<usize>,
    cards: IdMap<usize, usize>,
    /// Card-remembered owners (also in `owners`) and their recorded cards.
    /// Their header flag is clear. Owners absent here are scanned whole.
    array_cards: IdMap<usize, IdSet<usize>>,
}
impl RememberedSet {
    pub(super) fn with_epoch(epoch: CardEpoch) -> Self {
        Self {
            epoch,
            owners: IdSet::default(),
            cards: IdMap::default(),
            array_cards: IdMap::default(),
        }
    }
    fn advance_card_epoch(&self) {
        self.epoch.cell().fetch_add(1, Ordering::AcqRel);
    }
    pub(super) fn insert(&mut self, owner: usize) -> bool {
        // Whole-owner remembering subsumes any recorded cards.
        let had_cards = self.array_cards.remove(&owner).is_some();
        if had_cards {
            self.advance_card_epoch();
        }
        if !self.owners.insert(owner) {
            return had_cards;
        }
        *self.cards.entry(owner / GC_CARD_SIZE).or_default() += 1;
        true
    }
    /// Record one slot card of a card-remembered owner. Returns whether the
    /// owner entry is new. A whole-remembered owner already covers the card.
    pub(super) fn insert_card(&mut self, owner: usize, card: usize) -> bool {
        if self.owners.contains(&owner) {
            if let Some(cards) = self.array_cards.get_mut(&owner) {
                cards.insert(card);
            }
            return false;
        }
        self.owners.insert(owner);
        *self.cards.entry(owner / GC_CARD_SIZE).or_default() += 1;
        self.array_cards.entry(owner).or_default().insert(card);
        true
    }
    pub(super) fn remove(&mut self, owner: &usize) -> bool {
        if !self.owners.remove(owner) {
            return false;
        }
        if self.array_cards.remove(owner).is_some() {
            self.advance_card_epoch();
        }
        let card = owner / GC_CARD_SIZE;
        let count = self.cards.get_mut(&card).expect("remembered card count");
        *count -= 1;
        if *count == 0 {
            self.cards.remove(&card);
        }
        true
    }
    pub(super) fn contains_card(&self, card: usize) -> bool {
        self.cards.contains_key(&card)
    }
    /// Number of members whose header flag must be set. Card owners are
    /// also members, but deliberately leave their header flag clear.
    pub(super) fn whole_owner_count(&self) -> usize {
        self.owners.len() - self.array_cards.len()
    }
    /// Recorded slot cards of a card-remembered owner.
    pub(super) fn owner_cards(&self, owner: usize) -> Option<&IdSet<usize>> {
        self.array_cards.get(&owner)
    }
    pub(super) fn clear(&mut self) {
        self.owners.clear();
        self.cards.clear();
        self.array_cards.clear();
        self.advance_card_epoch();
    }
    /// Move the contents out for a minor collection, invalidating barrier
    /// dedup caches of the recorded cards.
    pub(super) fn take(&mut self) -> Self {
        self.advance_card_epoch();
        Self {
            epoch: self.epoch.clone(),
            owners: std::mem::take(&mut self.owners),
            cards: std::mem::take(&mut self.cards),
            array_cards: std::mem::take(&mut self.array_cards),
        }
    }
    /// Consume the set as whole owners and card-remembered owners.
    pub(super) fn into_parts(self) -> (Vec<usize>, Vec<(usize, Vec<usize>)>) {
        let Self {
            owners,
            array_cards,
            ..
        } = self;
        let whole = owners
            .into_iter()
            .filter(|owner| !array_cards.contains_key(owner))
            .collect();
        let carded = array_cards
            .into_iter()
            .map(|(owner, cards)| (owner, cards.into_iter().collect()))
            .collect();
        (whole, carded)
    }
}
impl Deref for RememberedSet {
    type Target = IdSet<usize>;
    fn deref(&self) -> &Self::Target {
        &self.owners
    }
}
#[cfg(test)]
impl PartialEq<HashSet<usize>> for RememberedSet {
    fn eq(&self, other: &HashSet<usize>) -> bool {
        self.owners.len() == other.len() && self.owners.iter().all(|owner| other.contains(owner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sweep_card_counts_preserve_shared_cards_and_ignore_duplicate_owners() {
        for owners in [1usize, 64, 1024, 8192] {
            let mut set = RememberedSet::default();
            for index in 0..owners {
                assert!(set.insert(index * 8));
                assert!(!set.insert(index * 8));
            }
            let cards = (owners * 8).div_ceil(GC_CARD_SIZE);
            assert_eq!(set.cards.len(), cards);
            for index in (0..owners).rev() {
                assert!(set.remove(&(index * 8)));
                assert!(!set.remove(&(index * 8)));
                let card = index * 8 / GC_CARD_SIZE;
                assert_eq!(set.contains_card(card), index * 8 % GC_CARD_SIZE != 0);
            }
            assert!(set.is_empty());
            assert!(set.cards.is_empty());
            println!(
                "remembered_count owners={owners} cards={cards} insert_calls={} remove_calls={} membership_checks={owners}",
                2 * owners,
                2 * owners
            );
        }
    }
    #[test]
    fn card_owners_split_from_whole_owners_and_invalidate_barrier_caches() {
        // The epoch is global and only ever grows; other tests may advance it
        // concurrently, so only advancement is asserted.
        let mut set = RememberedSet::default();
        let owner = 0x1000usize;
        assert!(set.insert_card(owner, 3), "new owner");
        assert!(!set.insert_card(owner, 3));
        assert!(!set.insert_card(owner, 7), "known owner, new card");
        assert!(set.contains(&owner));
        assert_eq!(set.owner_cards(owner).map(|cards| cards.len()), Some(2));

        let before = set.epoch.cell().load(Ordering::Acquire);
        let taken = set.take();
        assert!(
            set.epoch.cell().load(Ordering::Acquire) > before,
            "taking the set invalidates caches"
        );
        assert!(set.is_empty() && set.owner_cards(owner).is_none());
        let (whole, mut carded) = taken.into_parts();
        assert!(whole.is_empty());
        carded[0].1.sort_unstable();
        assert_eq!(carded, vec![(owner, vec![3, 7])]);

        // Whole-owner remembering supersedes cards and drops them.
        assert!(set.insert_card(owner, 1));
        let before = set.epoch.cell().load(Ordering::Acquire);
        assert!(set.insert(owner), "conversion must set the header flag");
        assert!(!set.insert(owner));
        assert!(set.epoch.cell().load(Ordering::Acquire) > before);
        assert!(set.owner_cards(owner).is_none());
        assert!(!set.insert_card(owner, 2), "whole owner ignores cards");
        assert!(set.owner_cards(owner).is_none());
        let (whole, carded) = set.take().into_parts();
        assert_eq!(whole, vec![owner]);
        assert!(carded.is_empty());

        // Sweeping a card owner invalidates caches for a reused address.
        assert!(set.insert_card(owner, 5));
        let before = set.epoch.cell().load(Ordering::Acquire);
        assert!(set.remove(&owner));
        assert!(set.epoch.cell().load(Ordering::Acquire) > before);
        assert!(set.is_empty() && set.cards.is_empty());

        assert!(set.insert_card(owner, 5));
        let before = set.epoch.cell().load(Ordering::Acquire);
        set.clear();
        assert!(set.epoch.cell().load(Ordering::Acquire) > before);
        assert!(set.owner_cards(owner).is_none());
    }
}
