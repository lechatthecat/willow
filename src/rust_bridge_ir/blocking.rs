// Shared native-only transport, included into generated bridge crates.
unsafe extern "C" {
    fn willow_rust_bridge_run(context: usize, invoke: unsafe extern "C" fn(usize, u32) -> usize, dispose: unsafe extern "C" fn(usize)) -> usize;
    fn willow_rust_bridge_fail(bytes: *const u8, len: usize);
}
fn blocking_message(payload: Box<dyn std::any::Any + Send>) -> String {
    let message = payload.downcast_ref::<String>().cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "non-string panic payload".to_owned());
    // Drain secondary payloads iteratively: a panicking destructor must not
    // unwind through the ABI, leak its replacement payload, or grow the stack.
    let mut pending = Some(payload);
    while let Some(payload) = pending.take() {
        pending = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload))).err();
    }
    message
}
fn blocking_call<T: Send + 'static, F: FnOnce() -> T + Send + 'static>(work: F) -> Option<T> {
    unsafe extern "C" fn invoke<T: Send + 'static, F: FnOnce() -> T + Send + 'static>(raw: usize, cancelled: u32) -> usize {
        let _region = BridgeRegion::enter();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let work = unsafe { Box::from_raw(raw as *mut F) };
            if cancelled != 0 { drop(work); None } else { Some(work()) }
        })).map_err(blocking_message);
        Box::into_raw(Box::new(outcome)) as usize
    }
    unsafe extern "C" fn dispose<T>(raw: usize) {
        if raw != 0 {
            let _region = BridgeRegion::enter();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                unsafe { drop(Box::from_raw(raw as *mut Result<Option<T>, String>)); }
            }));
            if let Err(payload) = outcome { drop(blocking_message(payload)); }
        }
    }
    let raw = unsafe { willow_rust_bridge_run(Box::into_raw(Box::new(work)) as usize, invoke::<T, F>, dispose::<T>) };
    if raw == 0 { return None; }
    let outcome = unsafe { *Box::from_raw(raw as *mut Result<Option<T>, String>) };
    match outcome {
        Ok(value) => value,
        Err(message) => {
            unsafe { willow_rust_bridge_fail(message.as_ptr(), message.len()); }
            None
        }
    }
}
