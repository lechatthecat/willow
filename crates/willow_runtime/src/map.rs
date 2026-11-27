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
mod tests;

#[cfg(test)]
mod native_accounting_tests;

#[cfg(test)]
mod iteration_tests;
