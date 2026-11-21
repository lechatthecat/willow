//! GC-managed hash map `Map<K, V>`.
//!
//! Scalar word maps store a compact, atomically guarded table in their non-moving
//! payload. Confined string-key maps with scalar values use checked local
//! storage; other maps store a `Mutex<MapData>` for concurrent GC tracing.
//! Two GC hooks keep it correct:
//!
//! * a trace function reports reference-typed *values* so they stay alive while
//!   the map is reachable (keys are copied out of the Willow heap, so they need
//!   no tracing — see [`MapKey`]);
//! * a finalizer drops the inline state when the map is swept, releasing the
//!   hash table and its owned string keys.
//!
//! Keys are one 64-bit word — an `i64`, the bits of an `f64`, or a `bool` — or
//! a `String` compared by content. Values are stored as raw 64-bit words; `.get`
//! returns a Willow `Option<V>` built directly here.

use crate::gc::{
    GcObjectKind, GcStoreDestination, willow_alloc_with_layout, write_barrier_object_owner,
};
use crate::native_memory::{Mutex, MutexGuard};
use crate::string::willow_string_as_bytes;
use std::cell::{RefCell, RefMut, UnsafeCell};
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU8, Ordering};

use willow_abi::runtime_type_ids::MAP_TYPE_ID;

/// A key copied out of the Willow heap so the map owns it independently of the
/// GC. String keys compare by content (not pointer identity), which is what
/// `Map<String, V>` lookups require. Every other admitted key is one word, so
/// `Word` holds an `i64`, a `bool` as 0/1, or canonical `f64` bits. Floating
/// keys reject NaN and normalize both signed zeros to +0.0, so admitted keys
/// have the same equality as the language's numerical `==`.
#[derive(PartialEq, Eq, Clone)]
enum MapKey {
    Word(i64),
    Str(String),
}

#[derive(Clone, Copy)]
struct MapLayout {
    key_kind: i64,
    value_kind: i64,
    value_is_ref: bool,
}

/// Scalar maps have no GC edges. Their storage nevertheless needs exclusion:
/// multiple language Mutex values may wrap the same Map, and original aliases
/// remain valid. The immutable layout/discriminant can be read concurrently.
struct MapStorage {
    layout: MapLayout,
    state: MapState,
}

enum MapState {
    Words(ScalarStorage),
    Traced(Mutex<MapData>),
    Local(crate::native_memory::Local<MapData>),
}

/// Local storage is selected only by the compiler's whole-function confinement
/// proof. RefCell checks local alias borrows without atomics or mutual exclusion.
/// This enum is deliberately NOT Sync; only the shared variant is synchronized.
enum ScalarStorage {
    Local(RefCell<WordMap>),
    Shared(WordStorage),
}
impl ScalarStorage {
    fn access(&self) -> ScalarGuard<'_> {
        match self {
            Self::Local(data) => {
                let no_preempt = crate::preempt::NoPreemptGuard::enter();
                ScalarGuard::Local(data.borrow_mut(), no_preempt)
            }
            Self::Shared(data) => ScalarGuard::Shared(data.lock()),
        }
    }
}
enum ScalarGuard<'a> {
    Local(RefMut<'a, WordMap>, crate::preempt::NoPreemptGuard),
    Shared(WordGuard<'a>),
}
impl Deref for ScalarGuard<'_> {
    type Target = WordMap;
    fn deref(&self) -> &WordMap {
        match self {
            Self::Local(data, _) => data,
            Self::Shared(data) => data,
        }
    }
}
impl DerefMut for ScalarGuard<'_> {
    fn deref_mut(&mut self) -> &mut WordMap {
        match self {
            Self::Local(data, _) => data,
            Self::Shared(data) => data,
        }
    }
}

/// One atomic access gate per shared map, independent of language-level locks.
/// Uncontended accesses use one acquire CAS and one release swap, without an
/// OS mutex or allocation. Contenders park on a condition variable. No GC allocation, safepoint, or user code is allowed
/// while guarded; task preemption is disabled before acquiring the gate.
#[derive(Default)]
struct WordStorage {
    // 0 = free, 1 = held without known waiters, 2 = held with possible waiters.
    state: AtomicU8,
    waiters: std::sync::Mutex<()>,
    wake: std::sync::Condvar,
    data: UnsafeCell<WordMap>,
}

// SAFETY: only WordGuard exposes data; its acquire/release gate serializes all
// reads and writes, including lookup, display and copy. GC skips scalar data.
unsafe impl Sync for WordStorage {}

impl WordStorage {
    fn lock(&self) -> WordGuard<'_> {
        crate::observation::record(crate::observation::Event::WordSharedAcquires, 1);
        let no_preempt = crate::preempt::NoPreemptGuard::enter();
        if self
            .state
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.wait();
        }
        WordGuard {
            storage: self,
            _no_preempt: no_preempt,
            _not_send: std::marker::PhantomData,
        }
    }

    #[cold]
    fn wait(&self) {
        let mut waiter = self.waiters.lock().unwrap();
        // Keep state=2 when acquiring via this path, so release wakes another
        // sleeper even if a fresh fast-path caller barged between wakeups.
        while self.state.swap(2, Ordering::Acquire) != 0 {
            waiter = self.wake.wait(waiter).unwrap();
        }
    }

    #[cold]
    fn wake_one(&self) {
        // Pair with the waiter's state check and atomic unlock-and-wait. This
        // prevents a release notification from racing ahead of the wait.
        let _waiter = self.waiters.lock().unwrap();
        self.wake.notify_one();
    }
}

struct WordGuard<'a> {
    storage: &'a WordStorage,
    _no_preempt: crate::preempt::NoPreemptGuard,
    // The preemption depth belongs to the acquiring OS thread.
    _not_send: std::marker::PhantomData<*mut ()>,
}
impl Deref for WordGuard<'_> {
    type Target = WordMap;
    fn deref(&self) -> &WordMap {
        // SAFETY: The live WordGuard holds the storage lock and releases it only after this borrow
        // ends.
        unsafe { &*self.storage.data.get() }
    }
}
impl DerefMut for WordGuard<'_> {
    fn deref_mut(&mut self) -> &mut WordMap {
        // SAFETY: The live WordGuard holds the storage lock and releases it only after this borrow
        // ends.
        unsafe { &mut *self.storage.data.get() }
    }
}
impl Drop for WordGuard<'_> {
    fn drop(&mut self) {
        // Release storage before the NoPreemptGuard field reenables preemption.
        if self.storage.state.swap(0, Ordering::Release) == 2 {
            self.storage.wake_one();
        }
    }
}

#[cfg(test)]
const STRING_HASH_BYTES: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::StringHashBytes);
#[cfg(test)]
const SNAPSHOT_COMPARISONS: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::SnapshotComparisons);
#[cfg(test)]
const COMPACT_MOVES: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::CompactMoves);
#[cfg(test)]
const WORD_HASH_CALLS: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::WordHashCalls);
#[cfg(test)]
const WORD_SHARED_ACQUIRES: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::WordSharedAcquires);

/// Fixed, non-cryptographic integer mixer. Both the bucket index and SwissTable
/// fingerprint see mixed bits, including for consecutive/strided integer keys.
#[derive(Default)]
struct WordHasher(u64);
impl Hasher for WordHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write_u64(&mut self, word: u64) {
        crate::observation::record(crate::observation::Event::WordHashCalls, 1);
        let mut x = word;
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        self.0 = x ^ (x >> 31);
    }
    fn write_i64(&mut self, word: i64) {
        self.write_u64(word as u64);
    }
    fn write(&mut self, bytes: &[u8]) {
        // Only i64 keys use this builder; keep the trait's byte entry point
        // deterministic as well without any dynamically dispatched key view.
        for &byte in bytes {
            self.write_u64(self.0 ^ u64::from(byte));
        }
    }
}

#[derive(Default)]
struct WordMap {
    entries: HashMap<i64, i64, BuildHasherDefault<WordHasher>>,
    charged: usize,
}
impl WordMap {
    fn reconcile(&mut self) {
        let bytes = crate::native_memory::hash_bytes::<i64, i64>(self.entries.capacity());
        if bytes > self.charged {
            crate::gc::charge_external(bytes - self.charged);
        } else if bytes < self.charged {
            crate::gc::release_external(self.charged - bytes);
        }
        self.charged = bytes;
    }
}
impl Drop for WordMap {
    fn drop(&mut self) {
        // Release the native storage before releasing its retained-byte charge.
        drop(std::mem::take(&mut self.entries));
        crate::gc::release_external(self.charged);
    }
}

/// The immutable discriminant/layout may be read by GC concurrently. Access to
/// Words contents is restricted to mutators holding the storage access gate.
///
/// # Safety
/// `map` must denote an initialized, aligned MapStorage from the map constructor.
/// Keep the payload live throughout the borrow and observe local-map confinement.
/// Finish accessing the guarded contents before a GC point.
unsafe fn storage<'a>(map: *mut u8) -> &'a MapStorage {
    // SAFETY: The map constructor allocates word-aligned GC storage sized for MapStorage. Its
    // layout selects the matching finalizer.
    unsafe { &*map.cast::<MapStorage>() }
}

fn word_key(word: i64, kind: i64) -> Option<i64> {
    if kind != 1 {
        return Some(word);
    }
    // Classify bits without floating-point arithmetic, preserving subnormals
    // and rejecting both quiet and signaling NaNs. Both zeros become +0.0.
    let magnitude = (word as u64) & 0x7fff_ffff_ffff_ffff;
    if magnitude > 0x7ff0_0000_0000_0000 {
        None
    } else {
        Some(if magnitude == 0 { 0 } else { word })
    }
}

struct MapData {
    /// Fixed at construction from Map<K, V>; inserts never change GC metadata.
    layout: MapLayout,
    /// Hash growth preserves value indices, permitting bounded GC slices.
    /// Deletion compaction republishes moved values through the GC barrier.
    // Randomized non-cryptographic foldhash; this is not a cryptographic or
    // adaptive HashDoS defense. Owned and borrowed keys use the same encoder.
    entries: hashbrown::HashMap<MapKey, usize>,
    values: Vec<i64>,
    free: Vec<usize>,
    /// Major cycles are serialized. Appends after the first slice are covered
    /// by insertion barriers and must not extend this epoch's finite scan.
    scan_limit: usize,
    key_bytes: usize,
    native_table_bytes: usize,
}

impl crate::native_memory::Footprint for MapData {
    fn native_bytes(&mut self) -> usize {
        crate::native_memory::sum_bytes([
            crate::native_memory::retain_capacity(
                &mut self.native_table_bytes,
                crate::native_memory::hash_bytes::<MapKey, usize>(self.entries.capacity()),
            ),
            self.values.capacity() * size_of::<i64>(),
            self.free.capacity() * size_of::<usize>(),
            self.key_bytes,
        ])
    }
}

impl MapData {
    /// Geometric compaction bounds scanning by live entries, rather than the
    /// lifetime insertion count. Relocated references are republished through
    /// the insertion barrier because a concurrent slice may have passed them.
    fn compact_after_remove(&mut self, owner: *mut u8) {
        if self.entries.len().saturating_mul(4) >= self.values.len() {
            return;
        }
        let mut values = Vec::with_capacity(self.entries.len());
        for index in self.entries.values_mut() {
            crate::observation::record(crate::observation::Event::CompactMoves, 1);
            let value = self.values[*index];
            if self.layout.value_is_ref {
                // SAFETY: The owner and non-null referent are live GC payloads with headers
                // at the addresses used by this filter.
                unsafe {
                    write_barrier_object_owner(
                        owner,
                        std::ptr::null_mut(),
                        value as *mut u8,
                        GcStoreDestination::MapValue as i64,
                    )
                };
            }
            *index = values.len();
            values.push(value);
        }
        self.values = values;
        self.free = Vec::new();
        self.entries.shrink_to_fit();
        self.scan_limit = self.scan_limit.min(self.values.len());
    }

    #[inline]
    fn get(&self, key: &KeyRef<'_>) -> Option<i64> {
        self.entries.get(key).map(|&index| self.values[index])
    }
}

/// The owned and borrowed keys share exactly the same hash/equality encoding.
#[derive(PartialEq, Eq, Clone, Copy)]
enum KeyRef<'a> {
    Word(i64),
    Str(&'a [u8]),
}

impl Hash for KeyRef<'_> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Word(word) => {
                word.hash(state);
            }
            Self::Str(text) => {
                crate::observation::record(crate::observation::Event::StringHashBytes, text.len());
                // MapKey is a standalone, private key, never a component of
                // a compound hash. The byte hasher incorporates its length;
                // equality still distinguishes key kinds and all content.
                state.write(text);
            }
        }
    }
}

impl MapKey {
    #[inline]
    fn key_ref(&self) -> KeyRef<'_> {
        match self {
            Self::Word(word) => KeyRef::Word(*word),
            Self::Str(text) => KeyRef::Str(text.as_bytes()),
        }
    }
}

impl Hash for MapKey {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key_ref().hash(state);
    }
}

impl hashbrown::Equivalent<MapKey> for KeyRef<'_> {
    #[inline]
    fn equivalent(&self, other: &MapKey) -> bool {
        *self == other.key_ref()
    }
}

/// Borrow a key only during the lookup; no Willow pointer is retained in the map.
///
/// # Safety
/// For a string key, zero denotes the empty string. Otherwise the key must be
/// a current WillowString whose bytes stay live and stationary throughout the
/// borrow. Finish using the borrowed key before a GC point.
/// Returns None for NaN; callers must release the map lock before raising.
unsafe fn key_from_word<'a>(word: i64, key_kind: i64) -> Option<KeyRef<'a>> {
    if key_kind == 3 {
        // SAFETY: The operand is null or a live string, and no safepoint intervenes in this
        // borrow.
        Some(KeyRef::Str(unsafe {
            willow_string_as_bytes(word as *const u8)
        }))
    } else {
        word_key(word, key_kind).map(KeyRef::Word)
    }
}

fn raise_nan_key() {
    crate::panic_context::raise_language_message("NaN cannot be used as a Map key");
}

/// Release the local borrow before reenabling task preemption.
enum MapGuard<'a> {
    Shared(MutexGuard<'a, MapData>),
    Local(
        crate::native_memory::LocalGuard<'a, MapData>,
        crate::preempt::NoPreemptGuard,
    ),
}
impl Deref for MapGuard<'_> {
    type Target = MapData;
    fn deref(&self) -> &MapData {
        match self {
            Self::Shared(data) => data,
            Self::Local(data, _) => data,
        }
    }
}
impl DerefMut for MapGuard<'_> {
    fn deref_mut(&mut self) -> &mut MapData {
        match self {
            Self::Shared(data) => data,
            Self::Local(data, _) => data,
        }
    }
}
/// Access inline MapData, keeping local borrows within a no-preempt region.
///
/// # Safety
/// `map` must be a live map payload with MapData (not Words) storage.
#[inline]
unsafe fn map_data<'a>(map: *mut u8) -> MapGuard<'a> {
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    match &unsafe { storage(map) }.state {
        MapState::Traced(data) => MapGuard::Shared(data.lock().unwrap()),
        MapState::Local(data) => {
            let guard = crate::preempt::NoPreemptGuard::enter();
            MapGuard::Local(data.access(), guard)
        }
        MapState::Words(_) => unreachable!("scalar maps have no traced storage"),
    }
}

/// Trace hook: report reference-typed values as GC children.
unsafe fn trace_map(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    if matches!(
        // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
        // constructor.
        unsafe { storage(payload) }.state,
        MapState::Words(_) | MapState::Local(_)
    ) {
        return;
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let mut data = unsafe { map_data(payload) };
    if data.layout.value_is_ref {
        for value in &mut data.values {
            slots.push((value as *mut i64).cast::<*mut u8>());
        }
    }
}

unsafe fn snapshot_map(payload: *mut u8, children: &mut Vec<*mut u8>) {
    if matches!(
        // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
        // constructor.
        unsafe { storage(payload) }.state,
        MapState::Words(_) | MapState::Local(_)
    ) {
        return;
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let data = unsafe { map_data(payload) };
    if data.layout.value_is_ref {
        children.extend(data.values.iter().map(|value| *value as *mut u8));
    }
}

unsafe fn snapshot_map_slice(
    payload: *mut u8,
    cursor: usize,
    limit: usize,
    children: &mut Vec<*mut u8>,
) -> crate::gc::TraceSliceProgress {
    use crate::gc::TraceSliceProgress;
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let MapState::Traced(mutex) = &unsafe { storage(payload) }.state else {
        return TraceSliceProgress::Done;
    };
    let mut data = match mutex.try_lock() {
        Ok(data) => data,
        Err(std::sync::TryLockError::WouldBlock) => return TraceSliceProgress::Retry,
        Err(std::sync::TryLockError::Poisoned(_)) => panic!("map trace found poisoned storage"),
    };
    if !data.layout.value_is_ref {
        return TraceSliceProgress::Done;
    }
    if cursor == 0 {
        data.scan_limit = data.values.len();
    }
    let end = cursor.saturating_add(limit).min(data.scan_limit);
    children.extend(
        data.values[cursor.min(end)..end]
            .iter()
            .map(|&value| value as *mut u8),
    );
    if end < data.scan_limit {
        TraceSliceProgress::Continue(end)
    } else {
        TraceSliceProgress::Done
    }
}

/// Finalizer hook: release the hash table and owned keys when the map is swept.
unsafe fn drop_map(payload: *mut u8) {
    // SAFETY: The map constructor allocates word-aligned GC storage sized for MapStorage. Its
    // layout selects the matching finalizer.
    unsafe { std::ptr::drop_in_place(payload.cast::<MapStorage>()) };
}

/// Register the map trace and finalizer. Called on every `willow_map_new`
/// (idempotent): `willow_gc_init` clears the type registry, so a process-global
/// `Once` would fail to re-register after the first reset (e.g. in multi-init
/// test runs). Real programs init once, so the repeated insert is harmless.
const MAP_REGISTRATION: crate::gc::NativeGcRegistration = crate::gc::NativeGcRegistration::Map;
const MAP_GC_TYPES: &[crate::gc::NativeGcType] = &[
    // SAFETY: The map constructor installs MapStorage for this ID. Stopped tracing visits
    // its root cells, and concurrent tracing reads synchronized entry snapshots in bounded
    // slices.
    unsafe {
        crate::gc::NativeGcType::new(MAP_TYPE_ID, Some(trace_map), Some(drop_map))
            .with_concurrent_trace(snapshot_map)
            .with_concurrent_slice(snapshot_map_slice)
    },
];

fn ensure_registered() {
    // SAFETY: This is the complete descriptor set for the MapStorage registration group and
    // its constructors.
    unsafe { MAP_REGISTRATION.ensure(MAP_GC_TYPES) };
}

/// Allocate an empty map with inline state. `gc_ref_mask` is zero because
/// reference values are traced through `trace_map`, not the native state words.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_map_new(key_kind: i64, value_kind: i64, value_is_ref: i64) -> *mut u8 {
    map_new(key_kind, value_kind, value_is_ref, false)
}

/// Compiler-only constructor for a map proven not to escape its synchronous
/// function.
///
/// # Safety
/// All aliases must remain in that invocation; never share this map
/// across threads, return it, capture it, or store it in another object.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_map_new_local(
    key_kind: i64,
    value_kind: i64,
    value_is_ref: i64,
) -> *mut u8 {
    map_new(key_kind, value_kind, value_is_ref, true)
}

fn map_new(key_kind: i64, value_kind: i64, value_is_ref: i64, confined: bool) -> *mut u8 {
    // Old-generation payloads never move and provide header alignment, which
    // must also satisfy the inline mutex on every supported target.
    const { assert!(align_of::<MapStorage>() <= align_of::<crate::gc::GcHeader>()) };
    ensure_registered();
    // SAFETY: The constructor selects the matching payload layout and initializes its fields
    // before exposing it or reaching a GC point.
    let map = unsafe {
        willow_alloc_with_layout(
            GcObjectKind::Map,
            MAP_TYPE_ID,
            size_of::<MapStorage>() as i64,
            0,
        )
    };
    if map.is_null() {
        return std::ptr::null_mut();
    }
    let layout = MapLayout {
        key_kind,
        value_kind,
        value_is_ref: value_is_ref != 0,
    };
    let state = if key_kind != 3 && !layout.value_is_ref {
        MapState::Words(if confined {
            ScalarStorage::Local(RefCell::new(WordMap::default()))
        } else {
            ScalarStorage::Shared(WordStorage::default())
        })
    } else {
        let data = MapData {
            layout,
            entries: hashbrown::HashMap::default(),
            values: Vec::new(),
            free: Vec::new(),
            scan_limit: 0,
            key_bytes: 0,
            native_table_bytes: 0,
        };
        if confined && !layout.value_is_ref {
            MapState::Local(crate::native_memory::Local::new(data))
        } else {
            MapState::Traced(Mutex::new(data))
        }
    };
    // No GC allocation occurs between reserving the payload and initializing it.
    // SAFETY: The map constructor allocates word-aligned GC storage sized for MapStorage. Its
    // layout selects the matching finalizer.
    unsafe {
        map.cast::<MapStorage>().write(MapStorage { layout, state });
    }

    map
}

/// Insert or update `key -> value`. `key_is_ref`/`val_is_ref` describe whether
/// the words are WillowString/GC pointers.
///
/// # Safety
/// A non-null `map` must be a live, aligned payload from a Willow map constructor,
/// kept reachable through the call. A local map must stay with its owning invocation.
/// The key flag and word must match the map layout. A string key must be null or
/// point to a live immutable WillowString through the lookup. The value flag and word must
/// match the declared value type, including a current GC payload or null for references.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_map_insert(
    map: *mut u8,
    key_word: i64,
    key_is_ref: i64,
    val_word: i64,
    val_is_ref: i64,
) {
    if map.is_null() {
        return;
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let storage = unsafe { storage(map) };
    if let MapState::Words(words) = &storage.state {
        debug_assert_eq!(key_is_ref, 0);
        debug_assert_eq!(val_is_ref, 0);
        let Some(key) = word_key(key_word, storage.layout.key_kind) else {
            raise_nan_key();
            return;
        };
        let mut words = words.access();
        words.entries.insert(key, val_word);
        words.reconcile();
        return;
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let mut data = unsafe { map_data(map) };
    debug_assert_eq!(data.layout.value_is_ref, val_is_ref != 0);
    debug_assert_eq!(data.layout.key_kind == 3, key_is_ref != 0);
    // SAFETY: The map's declared key kind determines whether the word contains a retained string
    // or a scalar.
    let Some(key) = (unsafe { key_from_word(key_word, data.layout.key_kind) }) else {
        drop(data);
        raise_nan_key();
        return;
    };
    // One borrowed probe for both insertion and replacement. Only vacancies
    // allocate owned key bytes; the entry retains its computed hash on growth.
    let is_ref = data.layout.value_is_ref;
    let MapData {
        entries,
        values,
        free,
        key_bytes,
        ..
    } = &mut *data;
    match entries.entry_ref(&key) {
        hashbrown::hash_map::EntryRef::Occupied(slot) => {
            let index = *slot.get();
            if is_ref {
                // SAFETY: The owner and non-null referent are live GC payloads with headers
                // at the addresses used by this filter.
                unsafe {
                    write_barrier_object_owner(
                        map,
                        values[index] as *mut u8,
                        val_word as *mut u8,
                        GcStoreDestination::MapValue as i64,
                    )
                };
            }
            values[index] = val_word;
        }
        hashbrown::hash_map::EntryRef::Vacant(slot) => {
            let owned_key = match key {
                KeyRef::Word(word) => MapKey::Word(word),
                KeyRef::Str(text) => {
                    let text = String::from_utf8(text.to_vec()).unwrap_or_else(|_| {
                        crate::failure::fatal_invariant("invalid UTF-8 in Map string key")
                    });
                    *key_bytes += text.capacity();
                    MapKey::Str(text)
                }
            };
            if is_ref {
                // SAFETY: The owner and non-null referent are live GC payloads with headers
                // at the addresses used by this filter.
                unsafe {
                    write_barrier_object_owner(
                        map,
                        std::ptr::null_mut(),
                        val_word as *mut u8,
                        GcStoreDestination::MapValue as i64,
                    )
                };
            }
            let index = if let Some(index) = free.pop() {
                values[index] = val_word;
                index
            } else {
                let index = values.len();
                values.push(val_word);
                index
            };
            slot.insert_with_key(owned_key, index);
        }
    }
}

/// Look up `key`, returning a Willow `Option<V>` (`Some(value)` or `None`).
/// `use_niche` is selected by the compiler's central `OptionRepr`: when set,
/// `V` is a guaranteed-non-null GC reference and the result is the value word
/// itself for `Some`, or zero for `None`. Other payloads use the boxed tagged
/// enum layout below.
///
/// # Safety
/// A non-null `map` must be a live, aligned payload from a Willow map constructor,
/// kept reachable through the call. A local map must stay with its owning invocation.
/// The key flag and word must match the map layout. A string key must be null or
/// point to a live immutable WillowString through the lookup. For a successful lookup, niche representation requires a non-null reference value type.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_map_get(
    map: *mut u8,
    key_word: i64,
    key_is_ref: i64,
    use_niche: i64,
) -> *mut u8 {
    if map.is_null() {
        return if use_niche != 0 {
            std::ptr::null_mut()
        } else {
            alloc_none()
        };
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let storage = unsafe { storage(map) };
    if let MapState::Words(words) = &storage.state {
        debug_assert_eq!(key_is_ref, 0);
        let Some(key) = word_key(key_word, storage.layout.key_kind) else {
            raise_nan_key();
            return std::ptr::null_mut();
        };
        let value = words.access().entries.get(&key).copied();
        return match value {
            Some(value) if use_niche != 0 => value as *mut u8,
            Some(value) => alloc_some(value, false),
            None if use_niche != 0 => std::ptr::null_mut(),
            None => alloc_none(),
        };
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let data = unsafe { map_data(map) };
    debug_assert_eq!(data.layout.key_kind == 3, key_is_ref != 0);
    // SAFETY: The map's declared key kind determines whether the word contains a retained string
    // or a scalar.
    let Some(key) = (unsafe { key_from_word(key_word, data.layout.key_kind) }) else {
        drop(data);
        raise_nan_key();
        return std::ptr::null_mut();
    };
    let value = data.get(&key);
    let is_ref = data.layout.value_is_ref;
    drop(data);
    match value {
        Some(v) if use_niche != 0 => v as *mut u8,
        Some(v) => alloc_some(v, is_ref),
        None if use_niche != 0 => std::ptr::null_mut(),
        None => alloc_none(),
    }
}

/// Look up a word without allocating an Option wrapper. Returns the Option tag
/// (zero for Some, one for None) and writes its payload to caller-owned storage.
///
/// `out_payload` must be valid, aligned writable storage for one `i64` for the
/// duration of this call. Missing keys and rejected NaN keys leave it zeroed.
///
/// # Safety
/// A non-null `map` must be a live, aligned payload from a Willow map constructor,
/// kept reachable through the call. A local map must stay with its owning invocation.
/// The key flag and word must match the map layout. A string key must be null or
/// point to a live immutable WillowString through the lookup.
/// Even for null maps, the caller must exclusively lend one aligned writable i64 at
/// `out_payload`, disjoint from the map and key.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_map_get_into(
    map: *mut u8,
    key_word: i64,
    key_is_ref: i64,
    out_payload: *mut i64,
) -> i64 {
    // SAFETY: The ABI caller provides a live, aligned, writable output slot for this result.
    unsafe { out_payload.write(0) };
    if map.is_null() {
        return 1;
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let storage = unsafe { storage(map) };
    if let MapState::Words(words) = &storage.state {
        debug_assert_eq!(key_is_ref, 0);
        let Some(key) = word_key(key_word, storage.layout.key_kind) else {
            raise_nan_key();
            return 1;
        };
        return match words.access().entries.get(&key) {
            Some(&value) => {
                // SAFETY: The ABI caller provides a live, aligned, writable output slot for this
                // result.
                unsafe { out_payload.write(value) };
                0
            }
            None => 1,
        };
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let data = unsafe { map_data(map) };
    debug_assert_eq!(data.layout.key_kind == 3, key_is_ref != 0);
    // SAFETY: The map's declared key kind determines whether the word contains a retained string
    // or a scalar.
    let Some(key) = (unsafe { key_from_word(key_word, data.layout.key_kind) }) else {
        drop(data);
        raise_nan_key();
        return 1;
    };
    let value = data.get(&key);
    drop(data);
    match value {
        Some(value) => {
            // SAFETY: The ABI caller provides a live, aligned, writable output slot for this
            // result.
            unsafe { out_payload.write(value) };
            0
        }
        None => 1,
    }
}

/// Remove a key, returning the Option tag and storage word without GC allocation.
/// Deleted reference slots are cleared with the collector's deletion barrier.
///
/// # Safety
/// A non-null `map` must be a live, aligned payload from a Willow map constructor,
/// kept reachable through the call. A local map must stay with its owning invocation.
/// The key flag and word must match the map layout. A string key must be null or
/// point to a live immutable WillowString through the lookup.
/// Even for null maps, the caller must exclusively lend one aligned writable i64 at
/// `out_payload`, disjoint from the map and key.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_map_remove_into(
    map: *mut u8,
    key_word: i64,
    key_is_ref: i64,
    out_payload: *mut i64,
) -> i64 {
    // SAFETY: The ABI caller provides a live, aligned, writable output slot for this result.
    unsafe { out_payload.write(0) };
    if map.is_null() {
        return 1;
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let storage = unsafe { storage(map) };
    let value = if let MapState::Words(words) = &storage.state {
        let Some(key) = word_key(key_word, storage.layout.key_kind) else {
            raise_nan_key();
            return 1;
        };
        let mut words = words.access();
        let value = words.entries.remove(&key);
        if words.entries.len().saturating_mul(4) < words.entries.capacity() {
            words.entries.shrink_to_fit();
            words.reconcile();
        }
        value
    } else {
        // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
        // constructor.
        let mut data = unsafe { map_data(map) };
        debug_assert_eq!(data.layout.key_kind == 3, key_is_ref != 0);
        // SAFETY: The map's declared key kind determines whether the word contains a retained
        // string or a scalar.
        let Some(key) = (unsafe { key_from_word(key_word, data.layout.key_kind) }) else {
            drop(data);
            raise_nan_key();
            return 1;
        };
        data.entries.remove_entry(&key).map(|(key, index)| {
            if let MapKey::Str(text) = key {
                data.key_bytes -= text.capacity();
            }
            let value = data.values[index];
            if data.layout.value_is_ref {
                // SAFETY: The owner and non-null referent are live GC payloads with headers
                // at the addresses used by this filter.
                unsafe {
                    write_barrier_object_owner(
                        map,
                        value as *mut u8,
                        std::ptr::null_mut(),
                        GcStoreDestination::MapValue as i64,
                    )
                };
            }
            data.values[index] = 0;
            data.free.push(index);
            data.compact_after_remove(map);
            value
        })
    };
    match value {
        Some(value) => {
            // SAFETY: The ABI caller provides a live, aligned, writable output slot for this
            // result.
            unsafe { out_payload.write(value) };
            0
        }
        None => 1,
    }
}

/// Produce an independent snapshot in ascending key order: numeric order for
/// scalars (false before true), UTF-8 byte order for strings. Mode 0 yields
/// keys, 1 values, and 2 `(key, value)` tuples. Later mutations are invisible.
///
/// # Safety
/// A non-null `map` must be a live, aligned payload from a Willow map constructor,
/// kept reachable through the call. A local map must stay with its owning invocation.
/// The requested mode and value representation must agree with the map type. For pair
/// values, each value must point to a live aligned two-word box with two initialized
/// words and the selected scalar or interface tracing layout.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_map_snapshot(map: *mut u8, mode: i64, value_pair: i64) -> *mut u8 {
    use crate::array::{willow_array_new, willow_array_set};
    use crate::gc::GcRootHandle;
    if map.is_null() {
        crate::panic_context::raise_language_message("cannot iterate a null Map");
        return std::ptr::null_mut();
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let layout = unsafe { storage(map) }.layout;
    // Root copied reference values before the first GC allocation. Native key
    // copies are independent of the source; no map borrow survives allocation.
    let mut entries: Vec<(MapKey, i64, Option<GcRootHandle>)> =
        // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
        // constructor.
        if let MapState::Words(words) = &unsafe { storage(map) }.state {
            words
                .access()
                .entries
                .iter()
                .map(|(&key, &value)| (MapKey::Word(key), value, None))
                .collect()
        } else {
            // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
            // constructor.
            let data = unsafe { map_data(map) };
            data.entries
                .iter()
                .map(|(key, &index)| {
                    let value = data.values[index];
                    (
                        key.clone(),
                        value,
                        (mode != 0 && data.layout.value_is_ref)
                            .then(|| GcRootHandle::new(value as *mut u8)),
                    )
                })
                .collect()
        };
    entries.sort_unstable_by(|a, b| {
        crate::observation::record(crate::observation::Event::SnapshotComparisons, 1);
        match (&a.0, &b.0) {
            (MapKey::Str(a), MapKey::Str(b)) => a.cmp(b),
            (MapKey::Word(a), MapKey::Word(b)) if layout.key_kind == 1 => {
                f64::from_bits(*a as u64).total_cmp(&f64::from_bits(*b as u64))
            }
            (MapKey::Word(a), MapKey::Word(b))
                if layout.key_kind == willow_abi::collection_element::U64 =>
            {
                (*a as u64).cmp(&(*b as u64))
            }
            (MapKey::Word(a), MapKey::Word(b)) => a.cmp(b),
            _ => unreachable!(),
        }
    });
    let is_ref = match mode {
        0 => layout.key_kind == 3,
        1 => layout.value_is_ref,
        _ => true,
    };
    let element_kind = match mode {
        0 => layout.key_kind,
        1 => layout.value_kind,
        _ => willow_abi::collection_element::OBJECT,
    };
    let flags = willow_abi::collection_element::storage_flags(element_kind, is_ref);
    // SAFETY: storage_flags derives a full-width slot for every traced element.
    let array = GcRootHandle::new(unsafe { willow_array_new(entries.len() as i64, flags) });
    for (index, (key, value, root)) in entries.into_iter().enumerate() {
        let key = if mode == 1 {
            0
        } else {
            match key {
                MapKey::Word(word) => word,
                MapKey::Str(text) => crate::string::willow_string_from_str(&text) as i64,
            }
        };
        let value = root.as_ref().map_or(value, |root| root.load() as i64);
        let word = match mode {
            0 => key,
            1 if value_pair != 0 => {
                // SAFETY: The map value kind identifies a retained GC pair containing two aligned
                // words.
                (unsafe { crate::array::copy_scalar_pair(value as *const i64) }) as i64
            }
            1 => value,
            _ => {
                use willow_abi::SlotKind::{GcRef, InterfacePair, ScalarPair, Word};
                let slots = [
                    if layout.key_kind == 3 { GcRef } else { Word },
                    match value_pair {
                        1 => ScalarPair,
                        2 => InterfacePair,
                        _ if layout.value_is_ref => GcRef,
                        _ => Word,
                    },
                ];
                let mut words = [key, value, 0];
                let width = if value_pair == 0 {
                    2
                } else {
                    let pair = value as *const i64;
                    // SAFETY: The map value kind identifies a retained GC pair containing two
                    // aligned words.
                    unsafe {
                        words[1] = pair.read();
                        words[2] = pair.add(1).read();
                    }
                    3
                };
                // SAFETY: The variant layout selects its initialized word or reference
                // payload, including the traced low word for a pair.
                (unsafe {
                    crate::gc::willow_alloc_enum_variant(
                        0,
                        willow_abi::EnumVariantLayout::new(0, &slots),
                        &words[..width],
                    )
                }) as i64
            }
        };
        // SAFETY: The result root is reloaded after constructing the element. Its layout
        // matches the snapshot mode and the index is within the allocated count.
        unsafe { willow_array_set(array.load(), index as i64, word) };
    }
    array.load()
}

/// Allocate an independent copy of `map` (same entries + value ref-ness). Backs
/// `Map<K,V>::freeze()` -> `FrozenMap<K,V>`: the copy shares no
/// `MapData` with the original, so it is safe to treat as immutable / Sync.
///
/// # Safety
/// A non-null `map` must be a live, aligned payload from a Willow map constructor,
/// kept reachable through the call. A local map must stay with its owning invocation.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_map_copy(map: *mut u8) -> *mut u8 {
    if map.is_null() {
        return willow_map_new(0, 0, 0);
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let layout = unsafe { storage(map) }.layout;
    // Allocate the destination BEFORE reading any value word out of the source.
    // `willow_map_new` can collect, and a moving minor
    // collection evacuates young values reachable only through the rooted
    // source map, rewriting the source's slots through `trace_map`. A native
    // snapshot taken before that allocation would still hold the pre-move
    // addresses and put dangling pointers into the copy. Once the destination
    // exists, nothing below GC-allocates or polls a safepoint (the key clones
    // are Rust-heap allocations), so the words read from the source are the
    // current ones.
    let copy = willow_map_new(
        layout.key_kind,
        layout.value_kind,
        i64::from(layout.value_is_ref),
    );
    if copy.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    if let MapState::Words(source) = &unsafe { storage(map) }.state {
        // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
        // constructor.
        let MapState::Words(destination) = &unsafe { storage(copy) }.state else {
            unreachable!()
        };
        let entries = source.access().entries.clone();
        let mut dst = destination.access();
        dst.entries = entries;
        dst.reconcile();
        return copy;
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let src = unsafe { map_data(map) };
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let mut dst = unsafe { map_data(copy) };
    dst.entries.reserve(src.entries.len());
    dst.values.reserve(src.values.len());
    dst.free.clone_from(&src.free);
    for &v in &src.values {
        if layout.value_is_ref {
            // SAFETY: The owner and non-null referent are live GC payloads with headers at
            // the addresses used by this filter.
            unsafe {
                write_barrier_object_owner(
                    copy,
                    std::ptr::null_mut(),
                    v as *mut u8,
                    GcStoreDestination::MapValue as i64,
                )
            };
        }
        dst.values.push(v);
    }
    for (key, &index) in &src.entries {
        let key = key.clone();
        if let MapKey::Str(text) = &key {
            dst.key_bytes += text.capacity();
        }
        dst.entries.insert(key, index);
    }
    copy
}

/// Number of entries.
///
/// # Safety
/// A non-null `map` must be a live, aligned payload from a Willow map constructor,
/// kept reachable through the call. A local map must stay with its owning invocation.
///
/// ```compile_fail,E0133
/// willow_runtime::map::willow_map_len(std::ptr::null_mut());
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_map_len(map: *mut u8) -> i64 {
    if map.is_null() {
        return 0;
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    match &unsafe { storage(map) }.state {
        MapState::Words(words) => words.access().entries.len() as i64,
        // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
        // constructor.
        MapState::Traced(_) | MapState::Local(_) => unsafe { map_data(map) }.entries.len() as i64,
    }
}

/// Whether `key` is present (1) or not (0).
///
/// # Safety
/// A non-null `map` must be a live, aligned payload from a Willow map constructor,
/// kept reachable through the call. A local map must stay with its owning invocation.
/// The key flag and word must match the map layout. A string key must be null or
/// point to a live immutable WillowString through the lookup.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_map_contains(map: *mut u8, key_word: i64, key_is_ref: i64) -> i64 {
    if map.is_null() {
        return 0;
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let storage = unsafe { storage(map) };
    if let MapState::Words(words) = &storage.state {
        debug_assert_eq!(key_is_ref, 0);
        let Some(key) = word_key(key_word, storage.layout.key_kind) else {
            raise_nan_key();
            return 0;
        };
        return i64::from(words.access().entries.contains_key(&key));
    }
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let data = unsafe { map_data(map) };
    debug_assert_eq!(data.layout.key_kind == 3, key_is_ref != 0);
    // SAFETY: The map's declared key kind determines whether the word contains a retained string
    // or a scalar.
    let Some(key) = (unsafe { key_from_word(key_word, data.layout.key_kind) }) else {
        drop(data);
        raise_nan_key();
        return 0;
    };
    i64::from(data.entries.contains_key(&key))
}

// Willow `Option` layout (must match the compiler's enum lowering):
//   Some(v) -> 2-word object [tag = 0, payload]
//   None    -> 1-word object [tag = 1]
// Variant tags follow declaration order in src/prelude.rs (`Some`, then `None`).

fn alloc_some(value_word: i64, val_is_ref: bool) -> *mut u8 {
    const WORD: &[willow_abi::SlotKind] = &[willow_abi::SlotKind::Word];
    const REF: &[willow_abi::SlotKind] = &[willow_abi::SlotKind::GcRef];
    // SAFETY: The variant layout selects its initialized word or reference payload, including
    // the traced low word for a pair.
    unsafe {
        crate::gc::willow_alloc_enum_variant(
            0,
            willow_abi::EnumVariantLayout::new(0, if val_is_ref { REF } else { WORD }),
            &[value_word],
        )
    }
}

fn alloc_none() -> *mut u8 {
    // SAFETY: The variant layout selects its initialized word or reference payload, including
    // the traced low word for a pair.
    unsafe {
        crate::gc::willow_alloc_enum_variant(0, willow_abi::EnumVariantLayout::new(1, &[]), &[])
    }
}

/// Debug display of a whole map: `{a: 1, b: 2}` — entries sorted by rendered key
/// for deterministic output (HashMap iteration order is not stable). Kinds follow
/// shared `collection_element` metadata; string KEYS are
/// printed bare (they are identifiers-like), string VALUES are quoted. `key_kind` is what
/// tells a word key apart from the `f64` and
/// `bool` that share its representation, so `Map<f64, V>` prints `1.5` and not
/// the bit pattern. Returns a newly allocated WillowString.
///
/// # Safety
/// A non-null `map` must be a live, aligned payload from a Willow map constructor,
/// kept reachable through the call. A local map must stay with its owning invocation.
/// The stored kind tags must describe the entries, including live or null WillowStrings
/// where the layout selects string formatting.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_map_to_string(map: *mut u8) -> *mut u8 {
    let (key_kind, val_kind) = if map.is_null() {
        (0, 0)
    } else {
        // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
        // constructor.
        let layout = unsafe { storage(map) }.layout;
        (layout.key_kind, layout.value_kind)
    };
    let mut entries: Vec<(String, i64)> = if map.is_null() {
        Vec::new()
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    } else if let MapState::Words(words) = &unsafe { storage(map) }.state {
        words
            .access()
            .entries
            .iter()
            .map(|(&key, &value)| {
                (
                    // SAFETY: Word keys are scalar; the map owns string keys as native text.
                    unsafe { crate::array::element_word_to_string(key, key_kind) },
                    value,
                )
            })
            .collect()
    } else {
        // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
        // constructor.
        let data = unsafe { map_data(map) };
        data.entries
            .iter()
            .map(|(k, &index)| {
                let key = match k {
                    // SAFETY: Word keys are scalar; the map owns string keys as native text.
                    MapKey::Word(n) => unsafe {
                        crate::array::element_word_to_string(*n, key_kind)
                    },
                    MapKey::Str(s) => s.clone(),
                };
                (key, data.values[index])
            })
            .collect()
    };
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::from("{");
    for (i, (key, word)) in entries.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(key);
        out.push_str(": ");
        // SAFETY: The declared value kind matches each word. No GC point intervenes between
        // copying and formatting them.
        out.push_str(&unsafe { crate::array::element_word_to_string(*word, val_kind) });
    }
    out.push('}');
    crate::string::willow_string_from_str(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected O(1) per operation: key equality checks per lookup, measured
    /// on the exact table types and hashers the maps use (`WordHasher` for
    /// scalar keys, the default hashbrown builder for string keys). Hits must
    /// average close to one comparison and misses close to zero at every size
    /// and key shape, including strided and negative words that defeat weak
    /// mixers; the averages must not grow with n.
    #[test]
    fn hash_lookups_compare_a_constant_number_of_keys() {
        use std::cell::Cell;
        thread_local! { static COMPARISONS: Cell<usize> = const { Cell::new(0) }; }
        #[derive(Clone)]
        struct Counted<K>(K);
        impl<K: PartialEq> PartialEq for Counted<K> {
            fn eq(&self, other: &Self) -> bool {
                COMPARISONS.with(|count| count.set(count.get() + 1));
                self.0 == other.0
            }
        }
        impl<K: Eq> Eq for Counted<K> {}
        impl<K: Hash> Hash for Counted<K> {
            fn hash<H: Hasher>(&self, state: &mut H) {
                self.0.hash(state);
            }
        }
        fn measure<K: Hash + Eq + Clone, S: std::hash::BuildHasher + Default>(
            keys: &[K],
            absent: &[K],
        ) -> (f64, f64) {
            let mut table: hashbrown::HashMap<Counted<K>, usize, S> = hashbrown::HashMap::default();
            for (index, key) in keys.iter().enumerate() {
                table.insert(Counted(key.clone()), index);
            }
            COMPARISONS.with(|count| count.set(0));
            for (index, key) in keys.iter().enumerate() {
                assert_eq!(table.get(&Counted(key.clone())), Some(&index));
            }
            let hits = COMPARISONS.with(Cell::get) as f64 / keys.len() as f64;
            COMPARISONS.with(|count| count.set(0));
            for key in absent {
                assert_eq!(table.get(&Counted(key.clone())), None);
            }
            let misses = COMPARISONS.with(Cell::get) as f64 / absent.len() as f64;
            (hits, misses)
        }
        for count in [1_i64 << 10, 1 << 13, 1 << 16, 1 << 18] {
            for stride in [1_i64, 1 << 32, -1, 1024, 2654435761] {
                let keys: Vec<i64> = (0..count).map(|i| i.wrapping_mul(stride)).collect();
                let absent: Vec<i64> = (count..2 * count).map(|i| i.wrapping_mul(stride)).collect();
                let (hits, misses) = measure::<_, BuildHasherDefault<WordHasher>>(&keys, &absent);
                eprintln!(
                    "word count={count} stride={stride} hit_cmp={hits:.4} miss_cmp={misses:.4}"
                );
                assert!(
                    hits < 1.25,
                    "word count={count} stride={stride} hits={hits}"
                );
                assert!(
                    misses < 0.25,
                    "word count={count} stride={stride} misses={misses}"
                );
            }
            let keys: Vec<String> = (0..count).map(|i| format!("key-{i}")).collect();
            let absent: Vec<String> = (0..count).map(|i| format!("other-{i}")).collect();
            let (hits, misses) = measure::<_, hashbrown::DefaultHashBuilder>(&keys, &absent);
            eprintln!("string count={count} hit_cmp={hits:.4} miss_cmp={misses:.4}");
            assert!(hits < 1.25, "string count={count} hits={hits}");
            assert!(misses < 0.25, "string count={count} misses={misses}");
        }
    }

    #[test]
    fn confined_string_maps_scale_without_lookup_allocation_and_skip_gc_borrows() {
        use crate::scheduler::scaling_measurements::counting_allocator as counter;
        let _guard = crate::gc::isolated_runtime();
        for count in [16, 128, 1024, 8192] {
            for length in [20, 128] {
                // SAFETY: The test controls its isolated runtime at this reset boundary; no
                // other mutator or subsequent access relies on the discarded heap.
                unsafe { willow_gc_init() };
                let baseline = crate::gc_telemetry::external_bytes();
                // SAFETY: Only the creating test task accesses this local map while it is live.
                let mut map = unsafe { willow_map_new_local(3, 0, 0) };
                // SAFETY: The initialized slot stays at this address until its registration
                // is popped; collection may rewrite it there.
                unsafe { willow_push_root(&raw mut map) };
                // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by
                // the constructor.
                assert!(matches!(unsafe { &storage(map).state }, MapState::Local(_)));
                let mut keys = vec![std::ptr::null_mut(); count];
                STRING_HASH_BYTES.set(0);
                for (i, key) in keys.iter_mut().enumerate() {
                    *key = willow_string_from_str(&format!("{i:0length$}"));
                    // SAFETY: The slot storage stays live and stationary through the matching
                    // pop, including during collection.
                    unsafe { willow_push_root(key) };
                    // SAFETY: The constructor and reference flags agree. Each pointer word
                    // comes from a live fixture object at the call.
                    unsafe { willow_map_insert(map, *key as i64, 1, i as i64, 0) };
                }
                let insertion_bytes = STRING_HASH_BYTES.get();
                assert!(insertion_bytes <= 6 * count * length);
                let allocated = counter::thread_allocations();
                let gc_bytes = crate::gc::willow_gc_allocated_bytes();
                let retained = crate::gc_telemetry::external_bytes();
                STRING_HASH_BYTES.set(0);
                for (i, &key) in keys.iter().enumerate() {
                    let mut payload = -1;
                    assert_eq!(
                        // SAFETY: The fixture supplies the live map and a key of its declared
                        // kind; the output borrows a disjoint local i64.
                        unsafe { willow_map_get_into(map, key as i64, 1, &mut payload) },
                        0
                    );
                    assert_eq!(payload, i as i64);
                    // SAFETY: The constructor and reference flags agree. Each pointer word
                    // comes from a live fixture object at the call.
                    unsafe { willow_map_insert(map, key as i64, 1, !payload, 0) };
                }
                assert_eq!(STRING_HASH_BYTES.get(), 2 * count * length);
                assert_eq!(counter::thread_allocations(), allocated);
                assert_eq!(crate::gc::willow_gc_allocated_bytes(), gc_bytes);
                assert_eq!(crate::gc_telemetry::external_bytes(), retained);
                // A concurrent tracer must not borrow confined scalar-value state.
                // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by
                // the constructor.
                let borrowed = unsafe { map_data(map) };
                let mut slots = Vec::new();
                let mut children = Vec::new();
                // SAFETY: The callback receives the live map payload selected by its registered
                // layout.
                unsafe {
                    trace_map(map, &mut slots);
                    snapshot_map(map, &mut children);
                }
                assert!(matches!(
                    // SAFETY: The callback receives the live map payload selected by its
                    // registered layout.
                    unsafe { snapshot_map_slice(map, 0, 1, &mut children) },
                    crate::gc::TraceSliceProgress::Done
                ));
                assert!(slots.is_empty() && children.is_empty());
                drop(borrowed);
                eprintln!(
                    "strings count={count} length={length} insertion_bytes={insertion_bytes} lookup_update_bytes={} allocations=0",
                    2 * count * length
                );
                // SAFETY: The removed shadow-stack entries belong to this operation and their
                // slot storage is still live.
                unsafe { willow_pop_roots(count as i32) };
                willow_gc_collect();
                // SAFETY: The source is a live map from the constructor and remains reachable
                // while copied.
                let mut copy = unsafe { willow_map_copy(map) };
                // SAFETY: The initialized slot stays at this address until its registration
                // is popped; collection may rewrite it there.
                unsafe { willow_push_root(&raw mut copy) };
                let key = willow_string_from_str(&format!("{:0length$}", count - 1));
                let mut payload = 0;
                assert_eq!(
                    // SAFETY: The fixture supplies the live map and a key of its declared kind;
                    // the output borrows a disjoint local i64.
                    unsafe { willow_map_get_into(copy, key as i64, 1, &mut payload) },
                    0
                );
                assert_eq!(payload, !((count - 1) as i64));
                // SAFETY: The removed shadow-stack entries belong to this operation and their
                // slot storage is still live.
                unsafe { willow_pop_roots(2) };
                willow_gc_collect();
                assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
            }
        }
    }

    #[test]
    fn confined_constructor_keeps_reference_values_synchronized() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: Only the creating test task accesses this local map while it is live.
        let mut map = unsafe { willow_map_new_local(3, 3, 1) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut map) };
        assert!(matches!(
            // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
            // constructor.
            unsafe { &storage(map).state },
            MapState::Traced(_)
        ));
        let mut key = willow_string_from_str("key");
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut key) };
        let value = willow_string_from_str("value");
        // SAFETY: The constructor and reference flags agree. Each pointer word comes from a
        // live fixture object at the call.
        unsafe { willow_map_insert(map, key as i64, 1, value as i64, 1) };
        willow_gc_collect();
        // SAFETY: The key matches the live map layout; any string key is current through the
        // lookup.
        let found = unsafe { willow_map_get(map, key as i64, 1, 1) };
        assert_eq!(
            // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
            unsafe { crate::string::willow_string_as_str(found) },
            "value"
        );
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(2) };
    }

    #[test]
    fn confined_scalar_accesses_never_acquire_mutual_exclusion() {
        let _guard = crate::gc::isolated_runtime();
        for count in [16, 128, 1024, 8192] {
            // SAFETY: The test controls its isolated runtime at this reset boundary; no other
            // mutator or subsequent access relies on the discarded heap.
            unsafe { crate::gc::willow_gc_init() };
            // SAFETY: Only the creating test task accesses this local map while it is live.
            let mut map = unsafe { willow_map_new_local(0, 0, 0) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { crate::gc::willow_push_root(&raw mut map) };
            assert!(matches!(
                // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by
                // the constructor.
                &unsafe { storage(map) }.state,
                MapState::Words(ScalarStorage::Local(_))
            ));
            WORD_SHARED_ACQUIRES.with(|count| count.set(0));
            for key in 0..count {
                // SAFETY: This live map was constructed with scalar key and value slots.
                unsafe { willow_map_insert(map, key, 0, !key, 0) };
                let mut payload = 0;
                // SAFETY: The fixture supplies the live map and a key of its declared kind;
                // the output borrows a disjoint local i64.
                assert_eq!(unsafe { willow_map_get_into(map, key, 0, &mut payload) }, 0);
                assert_eq!(payload, !key);
                assert_eq!(
                    // SAFETY: The fixture supplies the live map and a key of its declared kind;
                    // the output borrows a disjoint local i64.
                    unsafe { willow_map_get_into(map, key + count, 0, &mut payload) },
                    1
                );
                // SAFETY: The live map has scalar keys and this lookup borrows no pointer-
                // valued key. The handle is a live map from this fixture or its frozen copy.
                unsafe {
                    assert_eq!(willow_map_contains(map, key, 0), 1);
                    assert_eq!(willow_map_len(map), key + 1);
                }
            }
            assert_eq!(WORD_SHARED_ACQUIRES.with(|count| count.get()), 0);
            eprintln!(
                "confined operations={} mutual_exclusion_acquisitions=0",
                count * 5
            );
            // SAFETY: The source is a live map from the constructor and remains reachable
            // while copied.
            let mut copy = unsafe { willow_map_copy(map) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { crate::gc::willow_push_root(&raw mut copy) };
            assert!(matches!(
                // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by
                // the constructor.
                &unsafe { storage(copy) }.state,
                MapState::Words(ScalarStorage::Shared(_))
            ));
            // SAFETY: The handle is a live map from this fixture or its frozen copy.
            assert_eq!(unsafe { willow_map_len(copy) }, count);
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { crate::gc::willow_pop_roots(2) };
            crate::gc::willow_gc_collect();
        }
    }

    #[test]
    fn scalar_storage_uncontended_access_does_not_take_the_parking_mutex() {
        let storage = WordStorage::default();
        // Holding the slow-path mutex must not prevent fast acquire/release.
        // There are no contenders, so neither path should touch this mutex.
        let _parking = storage.waiters.lock().unwrap();
        for _ in 0..32 {
            let access = storage.lock();
            assert!(access.entries.is_empty());
            assert_eq!(storage.state.load(Ordering::Relaxed), 1);
            drop(access);
            assert_eq!(storage.state.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn scalar_storage_gate_serializes_retained_aliases_and_releases_on_unwind() {
        let _guard = crate::gc::isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { crate::gc::willow_gc_init() };
        let storage = WordStorage::default();
        storage.lock().entries.insert(0, 0);
        let start = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let storage = &storage;
                let start = &start;
                scope.spawn(crate::runtime::serving(move || {
                    start.wait();
                    for _ in 0..1024 {
                        let mut data = storage.lock();
                        let previous = data.entries[&0];
                        // Widen overlap: without the shared storage gate these
                        // distinct aliases would race in the read/modify/write.
                        std::thread::yield_now();
                        data.entries.insert(0, previous + 1);
                    }
                }));
            }
        });
        assert_eq!(storage.lock().entries[&0], 4096);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _access = storage.lock();
                panic!("release the storage gate on unwind");
            }))
            .is_err()
        );
        assert_eq!(storage.state.load(Ordering::Relaxed), 0);
        assert_eq!(storage.lock().entries[&0], 4096);
    }

    #[test]
    fn scalar_map_aliases_share_one_gate_across_all_accessors() {
        let _guard = crate::gc::isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { crate::gc::willow_gc_init() };
        let mut map = willow_map_new(0, 0, 0);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { crate::gc::willow_push_root(&raw mut map) };
        let address = map as usize;
        let start = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for writer in 0..4 {
                let start = &start;
                scope.spawn(crate::runtime::serving(move || {
                    // Raw aliases model independent wrappers and an original
                    // map alias. No external lock protects these operations.
                    let map = address as *mut u8;
                    start.wait();
                    for i in 0..4096 {
                        let key = writer * 4096 + i;
                        // SAFETY: This live map was constructed with scalar key and value
                        // slots.
                        unsafe { willow_map_insert(map, key, 0, !key, 0) };
                        let mut value = 0;
                        // SAFETY: The fixture supplies the live map and a key of its declared
                        // kind; the output borrows a disjoint local i64.
                        assert_eq!(unsafe { willow_map_get_into(map, key, 0, &mut value) }, 0);
                        assert_eq!(value, !key);
                        // SAFETY: The live map has scalar keys and this lookup borrows no
                        // pointer-valued key. The handle is a live map from this fixture or
                        // its frozen copy.
                        unsafe {
                            assert_eq!(willow_map_contains(map, key, 0), 1);
                            assert!(willow_map_len(map) > i);
                        }
                    }
                }));
            }
        });
        // SAFETY: The handle is a live map from this fixture or its frozen copy.
        assert_eq!(unsafe { willow_map_len(map) }, 16384);
        for key in 0..16384 {
            let mut value = 0;
            // SAFETY: The fixture supplies the live map and a key of its declared kind; the
            // output borrows a disjoint local i64.
            assert_eq!(unsafe { willow_map_get_into(map, key, 0, &mut value) }, 0);
            assert_eq!(value, !key);
        }
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { crate::gc::willow_pop_root() };
        crate::gc::willow_gc_collect();
    }

    #[test]
    fn scalar_map_hash_work_and_lookup_allocations_scale_linearly() {
        use crate::scheduler::scaling_measurements::counting_allocator as counter;
        let _guard = crate::gc::isolated_runtime();
        for count in [16_i64, 128, 1024, 8192] {
            for stride in [1_i64, 2654435761, 1 << 32, -1] {
                // SAFETY: The test controls its isolated runtime at this reset boundary; no
                // other mutator or subsequent access relies on the discarded heap.
                unsafe { crate::gc::willow_gc_init() };
                let baseline = crate::gc_telemetry::external_bytes();
                let mut map = willow_map_new(0, 0, 0);
                // SAFETY: The initialized slot stays at this address until its registration
                // is popped; collection may rewrite it there.
                unsafe { crate::gc::willow_push_root(&raw mut map) };
                WORD_HASH_CALLS.with(|calls| calls.set(0));
                for i in 0..count {
                    // SAFETY: This live map was constructed with scalar key and value slots.
                    unsafe { willow_map_insert(map, i.wrapping_mul(stride), 0, i, 0) };
                }
                let insert_hashes = WORD_HASH_CALLS.with(|calls| calls.get());
                // Geometric table growth rehashes old keys, but total hashes
                // remain linear; this bound includes every growth in the loop.
                assert!(insert_hashes <= 4 * count as usize);
                let allocated = counter::thread_allocations();
                let gc_bytes = crate::gc::willow_gc_allocated_bytes();
                let retained = crate::gc_telemetry::external_bytes();
                WORD_HASH_CALLS.with(|calls| calls.set(0));
                for i in 0..count {
                    let key = i.wrapping_mul(stride);
                    let mut payload = -1;
                    // SAFETY: The fixture supplies the live map and a key of its declared
                    // kind; the output borrows a disjoint local i64.
                    assert_eq!(unsafe { willow_map_get_into(map, key, 0, &mut payload) }, 0);
                    assert_eq!(payload, i);
                    // SAFETY: The live map has scalar keys and this lookup borrows no
                    // pointer-valued key.
                    assert_eq!(unsafe { willow_map_contains(map, key, 0) }, 1);
                    assert_eq!(
                        // SAFETY: The fixture supplies the live map and a key of its declared
                        // kind; the output borrows a disjoint local i64.
                        unsafe {
                            willow_map_get_into(
                                map,
                                (i + count).wrapping_mul(stride),
                                0,
                                &mut payload,
                            )
                        },
                        1
                    );
                    assert_eq!(payload, 0);
                    // SAFETY: This live map was constructed with scalar key and value slots.
                    unsafe { willow_map_insert(map, key, 0, !i, 0) };
                }
                assert_eq!(
                    WORD_HASH_CALLS.with(|calls| calls.get()),
                    4 * count as usize
                );
                assert_eq!(counter::thread_allocations(), allocated);
                assert_eq!(crate::gc::willow_gc_allocated_bytes(), gc_bytes);
                assert_eq!(crate::gc_telemetry::external_bytes(), retained);
                // SAFETY: The handle is a live map from this fixture or its frozen copy.
                assert_eq!(unsafe { willow_map_len(map) }, count);
                eprintln!(
                    "scalar count={count} stride={stride} insert_hashes={insert_hashes} lookup_update_hashes={} allocations=0",
                    4 * count
                );
                // SAFETY: The removed shadow-stack entries belong to this operation and their
                // slot storage is still live.
                unsafe { crate::gc::willow_pop_root() };
                crate::gc::willow_gc_collect();
                assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
            }
        }
    }

    #[test]
    fn scalar_gc_hooks_do_not_borrow_mutating_storage() {
        let _guard = crate::gc::isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { crate::gc::willow_gc_init() };
        let mut map = willow_map_new(0, 0, 0);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { crate::gc::willow_push_root(&raw mut map) };
        let address = map as usize;
        std::thread::scope(|scope| {
            scope.spawn(crate::runtime::serving(move || {
                let map = address as *mut u8;
                let mut slots = Vec::new();
                let mut children = Vec::new();
                for _ in 0..8192 {
                    // Exercise all hooks while the mutator may resize its table.
                    // SAFETY: The callback receives the live map payload selected by its
                    // registered layout.
                    unsafe {
                        trace_map(map, &mut slots);
                        snapshot_map(map, &mut children);
                        assert!(matches!(
                            snapshot_map_slice(map, 0, 1, &mut children),
                            crate::gc::TraceSliceProgress::Done
                        ));
                    }
                }
                assert!(slots.is_empty());
                assert!(children.is_empty());
            }));
            for key in 0..8192 {
                // SAFETY: This live map was constructed with scalar key and value slots.
                unsafe { willow_map_insert(map, key, 0, key, 0) };
            }
        });
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { crate::gc::willow_pop_root() };
        crate::gc::willow_gc_collect();
    }

    #[test]
    fn scalar_frozen_copy_supports_parallel_reads_and_independent_updates() {
        let _guard = crate::gc::isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { crate::gc::willow_gc_init() };
        let baseline = crate::gc_telemetry::external_bytes();
        let mut map = willow_map_new(0, 0, 0);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { crate::gc::willow_push_root(&raw mut map) };
        for key in 0..1024 {
            // SAFETY: This live map was constructed with scalar key and value slots.
            unsafe { willow_map_insert(map, key, 0, !key, 0) };
        }
        // SAFETY: The source is a live map from the constructor and remains reachable while
        // copied.
        let mut copy = unsafe { willow_map_copy(map) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { crate::gc::willow_push_root(&raw mut copy) };
        // SAFETY: This live map was constructed with scalar key and value slots.
        unsafe { willow_map_insert(map, 0, 0, 42, 0) };
        let address = copy as usize;
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(crate::runtime::serving(move || {
                    let map = address as *mut u8;
                    for key in 0..1024 {
                        let mut payload = 0;
                        // SAFETY: The fixture supplies the live map and a key of its declared
                        // kind; the output borrows a disjoint local i64.
                        assert_eq!(unsafe { willow_map_get_into(map, key, 0, &mut payload) }, 0);
                        assert_eq!(payload, !key);
                    }
                }));
            }
        });
        crate::gc::willow_gc_collect();
        let mut payload = 0;
        // SAFETY: The fixture supplies the live map and a key of its declared kind; the
        // output borrows a disjoint local i64.
        assert_eq!(unsafe { willow_map_get_into(copy, 0, 0, &mut payload) }, 0);
        assert_eq!(payload, -1);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { crate::gc::willow_pop_roots(2) };
        crate::gc::willow_gc_collect();
        assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
    }

    #[test]
    fn concurrent_slice_retries_a_busy_map_without_waiting_or_partial_children() {
        let _guard = crate::gc::isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { crate::gc::willow_gc_init() };
        let mut map = willow_map_new(0, 0, 1);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { crate::gc::willow_push_root(&raw mut map) };
        let child = crate::gc::willow_alloc(8);
        // SAFETY: The constructor and reference flags agree. Each pointer word comes from a
        // live fixture object at the call.
        unsafe { willow_map_insert(map, 0, 0, child as i64, 1) };
        // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
        // constructor.
        let guard = unsafe { map_data(map) };
        let mut children = Vec::new();
        assert!(matches!(
            // SAFETY: The callback receives the live map payload selected by its registered
            // layout.
            unsafe { snapshot_map_slice(map, 0, 512, &mut children) },
            crate::gc::TraceSliceProgress::Retry
        ));
        assert!(children.is_empty());
        drop(guard);
        assert!(matches!(
            // SAFETY: The callback receives the live map payload selected by its registered
            // layout.
            unsafe { snapshot_map_slice(map, 0, 512, &mut children) },
            crate::gc::TraceSliceProgress::Done
        ));
        assert_eq!(children, [child]);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { crate::gc::willow_pop_root() };
        crate::gc::willow_gc_collect();
    }
    use crate::gc::{
        isolated_runtime, willow_gc_collect, willow_gc_init, willow_pop_roots, willow_push_root,
    };
    use crate::string::{willow_string_as_str, willow_string_from_str};

    fn opt_tag(opt: *mut u8) -> i64 {
        // SAFETY: The Option result is a GC word-aligned allocation containing the tag and its
        // payload.
        unsafe { *(opt as *const i64) }
    }
    fn opt_payload(opt: *mut u8) -> i64 {
        // SAFETY: The Option result is a GC word-aligned allocation containing the tag and its
        // payload.
        unsafe { *((opt as *const i64).add(1)) }
    }

    #[test]
    fn map_unit_01_new_is_empty() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let m = willow_map_new(0, 0, 0);
        assert!(!m.is_null());
        // SAFETY: The handle is a live map from this fixture or its frozen copy.
        assert_eq!(unsafe { willow_map_len(m) }, 0);
    }

    #[test]
    fn inline_maps_remain_stable_and_are_reclaimed_at_increasing_counts() {
        use crate::gc::{GC_HEADER_SIZE, willow_gc_allocated_bytes};

        let _guard = isolated_runtime();
        for count in [1, 16, 128] {
            // SAFETY: The test controls its isolated runtime at this reset boundary; no other
            // mutator or subsequent access relies on the discarded heap.
            unsafe { willow_gc_init() };
            // Reserve every root slot before registering its address.
            let mut maps = vec![std::ptr::null_mut(); count];
            for slot in &mut maps {
                *slot = willow_map_new(3, 3, 1);
                assert!(!slot.is_null());
                assert!((*slot as usize).is_multiple_of(align_of::<MapStorage>()));
                // SAFETY: The slot storage stays live and stationary through the matching
                // pop, including during collection.
                unsafe { willow_push_root(slot) };
            }
            let payload_bytes = size_of::<MapStorage>();
            assert_eq!(
                willow_gc_allocated_bytes() as usize,
                count * (GC_HEADER_SIZE + payload_bytes)
            );
            let addresses = maps.clone();
            for &map in &maps {
                let mut key = willow_string_from_str("owned-key");
                // SAFETY: The initialized slot stays at this address until its registration
                // is popped; collection may rewrite it there.
                unsafe { willow_push_root(&raw mut key) };
                let value = willow_string_from_str("traced-value");
                // SAFETY: The constructor and reference flags agree. Each pointer word comes
                // from a live fixture object at the call.
                unsafe { willow_map_insert(map, key as i64, 1, value as i64, 1) };
                // SAFETY: The removed shadow-stack entries belong to this operation and their
                // slot storage is still live.
                unsafe { willow_pop_roots(1) };
            }
            willow_gc_collect();
            assert_eq!(maps, addresses, "inline mutexes must never move");
            for &map in &maps {
                let key = willow_string_from_str("owned-key");
                // SAFETY: The key matches the live map layout; any string key is current
                // through the lookup.
                let value = unsafe { willow_map_get(map, key as i64, 1, 1) };
                // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this
                // borrow.
                assert_eq!(unsafe { willow_string_as_str(value) }, "traced-value");
            }
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(count as i32) };
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
        }
    }

    #[test]
    fn map_unit_02_int_key_insert_get() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let m = willow_map_new(0, 0, 0);
        let _keep_m = crate::gc::GcRootHandle::new(m);
        // SAFETY: This live map was constructed with scalar key and value slots. The handle is
        // a live map from this fixture or its frozen copy.
        unsafe {
            willow_map_insert(m, 7, 0, 100, 0);
            willow_map_insert(m, 8, 0, 200, 0);
            assert_eq!(willow_map_len(m), 2);
        }
        // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
        let g = unsafe { willow_map_get(m, 7, 0, 0) };
        assert_eq!(opt_tag(g), 0); // Some
        assert_eq!(opt_payload(g), 100);
        // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
        assert_eq!(opt_tag(unsafe { willow_map_get(m, 99, 0, 0) }), 1); // None
    }

    #[test]
    fn map_get_into_preserves_payload_bits_without_allocating_wrappers() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        for kind in [0, 1, 2] {
            let mut map = willow_map_new(0, kind, 0);
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut map) };
            let words = [0, 1, -1, i64::MIN, i64::MAX, f64::NAN.to_bits() as i64];
            for (key, word) in words.iter().enumerate() {
                // SAFETY: This live map was constructed with scalar key and value slots.
                unsafe { willow_map_insert(map, key as i64, 0, *word, 0) };
            }
            let bytes = crate::gc::willow_gc_allocated_bytes();
            for count in [1, 16, 128] {
                for _ in 0..count {
                    for (key, word) in words.iter().enumerate() {
                        let mut payload = 123;
                        assert_eq!(
                            // SAFETY: The fixture supplies the live map and a key of its declared
                            // kind; the output borrows a disjoint local i64.
                            unsafe { willow_map_get_into(map, key as i64, 0, &mut payload) },
                            0
                        );
                        assert_eq!(payload, *word);
                    }
                    let mut payload = 123;
                    // SAFETY: The fixture supplies the live map and a key of its declared
                    // kind; the output borrows a disjoint local i64.
                    assert_eq!(unsafe { willow_map_get_into(map, 999, 0, &mut payload) }, 1);
                    assert_eq!(payload, 0);
                }
                assert_eq!(crate::gc::willow_gc_allocated_bytes(), bytes);
            }
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
        }
        let mut payload = 123;
        assert_eq!(
            // SAFETY: The null-map path writes the local output word and returns a miss.
            unsafe { willow_map_get_into(std::ptr::null_mut(), 0, 0, &mut payload) },
            1
        );
        assert_eq!(payload, 0);
    }

    #[test]
    fn map_unit_03_insert_overwrites() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let m = willow_map_new(0, 0, 0);
        // SAFETY: This live map was constructed with scalar key and value slots. The handle is
        // a live map from this fixture or its frozen copy. The live map has scalar keys and
        // this lookup borrows no pointer-valued key.
        unsafe {
            willow_map_insert(m, 1, 0, 10, 0);
            willow_map_insert(m, 1, 0, 20, 0);
            assert_eq!(willow_map_len(m), 1);
            assert_eq!(opt_payload(willow_map_get(m, 1, 0, 0)), 20);
        }
    }

    #[test]
    fn map_unit_04_string_keys_compare_by_content() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let m = willow_map_new(3, 0, 0);
        let _keep_m = crate::gc::GcRootHandle::new(m);
        let alice = willow_string_from_str("Alice");
        // SAFETY: The constructor and reference flags agree. Each pointer word comes from a
        // live fixture object at the call.
        unsafe { willow_map_insert(m, alice as i64, 1, 30, 0) };
        // A *different* string object with the same content must hit.
        let alice2 = willow_string_from_str("Alice");
        // SAFETY: The key matches the live map layout; any string key is current through the
        // lookup.
        let g = unsafe { willow_map_get(m, alice2 as i64, 1, 0) };
        assert_eq!(opt_tag(g), 0);
        assert_eq!(opt_payload(g), 30);
        let bob = willow_string_from_str("Bob");
        // SAFETY: The key matches the live map layout; any string key is current through the
        // lookup.
        assert_eq!(opt_tag(unsafe { willow_map_get(m, bob as i64, 1, 0) }), 1); // None
    }

    #[test]
    fn null_string_key_matches_empty_and_fills_one_output_word() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let map = crate::gc::GcRootHandle::new(willow_map_new(3, 0, 0));
        let empty = willow_string_from_str("");
        let mut output = [-11_i64, -1, -22];
        // SAFETY: The rooted map uses string keys and scalar values. Zero denotes
        // the empty key. The fresh string stays current through these non-allocating
        // lookups and the middle output word is exclusively borrowed for each call.
        unsafe {
            willow_map_insert(map.load(), 0, 1, 73, 0);
            assert_eq!(willow_map_contains(map.load(), empty as i64, 1), 1);
            assert_eq!(
                willow_map_get_into(map.load(), empty as i64, 1, &mut output[1]),
                0
            );
            assert_eq!(output, [-11, 73, -22]);
            assert_eq!(willow_map_remove_into(map.load(), 0, 1, &mut output[1]), 0);
            assert_eq!(output, [-11, 73, -22]);
            assert_eq!(willow_map_get_into(map.load(), 0, 1, &mut output[1]), 1);
        }
        assert_eq!(output, [-11, 0, -22]);
    }

    #[test]
    fn map_unit_05_contains() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let m = willow_map_new(0, 0, 0);
        // SAFETY: This live map was constructed with scalar key and value slots. The live map
        // has scalar keys and this lookup borrows no pointer-valued key.
        unsafe {
            willow_map_insert(m, 5, 0, 50, 0);
            assert_eq!(willow_map_contains(m, 5, 0), 1);
            assert_eq!(willow_map_contains(m, 6, 0), 0);
        }
    }

    #[test]
    fn map_unit_06_reference_values_survive_collection() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let mut m = willow_map_new(0, 3, 1);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut m) };
        let v = willow_string_from_str("kept-value");
        // SAFETY: The constructor and reference flags agree. Each pointer word comes from a
        // live fixture object at the call.
        unsafe { willow_map_insert(m, 1, 0, v as i64, 1) };
        willow_gc_collect();
        // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
        let g = unsafe { willow_map_get(m, 1, 0, 0) };
        assert_eq!(opt_tag(g), 0);
        let got = opt_payload(g) as *mut u8;
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        assert_eq!(unsafe { willow_string_as_str(got) }, "kept-value");
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
    }

    #[test]
    fn map_unit_07_reference_option_uses_nullable_pointer_niche() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let m = willow_map_new(0, 3, 1);
        let _keep_m = crate::gc::GcRootHandle::new(m);
        let value = willow_string_from_str("niche-value");
        // SAFETY: The constructor and reference flags agree. Each pointer word comes from a
        // live fixture object at the call. The live map has scalar keys and this lookup
        // borrows no pointer-valued key.
        unsafe {
            willow_map_insert(m, 1, 0, value as i64, 1);
            assert_eq!(willow_map_get(m, 1, 0, 1), value);
            assert!(willow_map_get(m, 2, 0, 1).is_null());
        }
    }
    #[test]
    fn map_layout_survives_freeze_and_drives_word_display() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let map = willow_map_new(1, 2, 0);
        let _keep_map = crate::gc::GcRootHandle::new(map);
        // SAFETY: This live map was constructed with scalar key and value slots.
        unsafe { willow_map_insert(map, 1.5f64.to_bits() as i64, 0, 1, 0) };
        // SAFETY: The source is a live map from the constructor and remains reachable while
        // copied.
        let copy = unsafe { willow_map_copy(map) };
        let _keep_copy = crate::gc::GcRootHandle::new(copy);
        // SAFETY: The map copy retains the declared key and value representations.
        let text = unsafe { willow_map_to_string(copy) };
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        assert_eq!(unsafe { willow_string_as_str(text) }, "{1.5: true}");
        // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
        // constructor.
        let layout = unsafe { storage(copy) }.layout;
        assert_eq!(
            (layout.key_kind, layout.value_kind, layout.value_is_ref),
            (1, 2, false)
        );
    }
    #[test]
    fn borrowed_string_keys_survive_copy_update_and_collection() {
        let _guard = isolated_runtime();
        for local in [false, true] {
            // SAFETY: The test controls its isolated runtime at this reset boundary; no other
            // mutator or subsequent access relies on the discarded heap.
            unsafe { willow_gc_init() };
            let mut map = if local {
                // SAFETY: Only the creating test task accesses this local map while it is live.
                unsafe { willow_map_new_local(3, 0, 0) }
            } else {
                willow_map_new(3, 0, 0)
            };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut map) };
            for (i, text) in ["", "日本語🦀", "ordinary", "embedded\0nul"]
                .into_iter()
                .enumerate()
            {
                let key = willow_string_from_str(text);
                // SAFETY: The constructor and reference flags agree. Each pointer word comes
                // from a live fixture object at the call.
                unsafe { willow_map_insert(map, key as i64, 1, i as i64, 0) };
            }
            // SAFETY: The source is a live map from the constructor and remains reachable
            // while copied.
            let mut copy = unsafe { willow_map_copy(map) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut copy) };
            // No source key is rooted: map keys own their bytes independently.
            willow_gc_collect();
            for (i, text) in ["", "日本語🦀", "ordinary", "embedded\0nul"]
                .into_iter()
                .enumerate()
            {
                let mut key = willow_string_from_str(text);
                // SAFETY: The initialized slot stays at this address until its registration
                // is popped; collection may rewrite it there.
                unsafe { willow_push_root(&raw mut key) };
                // SAFETY: The key matches the live map layout; any string key is current
                // through the lookup.
                assert_eq!(unsafe { willow_map_contains(map, key as i64, 1) }, 1);
                assert_eq!(
                    // SAFETY: The key matches the live map layout; any string key is current
                    // through the lookup.
                    opt_payload(unsafe { willow_map_get(copy, key as i64, 1, 0) }),
                    i as i64
                );
                // SAFETY: The constructor and reference flags agree. Each pointer word comes
                // from a live fixture object at the call.
                unsafe { willow_map_insert(map, key as i64, 1, 99, 0) };
                assert_eq!(
                    // SAFETY: The key matches the live map layout; any string key is current
                    // through the lookup.
                    opt_payload(unsafe { willow_map_get(copy, key as i64, 1, 0) }),
                    i as i64
                );
                assert_eq!(
                    // SAFETY: The key matches the live map layout; any string key is current
                    // through the lookup.
                    opt_payload(unsafe { willow_map_get(map, key as i64, 1, 0) }),
                    99
                );
                // SAFETY: The removed shadow-stack entries belong to this operation and their
                // slot storage is still live.
                unsafe { willow_pop_roots(1) };
            }
            // SAFETY: The handle is a live map from this fixture or its frozen copy.
            assert_eq!(unsafe { willow_map_len(map) }, 4);
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(2) };
        }
    }

    #[test]
    fn borrowed_word_keys_preserve_all_bits() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let map = willow_map_new(0, 0, 0);
        let _keep_map = crate::gc::GcRootHandle::new(map);
        let words = [
            0,
            i64::MIN,
            0x7ff8_0000_0000_0001,
            0x7ff8_0000_0000_0002,
            -1,
        ];
        for (i, word) in words.into_iter().enumerate() {
            // SAFETY: This live map was constructed with scalar key and value slots.
            unsafe { willow_map_insert(map, word, 0, i as i64, 0) };
        }
        for (i, word) in words.into_iter().enumerate() {
            // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued
            // key.
            assert_eq!(unsafe { willow_map_contains(map, word, 0) }, 1);
            assert_eq!(
                // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued
                // key.
                opt_payload(unsafe { willow_map_get(map, word, 0, 0) }),
                i as i64
            );
        }
        // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
        // The handle is a live map from this fixture or its frozen copy.
        unsafe {
            assert_eq!(willow_map_contains(map, 1, 0), 0);
            assert_eq!(willow_map_len(map), words.len() as i64);
        }
    }

    #[test]
    fn float_keys_normalize_zero_preserve_numbers_and_copy() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let mut map = willow_map_new(1, 0, 0);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut map) };
        for zeros in [[0.0_f64, -0.0], [-0.0_f64, 0.0]] {
            for (index, key) in zeros.into_iter().enumerate() {
                // SAFETY: This live map was constructed with scalar key and value slots.
                unsafe { willow_map_insert(map, key.to_bits() as i64, 0, index as i64 + 10, 0) };
            }
            // SAFETY: The handle is a live map from this fixture or its frozen copy.
            assert_eq!(unsafe { willow_map_len(map) }, 1);
            for key in zeros {
                assert_eq!(
                    // SAFETY: The live map has scalar keys and this lookup borrows no pointer-
                    // valued key.
                    unsafe { willow_map_contains(map, key.to_bits() as i64, 0) },
                    1
                );
                assert_eq!(
                    // SAFETY: The live map has scalar keys and this lookup borrows no
                    // pointer-valued key.
                    opt_payload(unsafe { willow_map_get(map, key.to_bits() as i64, 0, 0) }),
                    11
                );
            }
        }
        let values = [
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            -f64::from_bits(1),
            1.5,
            -1.5,
        ];
        for (index, value) in values.into_iter().enumerate() {
            // SAFETY: This live map was constructed with scalar key and value slots.
            unsafe { willow_map_insert(map, value.to_bits() as i64, 0, index as i64, 0) };
        }
        // SAFETY: The source is a live map from the constructor and remains reachable while
        // copied.
        let mut copy = unsafe { willow_map_copy(map) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut copy) };
        // SAFETY: The handle is a live map from this fixture or its frozen copy.
        assert_eq!(unsafe { willow_map_len(copy) }, 9);
        for (index, value) in values.into_iter().enumerate() {
            assert_eq!(
                // SAFETY: The live map has scalar keys and this lookup borrows no pointer-
                // valued key.
                opt_payload(unsafe { willow_map_get(copy, value.to_bits() as i64, 0, 0) }),
                index as i64
            );
        }
        assert_eq!(
            // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
            opt_payload(unsafe { willow_map_get(copy, i64::MIN, 0, 0) }),
            11
        );
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        assert!(!unsafe { willow_string_as_str(willow_map_to_string(copy)) }.contains("-0:"));
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(2) };
    }

    #[test]
    fn float_keys_reject_all_nan_forms_without_mutating_or_holding_lock() {
        use crate::panic_context::*;
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let previous = replace_current_context(Some(std::sync::Arc::new(PanicContext::new(903))));
        let mut map = willow_map_new(1, 0, 0);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut map) };
        // SAFETY: This live map was constructed with scalar key and value slots.
        unsafe { willow_map_insert(map, 0, 0, 42, 0) };
        for bits in [
            0x7ff0_0000_0000_0001u64,
            0x7ff8_0000_0000_0000,
            0x7fff_ffff_ffff_ffff,
        ] {
            for sign in [0, 1u64 << 63] {
                for operation in 0..5 {
                    let word = (bits | sign) as i64;
                    match operation {
                        // SAFETY: This live map was constructed with scalar key and value
                        // slots.
                        0 => unsafe { willow_map_insert(map, word, 0, 99, 0) },
                        // SAFETY: The live map has scalar keys and this lookup borrows no
                        // pointer-valued key.
                        1 => assert_eq!(unsafe { willow_map_contains(map, word, 0) }, 0),
                        4 => {
                            let mut payload = 123;
                            assert_eq!(
                                // SAFETY: The fixture supplies the live map and a key of its
                                // declared kind; the output borrows a disjoint local i64.
                                unsafe { willow_map_get_into(map, word, 0, &mut payload) },
                                1
                            );
                            assert_eq!(payload, 0);
                        }
                        _ => assert!(
                            // SAFETY: The live map has scalar keys and this lookup borrows no
                            // pointer-valued key.
                            unsafe { willow_map_get(map, word, 0, operation - 2) }.is_null()
                        ),
                    }
                    assert_eq!(willow_panic_depth(), 1);
                    willow_panic_enter_defer();
                    let info = willow_panic_recover();
                    willow_panic_leave_defer();
                    assert_eq!(
                        // SAFETY: The retained PanicInfo keeps its initialized fields live through
                        // this read.
                        unsafe { panic_info_message(info) },
                        "NaN cannot be used as a Map key"
                    );
                    willow_panic_release_recovered(info);
                    // SAFETY: The handle is a live map from this fixture or its frozen copy.
                    // The live map has scalar keys and this lookup borrows no pointer-valued
                    // key.
                    unsafe {
                        assert_eq!(willow_map_len(map), 1);
                        assert_eq!(opt_payload(willow_map_get(map, 0, 0, 0)), 42);
                    }
                }
            }
        }
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
        replace_current_context(previous);
    }

    #[test]
    fn float_key_normalization_scales_without_allocations() {
        use crate::scheduler::scaling_measurements::counting_allocator as counter;
        for count in [16usize, 128, 1024, 8192] {
            let allocations = counter::thread_allocations();
            for index in 0..count {
                let word = (index as f64).to_bits() as i64;
                assert!(
                    // SAFETY: The map's declared key kind determines whether the word contains a
                    // retained string or a scalar.
                    matches!(unsafe { key_from_word(word, 1) }, Some(KeyRef::Word(got)) if got == word)
                );
                // Integer keys that happen to encode NaN or -0.0 remain integers.
                assert!(matches!(
                    // SAFETY: The map's declared key kind determines whether the word contains a
                    // retained string or a scalar.
                    unsafe { key_from_word(i64::MIN, 0) },
                    Some(KeyRef::Word(i64::MIN))
                ));
                assert!(matches!(
                    // SAFETY: The map's declared key kind determines whether the word contains a
                    // retained string or a scalar.
                    unsafe { key_from_word(f64::NAN.to_bits() as i64, 0) },
                    Some(KeyRef::Word(_))
                ));
            }
            assert_eq!(counter::thread_allocations(), allocations);
            eprintln!("normalization calls={}, allocations=0", 3 * count);
        }
    }

    /// `freeze()` copies the value words a moving collection
    /// leaves in the source, not the ones it read before that collection.
    ///
    /// The scenario is a worker that freezes a map while another thread stops
    /// the world for a minor collection. The rooted source map is pinned, but
    /// its young values are reachable only through its slots, so they are
    /// COPIED to the old generation and the source's slots are rewritten. A
    /// native snapshot of those slots taken before `willow_map_copy`'s own
    /// allocation would carry the pre-move addresses into the copy.
    ///
    /// Deterministic ordering: this thread registers as a mutator, so the
    /// collector cannot proceed until it parks; `alloc` stress makes the
    /// destination allocation reach `collect_internal`, whose first act under a
    /// pending stop is to park at the safepoint. The collection therefore runs
    /// exactly between the copy's allocation and its return.
    #[test]
    fn copy_stores_values_as_relocated_by_a_collection_inside_its_allocation() {
        use std::time::{Duration, Instant};
        const VALUES: i64 = 8;

        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        crate::gc::willow_gc_register_mutator();
        // Environment stress must not promote the values before the copy: the
        // point is that they are still young when the collection runs.
        crate::gc::set_gc_stress_for_test(Some(""));
        let tls = crate::gc::tlab_state_for_test();
        let mut map = willow_map_new(0, 4, 1);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut map) };
        let mut young = Vec::new();
        for i in 0..VALUES {
            // SAFETY: The fixture owns the stable TLAB through unregistration. The requested
            // size and tracing metadata describe the object initialized here.
            let value = unsafe { crate::gc::willow_gc_alloc_slow(tls.as_ptr(), 42, 0, 8, 0) };
            assert!(!value.is_null());
            // SAFETY: The test allocates a full i64 in word-aligned GC storage and reads the live
            // value through the map or root.
            unsafe { *(value as *mut i64) = 1000 + i };
            // SAFETY: The constructor and reference flags agree. Each pointer word comes from
            // a live fixture object at the call.
            unsafe { willow_map_insert(map, i, 0, value as i64, 1) };
            young.push(value);
        }
        let moved_before = crate::gc::willow_gc_moved_objects();

        let collector =
            std::thread::spawn(crate::runtime::serving(crate::gc::minor_collect_internal));
        // Wait for the COORDINATION flag, not the lock-free gate: the gate is
        // published first, and a safepoint reached between the two returns
        // without parking, which would leave the collector waiting forever.
        let deadline = Instant::now() + Duration::from_secs(30);
        while !crate::gc::stop_pending_for_test() {
            assert!(
                Instant::now() < deadline,
                "the collector never requested a stop"
            );
            std::thread::yield_now();
        }
        crate::gc::set_gc_stress_for_test(Some("alloc"));
        // SAFETY: The source is a live map from the constructor and remains reachable while
        // copied.
        let mut copy = unsafe { willow_map_copy(map) };
        crate::gc::set_gc_stress_for_test(None);
        collector.join().unwrap();
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut copy) };

        assert_eq!(
            crate::gc::willow_gc_moved_objects() - moved_before,
            VALUES,
            "every value was evacuated while the copy was in progress"
        );
        for i in 0..VALUES {
            // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued
            // key.
            let source = unsafe { willow_map_get(map, i, 0, 1) };
            // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued
            // key.
            let copied = unsafe { willow_map_get(copy, i, 0, 1) };
            assert_ne!(source, young[i as usize], "value {i} moved");
            assert_eq!(copied, source, "copy holds the relocated value {i}");
            // SAFETY: The test allocates a full i64 in word-aligned GC storage and reads the live
            // value through the map or root.
            assert_eq!(unsafe { *(copied as *const i64) }, 1000 + i);
        }
        // SAFETY: The handle is a live map from this fixture or its frozen copy.
        assert_eq!(unsafe { willow_map_len(copy) }, VALUES);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(2) };
        crate::gc::willow_gc_unregister_mutator();
    }

    /// The copy's own slots take the write barrier: a young value copied into
    /// the old destination puts the DESTINATION in the remembered set (the
    /// source already is), so a later minor collection rewrites the copy as
    /// well as the source. The copy is a direct root here, which would get it
    /// traced anyway; the remembered-set count is what shows the barrier ran.
    #[test]
    fn copy_of_young_values_is_updated_by_the_next_minor_collection() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        crate::gc::set_gc_stress_for_test(Some(""));
        let tls = crate::gc::tlab_state_for_test();
        let mut map = willow_map_new(0, 4, 1);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut map) };
        // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size
        // and tracing metadata describe the object initialized here.
        let young = unsafe { crate::gc::willow_gc_alloc_slow(tls.as_ptr(), 42, 0, 8, 0) };
        // SAFETY: The test allocates a full i64 in word-aligned GC storage and reads the live
        // value through the map or root.
        unsafe { *(young as *mut i64) = 77 };
        // SAFETY: The constructor and reference flags agree. Each pointer word comes from a
        // live fixture object at the call.
        unsafe { willow_map_insert(map, 1, 0, young as i64, 1) };
        let remembered = crate::gc::willow_gc_remembered_set_size();
        // SAFETY: The source is a live map from the constructor and remains reachable while
        // copied.
        let mut copy = unsafe { willow_map_copy(map) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut copy) };
        crate::gc::set_gc_stress_for_test(None);
        // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
        assert_eq!(unsafe { willow_map_get(copy, 1, 0, 1) }, young);
        assert_eq!(
            crate::gc::willow_gc_remembered_set_size(),
            remembered + 1,
            "the copy joined the remembered set"
        );

        crate::gc::minor_collect_internal();

        // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
        let moved = unsafe { willow_map_get(map, 1, 0, 1) };
        assert_ne!(moved, young, "the young value was evacuated");
        // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
        assert_eq!(unsafe { willow_map_get(copy, 1, 0, 1) }, moved);
        // SAFETY: The test allocates a full i64 in word-aligned GC storage and reads the live
        // value through the map or root.
        assert_eq!(unsafe { *(moved as *const i64) }, 77);
        // The copy is independent: dropping the source keeps the value alive
        // through the copy alone.
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(2) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut copy) };
        crate::gc::willow_gc_collect();
        assert_eq!(
            // SAFETY: The test allocates a full i64 in word-aligned GC storage and reads the live
            // value through the map or root.
            unsafe { *(willow_map_get(copy, 1, 0, 1) as *const i64) },
            77
        );
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
        // `tls` lives on this stack and is registered with the heap; drop the
        // registration before it goes out of scope.
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
    }

    /// A full collection inside the copy's allocation (non-moving) keeps every
    /// value: the only thing keeping them alive at that moment is the rooted
    /// source map.
    #[test]
    fn copy_keeps_string_values_across_a_full_collection_in_its_allocation() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let mut map = willow_map_new(3, 3, 1);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut map) };
        for (key, value) in [("a", "alpha"), ("b", "beta"), ("c", "gamma")] {
            let mut key = willow_string_from_str(key);
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut key) };
            let value = willow_string_from_str(value);
            // SAFETY: The constructor and reference flags agree. Each pointer word comes from
            // a live fixture object at the call.
            unsafe { willow_map_insert(map, key as i64, 1, value as i64, 1) };
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
        }
        let majors = crate::gc::willow_gc_major_collections();
        crate::gc::set_gc_stress_for_test(Some("alloc"));
        // SAFETY: The source is a live map from the constructor and remains reachable while
        // copied.
        let mut copy = unsafe { willow_map_copy(map) };
        crate::gc::set_gc_stress_for_test(None);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut copy) };
        assert!(crate::gc::willow_gc_major_collections() > majors);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(2) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut copy) };
        crate::gc::willow_gc_collect();
        for (key, value) in [("a", "alpha"), ("b", "beta"), ("c", "gamma")] {
            let mut key = willow_string_from_str(key);
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut key) };
            // SAFETY: The key matches the live map layout; any string key is current through
            // the lookup.
            let got = unsafe { willow_map_get(copy, key as i64, 1, 1) };
            // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
            assert_eq!(unsafe { willow_string_as_str(got) }, value);
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
        }
        // SAFETY: The handle is a live map from this fixture or its frozen copy.
        assert_eq!(unsafe { willow_map_len(copy) }, 3);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
    }

    /// a read-only lookup must not copy the key. Counted per
    /// thread through the test binary's counting global allocator, after all
    /// setup allocations, so the numbers are this loop's own Rust allocation
    /// traffic and nothing else's.
    #[test]
    fn read_only_lookups_never_copy_the_key() {
        use crate::scheduler::scaling_measurements::counting_allocator as counter;
        const CALLS: usize = 1_000;
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let mut map = willow_map_new(3, 0, 0);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut map) };
        let mut get_bytes_by_len: Vec<(usize, usize)> = Vec::new();
        for len in [8usize, 64, 1024] {
            let mut hit = willow_string_from_str(&"k".repeat(len));
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut hit) };
            // SAFETY: The constructor and reference flags agree. Each pointer word comes from
            // a live fixture object at the call.
            unsafe { willow_map_insert(map, hit as i64, 1, len as i64, 0) };
            let mut miss = willow_string_from_str(&"m".repeat(len));
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut miss) };

            // `contains` returns a scalar, so the whole call is the lookup:
            // hits and misses alike must allocate nothing at any key length.
            let allocs = counter::thread_allocations();
            let bytes = counter::thread_bytes();
            for _ in 0..CALLS {
                // SAFETY: The key matches the live map layout; any string key is current
                // through the lookup.
                unsafe {
                    assert_eq!(willow_map_contains(map, hit as i64, 1), 1);
                    assert_eq!(willow_map_contains(map, miss as i64, 1), 0);
                }
            }
            assert_eq!(
                (
                    counter::thread_allocations() - allocs,
                    counter::thread_bytes() - bytes
                ),
                (0, 0),
                "contains allocated at key length {len}"
            );

            // The same for `get`'s key lookup with its required `Option`
            // output excluded: borrow the key, probe the table, drop it.
            // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
            // constructor.
            let data = unsafe { map_data(map) };
            let allocs = counter::thread_allocations();
            let bytes = counter::thread_bytes();
            for _ in 0..CALLS {
                // SAFETY: The map's declared key kind determines whether the word contains a
                // retained string or a scalar.
                let key = unsafe { key_from_word(hit as i64, 3) }.unwrap();
                assert_eq!(data.get(&key), Some(len as i64));
                // SAFETY: The map's declared key kind determines whether the word contains a
                // retained string or a scalar.
                let key = unsafe { key_from_word(miss as i64, 3) }.unwrap();
                assert_eq!(data.get(&key), None);
            }
            assert_eq!(
                (
                    counter::thread_allocations() - allocs,
                    counter::thread_bytes() - bytes
                ),
                (0, 0),
                "get key lookup allocated at key length {len}"
            );
            drop(data);

            // The public `get` still allocates its `Option` in the GC heap,
            // which in turn asks Rust for space. That stays under one Rust
            // allocation per call, so no per-call owned key hides in it.
            let allocs = counter::thread_allocations();
            let bytes = counter::thread_bytes();
            for _ in 0..CALLS {
                assert_eq!(
                    // SAFETY: The key matches the live map layout; any string key is current
                    // through the lookup.
                    opt_payload(unsafe { willow_map_get(map, hit as i64, 1, 0) }),
                    len as i64
                );
                assert_eq!(
                    // SAFETY: The key matches the live map layout; any string key is current
                    // through the lookup.
                    opt_tag(unsafe { willow_map_get(map, miss as i64, 1, 0) }),
                    1
                );
            }
            let get_allocs = counter::thread_allocations() - allocs;
            let get_bytes = counter::thread_bytes() - bytes;
            assert!(
                get_allocs < 2 * CALLS,
                "get allocated {get_allocs} times for {} calls at key length {len}",
                2 * CALLS
            );
            get_bytes_by_len.push((len, get_bytes));
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(2) };
        }
        // Copying each looked-up key would put `calls * len` bytes through the
        // allocator; the GC's own bookkeeping grows with the live heap instead,
        // so the totals stay far below that and the growth between the
        // shortest and longest key does too.
        let (long_len, long_bytes) = *get_bytes_by_len.last().expect("one length");
        assert!(
            long_bytes < 2 * CALLS * long_len,
            "{long_bytes} bytes for {} lookups of a {long_len}-byte key",
            2 * CALLS
        );
        let (short_len, short_bytes) = get_bytes_by_len[0];
        // Compare without subtracting: GC bookkeeping can make the short run
        // allocate slightly more than the long one.
        assert!(
            long_bytes < short_bytes + CALLS * (long_len - short_len),
            "get bytes went {short_bytes} -> {long_bytes} from key length {short_len} to {long_len}"
        );
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
    }
}

#[cfg(test)]
mod native_accounting_tests {
    use super::*;
    use crate::gc::*;

    #[test]
    fn string_keys_growth_updates_copy_telemetry_and_finalizers() {
        let _guard = isolated_runtime();
        for count in [1, 32, 1024] {
            // SAFETY: The test controls its isolated runtime at this reset boundary; no other
            // mutator or subsequent access relies on the discarded heap.
            unsafe { willow_gc_init() };
            let baseline = crate::gc_telemetry::external_bytes();
            let mut map = willow_map_new(3, 0, 0);
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut map) };
            assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
            let before = crate::gc_telemetry::snapshot();
            let mut key_bytes = 0;
            for index in 0..count {
                let text = format!("{index:08}-{}", "x".repeat(256));
                key_bytes += text.len() as u64;
                let mut key = crate::string::willow_string_from_str(&text);
                // SAFETY: The initialized slot stays at this address until its registration
                // is popped; collection may rewrite it there.
                unsafe { willow_push_root(&raw mut key) };
                // SAFETY: The constructor and reference flags agree. Each pointer word comes
                // from a live fixture object at the call.
                unsafe { willow_map_insert(map, key as i64, 1, index, 0) };
                let charged = crate::gc_telemetry::external_bytes();
                // SAFETY: The constructor and reference flags agree. Each pointer word comes
                // from a live fixture object at the call.
                unsafe { willow_map_insert(map, key as i64, 1, index + 1, 0) };
                assert_eq!(
                    crate::gc_telemetry::external_bytes(),
                    charged,
                    "replacement grew native storage"
                );
                // SAFETY: The removed shadow-stack entries belong to this operation and their
                // slot storage is still live.
                unsafe { willow_pop_root() };
            }
            let bytes = crate::gc_telemetry::external_bytes() - baseline;
            assert!(bytes >= key_bytes);
            let stats = crate::gc_telemetry::snapshot();
            assert!(stats.heap.reserved_bytes >= before.heap.reserved_bytes);
            assert_eq!(
                crate::gc_telemetry::native_memory_snapshot().requested_bytes,
                baseline + bytes
            );
            assert_eq!(
                stats.heap.reserved_bytes,
                stats.heap.old_reserved_bytes + stats.heap.nursery_reserved_bytes
            );
            willow_gc_collect();
            assert_eq!(crate::gc_telemetry::external_bytes(), baseline + bytes);
            // SAFETY: The source is a live map from the constructor and remains reachable
            // while copied.
            let mut copy = unsafe { willow_map_copy(map) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut copy) };
            assert!(crate::gc_telemetry::external_bytes() >= baseline + bytes + key_bytes);
            // SAFETY: The handle is a live map from this fixture or its frozen copy.
            assert_eq!(unsafe { willow_map_len(copy) }, count);
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(2) };
            willow_gc_collect();
            assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
            println!(
                "native map entries={count} owned_key_bytes={key_bytes} charged_bytes={bytes}"
            );
        }
    }

    #[test]
    fn reset_releases_native_map_storage() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let baseline = crate::gc_telemetry::external_bytes();
        let map = willow_map_new(0, 0, 0);
        for key in 0..128 {
            // SAFETY: This live map was constructed with scalar key and value slots.
            unsafe { willow_map_insert(map, key, 0, key, 0) };
        }
        assert!(crate::gc_telemetry::external_bytes() > baseline);
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
    }
}

#[cfg(test)]
mod iteration_tests {
    use super::*;
    use crate::gc::*;

    #[test]
    fn remove_reuses_slots_and_snapshots_sort_all_storage_paths() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        for confined in [false, true] {
            for string_key in [false, true] {
                let map =
                    GcRootHandle::new(map_new(if string_key { 3 } else { 0 }, 0, 0, confined));
                for count in [8, 32, 128, 512] {
                    for round in 0..4 {
                        for key in (0..count).rev() {
                            let word = if string_key {
                                crate::string::willow_string_from_str(&format!("{key:04}")) as i64
                            } else {
                                key
                            };
                            // SAFETY: The constructor and reference flags agree. Each pointer
                            // word comes from a live fixture object at the call.
                            unsafe {
                                willow_map_insert(map.load(), word, i64::from(string_key), key, 0)
                            };
                        }
                        // SAFETY: The root supplies the current map; this snapshot selects
                        // ordinary single-word values.
                        let snapshot =
                            GcRootHandle::new(unsafe { willow_map_snapshot(map.load(), 1, 0) });
                        for key in 0..count {
                            assert_eq!(
                                // SAFETY: The root supplies the current live array for this
                                // access.
                                unsafe { crate::array::willow_array_get(snapshot.load(), key) },
                                key
                            );
                            let word = if string_key {
                                crate::string::willow_string_from_str(&format!("{key:04}")) as i64
                            } else {
                                key
                            };
                            let mut out = -1;
                            assert_eq!(
                                // SAFETY: The fixture supplies the live map and a key of its
                                // declared kind; the output borrows a disjoint local i64.
                                unsafe {
                                    willow_map_remove_into(
                                        map.load(),
                                        word,
                                        i64::from(string_key),
                                        &mut out,
                                    )
                                },
                                0
                            );
                            assert_eq!(out, key);
                            assert_eq!(
                                // SAFETY: The fixture supplies the live map and a key of its
                                // declared kind; the output borrows a disjoint local i64.
                                unsafe {
                                    willow_map_remove_into(
                                        map.load(),
                                        word,
                                        i64::from(string_key),
                                        &mut out,
                                    )
                                },
                                1
                            );
                            assert_eq!(out, 0);
                        }
                        // SAFETY: The handle is a live map from this fixture or its frozen
                        // copy.
                        assert_eq!(unsafe { willow_map_len(map.load()) }, 0);
                        if string_key {
                            // SAFETY: The handle identifies a live, GC word-aligned MapStorage
                            // initialized by the constructor.
                            let data = unsafe { map_data(map.load()) };
                            assert!(data.values.is_empty(), "round {round}");
                            assert_eq!(data.key_bytes, 0);
                            assert!(data.values.iter().all(|v| *v == 0));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn removed_holes_copy_and_reinsert_are_independent() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let map = GcRootHandle::new(willow_map_new(3, 0, 0));
        for text in ["a", "b", "c"] {
            let key = crate::string::willow_string_from_str(text);
            // SAFETY: The constructor and reference flags agree. Each pointer word comes from
            // a live fixture object at the call.
            unsafe { willow_map_insert(map.load(), key as i64, 1, 7, 0) };
        }
        let key = GcRootHandle::new(crate::string::willow_string_from_str("b"));
        let mut out = 0;
        // SAFETY: The fixture supplies the live map and a key of its declared kind; the
        // output borrows a disjoint local i64.
        unsafe { willow_map_remove_into(map.load(), key.load() as i64, 1, &mut out) };
        // SAFETY: The source is a live map from the constructor and remains reachable while
        // copied.
        let copy = GcRootHandle::new(unsafe { willow_map_copy(map.load()) });
        // SAFETY: The constructor and reference flags agree. Each pointer word comes from a
        // live fixture object at the call.
        unsafe { willow_map_insert(copy.load(), key.load() as i64, 1, 9, 0) };
        assert_eq!(
            // SAFETY: The key matches the live map layout; any string key is current through the
            // lookup.
            unsafe { willow_map_contains(map.load(), key.load() as i64, 1) },
            0
        );
        assert_eq!(
            // SAFETY: The fixture supplies the live map and a key of its declared kind; the
            // output borrows a disjoint local i64.
            unsafe { willow_map_get_into(copy.load(), key.load() as i64, 1, &mut out) },
            0
        );
        assert_eq!(out, 9);
    }

    #[test]
    fn snapshot_comparison_and_compaction_counts_scale() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        for count in [64_i64, 256, 1024, 4096] {
            let map = GcRootHandle::new(willow_map_new(0, 3, 1));
            let value = GcRootHandle::new(crate::string::willow_string_from_str("live"));
            for key in 0..count {
                // SAFETY: The constructor and reference flags agree. Each pointer word comes
                // from a live fixture object at the call.
                unsafe { willow_map_insert(map.load(), key, 0, value.load() as i64, 1) };
            }
            SNAPSHOT_COMPARISONS.with(|n| n.set(0));
            // SAFETY: The root supplies the current map; this snapshot selects ordinary
            // single-word values.
            let snapshot = GcRootHandle::new(unsafe { willow_map_snapshot(map.load(), 0, 0) });
            assert_eq!(
                // SAFETY: The root supplies the current live array for this access.
                unsafe { crate::array::willow_array_len(snapshot.load()) },
                count
            );
            let comparisons = SNAPSHOT_COMPARISONS.with(|n| n.get());
            assert!(comparisons < 4 * count as usize * count.ilog2() as usize);
            COMPACT_MOVES.with(|n| n.set(0));
            for key in 0..count {
                let mut out = 0;
                // SAFETY: The fixture supplies the live map and a key of its declared kind;
                // the output borrows a disjoint local i64.
                unsafe { willow_map_remove_into(map.load(), key, 0, &mut out) };
            }
            let moved = COMPACT_MOVES.with(|n| n.get());
            assert!(moved < count as usize);
            // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
            // constructor.
            assert!(unsafe { map_data(map.load()) }.values.is_empty());
            println!("entries={count} comparisons={comparisons} compact_moves={moved}");
        }
    }

    #[test]
    fn remove_barrier_clears_reference_slots_and_snapshot_roots_survive_gc() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let map = GcRootHandle::new(willow_map_new(0, 3, 1));
        for key in 0..128 {
            let value = crate::string::willow_string_from_str(&format!("value-{key}"));
            // SAFETY: The constructor and reference flags agree. Each pointer word comes from
            // a live fixture object at the call.
            unsafe { willow_map_insert(map.load(), key, 0, value as i64, 1) };
        }
        // SAFETY: The root supplies the current map; this snapshot selects ordinary single-
        // word values.
        let array = GcRootHandle::new(unsafe { willow_map_snapshot(map.load(), 1, 0) });
        for key in 0..128 {
            let mut out = 0;
            assert_eq!(
                // SAFETY: The fixture supplies the live map and a key of its declared kind; the
                // output borrows a disjoint local i64.
                unsafe { willow_map_remove_into(map.load(), key, 0, &mut out) },
                0
            );
        }
        let mut children = Vec::new();
        // SAFETY: The callback receives the live map payload selected by its registered layout.
        unsafe {
            snapshot_map(map.load(), &mut children);
        }
        assert!(children.iter().all(|child| child.is_null()));
        willow_gc_collect();
        for key in 0..128 {
            // SAFETY: The root supplies the current live array for this access.
            let value = unsafe { crate::array::willow_array_get(array.load(), key) };
            assert_eq!(
                // SAFETY: The operand is null or a live string, and no safepoint intervenes in
                // this borrow.
                unsafe { willow_string_as_bytes(value as *const u8) },
                format!("value-{key}").as_bytes()
            );
        }
    }
}
