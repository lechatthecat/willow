unsafe extern "C" {
    fn willow_rust_bridge_panic_message(bytes: *const u8, len: usize) -> !;
}
fn bridge_panic(payload: Box<dyn std::any::Any + Send>) -> ! {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic payload");
    // Fatal runtime failure: do not execute a user-defined payload destructor.
    unsafe { willow_rust_bridge_panic_message(message.as_ptr(), message.len()) }
}
