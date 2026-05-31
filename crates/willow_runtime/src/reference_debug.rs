use crate::string::willow_string_as_str;
use std::cell::RefCell;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceCallContext {
    pub file: String,
    pub line: i32,
    pub col: i32,
    pub callee: String,
    pub param: String,
    pub param_type: String,
    pub mode: String,
    pub place_kind: String,
    pub place_name: String,
}

std::thread_local! {
    static CURRENT_REFERENCE_CALL: RefCell<Option<ReferenceCallContext>> =
        const { RefCell::new(None) };
}

fn ws(ptr: *const u8) -> String {
    unsafe { willow_string_as_str(ptr) }.to_string()
}

pub fn current_reference_call() -> Option<ReferenceCallContext> {
    CURRENT_REFERENCE_CALL.with(|current| current.borrow().clone())
}

pub fn clear_current_reference_call() {
    CURRENT_REFERENCE_CALL.with(|current| {
        *current.borrow_mut() = None;
    });
}

pub fn reference_call_context_text(ctx: &ReferenceCallContext) -> String {
    format!(
        "  reference call: {} parameter `{}` {} {} at {}:{}:{} using {} `{}`",
        ctx.callee,
        ctx.param,
        ctx.mode,
        ctx.param_type,
        ctx.file,
        ctx.line,
        ctx.col,
        ctx.place_kind,
        ctx.place_name
    )
}

pub fn print_current_reference_call_context() {
    if let Some(ctx) = current_reference_call() {
        eprintln!("{}", reference_call_context_text(&ctx));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_debug_reference_call(
    file: *const u8,
    line: i32,
    col: i32,
    callee: *const u8,
    param: *const u8,
    param_type: *const u8,
    mode: *const u8,
    place_kind: *const u8,
    place_name: *const u8,
) {
    CURRENT_REFERENCE_CALL.with(|current| {
        *current.borrow_mut() = Some(ReferenceCallContext {
            file: ws(file),
            line,
            col,
            callee: ws(callee),
            param: ws(param),
            param_type: ws(param_type),
            mode: ws(mode),
            place_kind: ws(place_kind),
            place_name: ws(place_name),
        });
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_debug_reference_call_clear() {
    clear_current_reference_call();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{runtime_test_guard, willow_gc_init};
    use crate::string::willow_string_alloc;

    fn ws_ptr(s: &str) -> *mut u8 {
        willow_string_alloc(s.as_bytes().as_ptr(), s.len() as i64)
    }

    #[test]
    fn reference_debug_unit_01_records_and_clears_reference_context() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        willow_debug_reference_call(
            ws_ptr("main.wi"),
            7,
            13,
            ws_ptr("increment"),
            ws_ptr("x"),
            ws_ptr("i64"),
            ws_ptr("&mut"),
            ws_ptr("local"),
            ws_ptr("n"),
        );

        let ctx = current_reference_call().expect("reference context should be recorded");
        assert_eq!(ctx.callee, "increment");
        assert_eq!(ctx.mode, "&mut");
        assert_eq!(
            reference_call_context_text(&ctx),
            "  reference call: increment parameter `x` &mut i64 at main.wi:7:13 using local `n`"
        );

        willow_debug_reference_call_clear();
        assert!(current_reference_call().is_none());
    }
}
