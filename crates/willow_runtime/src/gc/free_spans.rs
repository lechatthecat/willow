//! Sweep-built, address-ordered first-fit index. Consumed holes retain an empty
//! leaf until the next sweep, so allocation never moves the remaining holes.

use super::RegionFreeSpan;

#[derive(Debug, Default)]
pub(super) struct FreeSpans {
    spans: Vec<Option<RegionFreeSpan>>,
    maxima: Vec<usize>,
    leaves: usize,
    #[cfg(test)]
    node_visits: usize,
    #[cfg(test)]
    build_visits: usize,
}

impl From<Vec<RegionFreeSpan>> for FreeSpans {
    fn from(spans: Vec<RegionFreeSpan>) -> Self {
        if spans.is_empty() {
            return Self::default();
        }
        let leaves = spans.len().next_power_of_two();
        let mut maxima = vec![0; 2 * leaves];
        #[cfg(test)]
        let mut build_visits = 0;
        for (index, span) in spans.iter().enumerate() {
            maxima[leaves + index] = span.size;
            #[cfg(test)]
            {
                build_visits += 1;
            }
        }
        for index in (1..leaves).rev() {
            maxima[index] = maxima[2 * index].max(maxima[2 * index + 1]);
            #[cfg(test)]
            {
                build_visits += 1;
            }
        }
        Self {
            spans: spans.into_iter().map(Some).collect(),
            maxima,
            leaves,
            #[cfg(test)]
            node_visits: 0,
            #[cfg(test)]
            build_visits,
        }
    }
}

impl FreeSpans {
    pub(super) fn largest(&self) -> usize {
        self.maxima.get(1).copied().unwrap_or(0)
    }

    pub(super) fn take(&mut self, size: usize) -> Option<usize> {
        debug_assert!(size > 0);
        #[cfg(test)]
        {
            self.node_visits += 1;
        }
        if self.largest() < size {
            return None;
        }
        let mut node = 1;
        while node < self.leaves {
            #[cfg(test)]
            {
                self.node_visits += 1;
            }
            node *= 2;
            if self.maxima[node] < size {
                node += 1;
            }
        }
        let slot = &mut self.spans[node - self.leaves];
        let span = slot.as_mut().expect("fitting leaf owns a free span");
        let offset = span.offset;
        span.offset += size;
        span.size -= size;
        self.maxima[node] = span.size;
        if span.size == 0 {
            *slot = None;
        }
        while node > 1 {
            node /= 2;
            #[cfg(test)]
            {
                self.node_visits += 1;
            }
            self.maxima[node] = self.maxima[2 * node].max(self.maxima[2 * node + 1]);
        }
        Some(offset)
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &RegionFreeSpan> {
        self.into_iter()
    }

    pub(super) fn is_consistent(&self) -> bool {
        if self.spans.is_empty() {
            return self.leaves == 0 && self.maxima.is_empty();
        }
        self.leaves == self.spans.len().next_power_of_two()
            && self.maxima.len() == 2 * self.leaves
            && (0..self.leaves).all(|index| {
                self.maxima[self.leaves + index]
                    == self
                        .spans
                        .get(index)
                        .and_then(|span| *span)
                        .map_or(0, |span| span.size)
            })
            && (1..self.leaves).all(|index| {
                self.maxima[index] == self.maxima[2 * index].max(self.maxima[2 * index + 1])
            })
    }
}

impl<'a> IntoIterator for &'a FreeSpans {
    type Item = &'a RegionFreeSpan;
    type IntoIter = std::iter::Flatten<std::slice::Iter<'a, Option<RegionFreeSpan>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.spans.iter().flatten()
    }
}

// Single-object release and metadata corruption are test-only operations.
// Production reconstructs the index once from the sweep's ordered gaps.
#[cfg(test)]
impl FreeSpans {
    pub(super) fn len(&self) -> usize {
        self.iter().count()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }

    pub(super) fn push(&mut self, span: RegionFreeSpan) {
        *self = Self::from(self.iter().copied().chain([span]).collect::<Vec<_>>());
    }
}

#[cfg(test)]
impl PartialEq<Vec<RegionFreeSpan>> for FreeSpans {
    fn eq(&self, other: &Vec<RegionFreeSpan>) -> bool {
        self.iter().eq(other.iter())
    }
}

#[cfg(test)]
impl std::ops::Index<usize> for FreeSpans {
    type Output = RegionFreeSpan;

    fn index(&self, index: usize) -> &Self::Output {
        self.iter().nth(index).expect("free span index in bounds")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_take(spans: &mut Vec<RegionFreeSpan>, size: usize) -> Option<usize> {
        let index = spans.iter().position(|span| span.size >= size)?;
        let span = &mut spans[index];
        let offset = span.offset;
        span.offset += size;
        span.size -= size;
        if span.size == 0 {
            spans.remove(index);
        }
        Some(offset)
    }

    #[test]
    fn first_fit_matches_linear_reference_with_mixed_sizes_and_splits() {
        for holes in [0, 1, 3, 31, 64, 129, 1024] {
            let mut reference: Vec<_> = (0..holes)
                .map(|i| RegionFreeSpan {
                    offset: i * 2048,
                    size: 8 * (1 + (i * 37) % 127),
                })
                .collect();
            let mut indexed = FreeSpans::from(reference.clone());
            for i in 0..4 * holes + 1 {
                let size = 8 * (1 + (i * 53) % 129);
                assert_eq!(indexed.take(size), reference_take(&mut reference, size));
                assert_eq!(indexed, reference);
                assert!(indexed.is_consistent());
                assert_eq!(
                    indexed.largest(),
                    reference.iter().map(|span| span.size).max().unwrap_or(0)
                );
            }
            while !reference.is_empty() {
                assert_eq!(indexed.take(8), reference_take(&mut reference, 8));
            }
            assert!(indexed.is_empty());
            assert_eq!(indexed.largest(), 0);
            assert_eq!(indexed.take(8), None);
        }
    }

    #[test]
    fn verifier_rejects_stale_internal_maximum() {
        let mut index = FreeSpans::from(vec![RegionFreeSpan { offset: 0, size: 8 }; 4]);
        assert!(index.is_consistent());
        index.maxima[2] = 0;
        assert!(!index.is_consistent());
    }

    #[test]
    fn fragmented_reuse_has_logarithmic_visits_and_linear_rebuild() {
        for holes in [32usize, 64, 128, 256, 512, 1024, 2048] {
            // Small holes precede every fitting hole: a linear search cannot
            // cheaply stop early, and every large hole requires four splits.
            let spans: Vec<_> = (0..holes)
                .map(|i| RegionFreeSpan {
                    offset: i * 512,
                    size: if i % 2 == 0 { 8 } else { 256 },
                })
                .collect();
            let mut index = FreeSpans::from(spans);
            let requests = 2 * holes;
            assert_eq!(index.build_visits, 2 * holes - 1);
            for i in 0..requests {
                assert_eq!(index.take(64), Some((2 * (i / 4) + 1) * 512 + (i % 4) * 64));
            }
            let expected = requests * (1 + 2 * holes.ilog2() as usize);
            assert_eq!(index.node_visits, expected);
            assert_eq!(index.take(64), None);
            assert_eq!(index.node_visits, expected + 1);
            assert_eq!(index.largest(), 8);
            // The old allocator inspected every remaining hole, including all
            // small holes, on each allocation to recompute the maximum.
            let linear_visits = (0..requests).map(|i| holes - i / 4).sum::<usize>();
            eprintln!(
                "free_spans holes={holes} requests={requests} old_scan_visits={linear_visits} index_visits={expected} build_visits={}",
                index.build_visits
            );
        }
    }
}
