//! Completed major sweep work; includes foreground and allocation-assisted work.
//! Counts visits (including live objects); time includes waits, drops and release.
use std::sync::Mutex;
#[derive(Clone, Copy, Default, Debug)]
pub struct Stats {
    pub objects: u64,
    pub bytes: u64,
    pub wall_ns: u64,
    pub reclaimed_bytes: u64,
}
static STATS: Mutex<Stats> = Mutex::new(Stats {
    objects: 0,
    bytes: 0,
    wall_ns: 0,
    reclaimed_bytes: 0,
});
pub fn snapshot() -> Stats {
    *STATS.lock().unwrap_or_else(|p| p.into_inner())
}
pub(crate) fn reset() {
    *STATS.lock().unwrap_or_else(|p| p.into_inner()) = Stats::default();
}
pub(crate) fn completed(objects: u64, bytes: u64, wall_ns: u64, reclaimed_bytes: u64) {
    let mut s = STATS.lock().unwrap_or_else(|p| p.into_inner());
    s.objects = s.objects.saturating_add(objects);
    s.bytes = s.bytes.saturating_add(bytes);
    s.wall_ns = s.wall_ns.saturating_add(wall_ns);
    s.reclaimed_bytes = s.reclaimed_bytes.saturating_add(reclaimed_bytes);
}
