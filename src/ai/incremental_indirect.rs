//! Persistent address-taken candidate buckets, updated from typed owner outputs.
use super::*;
use crate::semantic::indirect_lock::IndirectInputs;
use std::{collections::HashSet, sync::Arc};
type Bucket = (u64, Option<String>);
type Target = (UnitId, Option<FunctionId>);
type Caller = (UnitId, FunctionId);
type Counts<K> = im::HashMap<K, usize>;
#[derive(Clone, Default)]
pub(super) struct State {
    owners: im::HashMap<BodyId, (UnitId, Arc<IndirectInputs>)>,
    values: im::HashMap<Bucket, Counts<Target>>,
    arity_keys: im::HashMap<u64, im::HashSet<Option<String>>>,
    calls: im::HashMap<Bucket, Counts<Caller>>,
    arity_calls: im::HashMap<u64, Counts<Caller>>,
    caller_keys: im::HashMap<Caller, Counts<Bucket>>,
    active_ids: Counts<String>,
    typed_calls: Counts<u64>,
}
fn adjust<K: Eq + std::hash::Hash + Clone>(counts: &mut Counts<K>, key: K, add: bool) {
    if add {
        *counts.entry(key).or_default() += 1;
    } else if let Some(value) = counts.get_mut(&key) {
        if *value <= 1 {
            counts.remove(&key);
        } else {
            *value -= 1;
        }
    }
}
impl State {
    pub(super) fn seed(&mut self, captures: &HashMap<UnitId, CapturedUnit>) {
        for (&unit, capture) in captures {
            for (target, bit, key) in &capture.indirect.values {
                self.value(unit, *target, *bit, key.clone(), true);
            }
            for call in &capture.indirect.calls {
                self.call(unit, call, true);
            }
        }
    }
    pub(super) fn remember(&mut self, unit: UnitId, body: BodyId, inputs: IndirectInputs) {
        self.owners.insert(body, (unit, Arc::new(inputs)));
    }
    fn value(
        &mut self,
        unit: UnitId,
        target: Option<FunctionId>,
        bit: u64,
        key: Option<String>,
        add: bool,
    ) {
        let bucket = (bit, key);
        adjust(
            self.values.entry(bucket.clone()).or_default(),
            (unit, target),
            add,
        );
        if self
            .values
            .get(&bucket)
            .is_some_and(|counts| counts.is_empty())
        {
            self.values.remove(&bucket);
        }
        if self.values.contains_key(&bucket) {
            self.arity_keys
                .entry(bit)
                .or_default()
                .insert(bucket.1.clone());
        } else if let Some(keys) = self.arity_keys.get_mut(&bit) {
            keys.remove(&bucket.1);
            if keys.is_empty() {
                self.arity_keys.remove(&bit);
            }
        }
    }
    fn call(
        &mut self,
        unit: UnitId,
        call: &crate::semantic::indirect_lock::IndirectCall,
        add: bool,
    ) {
        let bucket = (call.bit, call.key.clone());
        let caller = (unit, call.caller);
        let slot = call.bit.trailing_zeros() as usize;
        let id = call.key.as_ref().map_or_else(
            || indirect::bucket_id(slot),
            |key| indirect::typed_bucket_id(slot, key),
        );
        adjust(&mut self.active_ids, id, add);
        if call.key.is_some() {
            adjust(&mut self.typed_calls, call.bit, add);
        }
        adjust(self.calls.entry(bucket.clone()).or_default(), caller, add);
        if self
            .calls
            .get(&bucket)
            .is_some_and(|counts| counts.is_empty())
        {
            self.calls.remove(&bucket);
        }
        adjust(self.arity_calls.entry(call.bit).or_default(), caller, add);
        if self
            .arity_calls
            .get(&call.bit)
            .is_some_and(|counts| counts.is_empty())
        {
            self.arity_calls.remove(&call.bit);
        }
        adjust(self.caller_keys.entry(caller).or_default(), bucket, add);
        if self
            .caller_keys
            .get(&caller)
            .is_some_and(|counts| counts.is_empty())
        {
            self.caller_keys.remove(&caller);
        }
    }
    pub(super) fn replace(
        &mut self,
        unit: UnitId,
        body: BodyId,
        inputs: Option<IndirectInputs>,
    ) -> HashSet<Caller> {
        if let Some(inputs) = &inputs
            && let Some((old_unit, old)) = self.owners.get(&body)
            && *old_unit == unit
            && old
                .values
                .iter()
                .map(|value| (value.target, value.bit, &value.key))
                .eq(inputs
                    .values
                    .iter()
                    .map(|value| (value.target, value.bit, &value.key)))
            && old
                .calls
                .iter()
                .map(|call| (call.caller, call.bit, &call.key))
                .eq(inputs
                    .calls
                    .iter()
                    .map(|call| (call.caller, call.bit, &call.key)))
        {
            self.remember(unit, body, inputs.clone());
            return HashSet::new();
        }
        let mut presence = HashMap::new();
        if let Some((old_unit, old)) = self.owners.get(&body) {
            for value in &old.values {
                let bucket = (value.bit, value.key.clone());
                let target = (*old_unit, value.target);
                presence.entry((bucket.clone(), target)).or_insert_with(|| {
                    self.values
                        .get(&bucket)
                        .is_some_and(|targets| targets.contains_key(&target))
                });
            }
        }
        if let Some(inputs) = &inputs {
            for value in &inputs.values {
                let bucket = (value.bit, value.key.clone());
                let target = (unit, value.target);
                presence.entry((bucket.clone(), target)).or_insert_with(|| {
                    self.values
                        .get(&bucket)
                        .is_some_and(|targets| targets.contains_key(&target))
                });
            }
        }
        if let Some((old_unit, old)) = self.owners.remove(&body) {
            for value in &old.values {
                self.value(old_unit, value.target, value.bit, value.key.clone(), false);
            }
            for call in &old.calls {
                self.call(old_unit, call, false);
            }
        }
        if let Some(inputs) = inputs {
            for value in &inputs.values {
                self.value(unit, value.target, value.bit, value.key.clone(), true);
            }
            for call in &inputs.calls {
                self.call(unit, call, true);
            }
            self.remember(unit, body, inputs);
        }
        let buckets: HashSet<_> = presence
            .into_iter()
            .filter_map(|((bucket, target), before)| {
                (before
                    != self
                        .values
                        .get(&bucket)
                        .is_some_and(|targets| targets.contains_key(&target)))
                .then_some(bucket)
            })
            .collect();
        let mut callers = HashSet::new();
        for (bit, key) in buckets {
            if key.is_none() {
                callers.extend(
                    self.arity_calls
                        .get(&bit)
                        .into_iter()
                        .flat_map(|counts| counts.keys())
                        .copied(),
                );
            } else {
                callers.extend(
                    self.calls
                        .get(&(bit, key))
                        .into_iter()
                        .flat_map(|counts| counts.keys())
                        .copied(),
                );
                callers.extend(
                    self.calls
                        .get(&(bit, None))
                        .into_iter()
                        .flat_map(|counts| counts.keys())
                        .copied(),
                );
            }
        }
        callers
    }
    pub(super) fn active(&self, id: &str) -> bool {
        if self.active_ids.contains_key(id) {
            return true;
        }
        let Some(slot) = id
            .strip_prefix("$indirect-arity:")
            .and_then(|suffix| suffix.strip_suffix(":*"))
            .and_then(|slot| slot.parse::<u32>().ok())
        else {
            return false;
        };
        let bit = 1u64 << slot;
        self.values
            .get(&(bit, None))
            .is_some_and(|values| !values.is_empty())
            && self.typed_calls.contains_key(&bit)
    }
    pub(super) fn inputs(&self, callers: &HashSet<Caller>) -> HashMap<UnitId, indirect::Inputs> {
        let mut result = HashMap::<UnitId, indirect::Inputs>::new();
        let mut requested = HashSet::new();
        for caller in callers {
            for (bit, key) in self
                .caller_keys
                .get(caller)
                .into_iter()
                .flat_map(|keys| keys.keys())
            {
                result.entry(caller.0).or_default().calls.push(
                    crate::semantic::indirect_lock::IndirectCall {
                        caller: caller.1,
                        bit: *bit,
                        key: key.clone(),
                    },
                );
                requested.insert((*bit, key.clone()));
            }
        }
        let mut buckets = HashSet::new();
        for (bit, key) in requested {
            if key.is_none() {
                for key in self
                    .arity_keys
                    .get(&bit)
                    .into_iter()
                    .flat_map(|keys| keys.iter())
                {
                    buckets.insert((bit, key.clone()));
                }
            } else {
                buckets.insert((bit, key));
                buckets.insert((bit, None));
            }
        }
        let mut values = HashSet::new();
        for (bit, key) in buckets {
            for target in self
                .values
                .get(&(bit, key.clone()))
                .into_iter()
                .flat_map(|targets| targets.keys())
            {
                values.insert((*target, bit, key.clone()));
            }
        }
        for ((unit, target), bit, key) in values {
            result
                .entry(unit)
                .or_default()
                .values
                .push((target, bit, key));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_indirect_reference_counts_remove_last_candidate_and_call() {
        let unit = UnitId::ENTRY;
        let first = BodyId::fresh();
        let second = BodyId::fresh();
        let caller_body = BodyId::fresh();
        let caller = FunctionId::free("caller");
        let values = IndirectInputs {
            values: vec![crate::semantic::indirect_lock::FunctionValue {
                target: None,
                bit: 1,
                span: Span::new(0, 1, 1, 1),
                key: None,
            }],
            ..Default::default()
        };
        let calls = IndirectInputs {
            calls: vec![crate::semantic::indirect_lock::IndirectCall {
                caller,
                bit: 1,
                key: Some("fn()->void".into()),
            }],
            ..Default::default()
        };
        let mut state = State::default();
        state.replace(unit, caller_body, Some(calls));
        state.replace(unit, first, Some(values.clone()));
        assert!(state.replace(unit, second, Some(values.clone())).is_empty());
        assert!(state.active(&indirect::wild_bucket_id(0)));
        assert!(state.replace(unit, first, Some(values)).is_empty());
        assert!(state.replace(unit, first, None).is_empty());
        assert!(state.active(&indirect::wild_bucket_id(0)));
        let affected = state.replace(unit, second, None);
        assert!(affected.contains(&(unit, caller)));
        assert!(!state.active(&indirect::wild_bucket_id(0)));
        assert!(state.active(&indirect::typed_bucket_id(0, "fn()->void")));
        state.replace(unit, caller_body, None);
        assert!(
            state.owners.is_empty()
                && state.values.is_empty()
                && state.arity_keys.is_empty()
                && state.calls.is_empty()
                && state.arity_calls.is_empty()
                && state.caller_keys.is_empty()
                && state.active_ids.is_empty()
                && state.typed_calls.is_empty()
        );
    }
    #[test]
    fn retained_indirect_duplicate_candidates_do_not_visit_unrelated_callers() {
        for count in [8, 32, 128] {
            let unit = UnitId::ENTRY;
            let first = BodyId::fresh();
            let duplicate = BodyId::fresh();
            let mut state = State::default();
            for i in 0..count {
                state.replace(
                    unit,
                    BodyId::fresh(),
                    Some(IndirectInputs {
                        calls: vec![crate::semantic::indirect_lock::IndirectCall {
                            caller: FunctionId::free(format!("caller_{i}")),
                            bit: 1,
                            key: Some("fn()->void".into()),
                        }],
                        ..Default::default()
                    }),
                );
            }
            let values = IndirectInputs {
                values: vec![crate::semantic::indirect_lock::FunctionValue {
                    target: None,
                    bit: 1,
                    span: Span::new(0, 1, 1, 1),
                    key: None,
                }],
                ..Default::default()
            };
            assert_eq!(
                state.replace(unit, first, Some(values.clone())).len(),
                count
            );
            assert!(state.replace(unit, duplicate, Some(values)).is_empty());
            assert!(state.replace(unit, first, None).is_empty());
            assert_eq!(state.replace(unit, duplicate, None).len(), count);
        }
    }
}
