#[unsafe(no_mangle)]
pub extern "C" fn willow_bridge_answer() -> i32 { willow_bridge_native::answer() }
