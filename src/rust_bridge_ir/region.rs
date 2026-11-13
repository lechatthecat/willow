// Included into generated bridge crates, not compiled as a compiler module.
unsafe extern "C" {
    fn willow_rust_bridge_region_enter() -> u32;
    fn willow_rust_bridge_region_exit(region: u32);
}
/// Collections proceed while the foreign body runs, however long it blocks.
/// Dropping also leaves the region when the body panics.
#[allow(dead_code, reason = "Shared bridge region support may be unused by an adapter")]
struct BridgeRegion(u32);
#[allow(dead_code, reason = "Shared bridge region support may be unused by an adapter")]
impl BridgeRegion {
    fn enter() -> Self {
        Self(unsafe { willow_rust_bridge_region_enter() })
    }
}
impl Drop for BridgeRegion {
    fn drop(&mut self) {
        unsafe { willow_rust_bridge_region_exit(self.0) }
    }
}
