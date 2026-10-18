//! Remembered membership plus per-card ownership counts. Removing a swept
//! owner updates its card in constant expected time, without a final full scan
//! or accidentally clearing the card of a surviving neighbor.
use super::*;
use std::ops::Deref;

#[derive(Debug, Default)]
pub(super) struct RememberedSet {
    owners: IdSet<usize>,
    cards: IdMap<usize, usize>,
}
impl RememberedSet {
    pub(super) fn insert(&mut self, owner: usize) -> bool {
        if !self.owners.insert(owner) {
            return false;
        }
        *self.cards.entry(owner / GC_CARD_SIZE).or_default() += 1;
        true
    }
    pub(super) fn remove(&mut self, owner: &usize) -> bool {
        if !self.owners.remove(owner) {
            return false;
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
    pub(super) fn clear(&mut self) {
        self.owners.clear();
        self.cards.clear();
    }
}
impl Deref for RememberedSet {
    type Target = IdSet<usize>;
    fn deref(&self) -> &Self::Target {
        &self.owners
    }
}
impl IntoIterator for RememberedSet {
    type Item = usize;
    type IntoIter = std::collections::hash_set::IntoIter<usize>;
    fn into_iter(self) -> Self::IntoIter {
        self.owners.into_iter()
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
}
