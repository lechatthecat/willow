use std::cell::RefCell;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeFrame {
    pub function: String,
    pub file: String,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeStackTrace {
    frames: Vec<RuntimeFrame>,
}

impl RuntimeStackTrace {
    pub fn push(&mut self, frame: RuntimeFrame) {
        self.frames.push(frame);
    }

    pub fn pop(&mut self) -> Option<RuntimeFrame> {
        self.frames.pop()
    }

    pub fn frames(&self) -> &[RuntimeFrame] {
        &self.frames
    }
}

thread_local! {
    /// Per-thread call stack maintained by debug builds: the compiler emits a
    /// `willow_callstack_push` before each user-function call and a matching
    /// `willow_callstack_pop` after it returns. When a call never returns (it
    /// panics/aborts), its frames remain on the stack and the panic handlers
    /// print the chain (willow-992h).
    static CALL_STACK: RefCell<RuntimeStackTrace> = RefCell::new(RuntimeStackTrace::default());
}

/// Read `len` raw UTF-8 bytes at `ptr` into an owned (Rust-heap) String. Used
/// for call-frame names/paths so the call stack never allocates on the Willow
/// GC heap (which would pollute `gc_allocated_bytes`).
unsafe fn raw_str(ptr: *const u8, len: i64) -> String {
    if ptr.is_null() || len <= 0 {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// Push a call frame. `function`/`file` are pointers to raw static UTF-8 bytes
/// (NOT WillowStrings) with explicit lengths, copied onto the Rust heap so the
/// debug call stack does not allocate on the Willow GC heap (willow-992h).
#[unsafe(no_mangle)]
pub extern "C" fn willow_callstack_push(
    function: *const u8,
    function_len: i64,
    file: *const u8,
    file_len: i64,
    line: i32,
    col: i32,
) {
    let function = unsafe { raw_str(function, function_len) };
    let file = unsafe { raw_str(file, file_len) };
    CALL_STACK.with(|s| {
        s.borrow_mut().push(RuntimeFrame {
            function,
            file,
            line: line as usize,
            col: col as usize,
        });
    });
}

/// Pop the most recent call frame (matched with a successful return).
#[unsafe(no_mangle)]
pub extern "C" fn willow_callstack_pop() {
    CALL_STACK.with(|s| {
        s.borrow_mut().pop();
    });
}

/// Render the current call chain (most recent call first). Returns the empty
/// string when no frames are recorded (e.g. a panic directly in `main`).
pub fn current_call_stack_text() -> String {
    CALL_STACK.with(|s| {
        let trace = s.borrow();
        let frames = trace.frames();
        if frames.is_empty() {
            return String::new();
        }
        let mut out = String::from("call stack (most recent call first):");
        for (i, frame) in frames.iter().rev().enumerate() {
            out.push_str(&format!(
                "\n  {i}: {} at {}:{}:{}",
                frame.function, frame.file, frame.line, frame.col
            ));
        }
        out
    })
}

/// Print the current call chain to stderr (debug panic/abort handlers).
pub fn print_current_call_stack() {
    let text = current_call_stack_text();
    if !text.is_empty() {
        eprintln!("{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_trace_records_source_frames() {
        let mut trace = RuntimeStackTrace::default();
        trace.push(RuntimeFrame {
            function: "main".to_string(),
            file: "main.wi".to_string(),
            line: 3,
            col: 5,
        });
        assert_eq!(trace.frames()[0].function, "main");
        assert_eq!(trace.pop().unwrap().line, 3);
    }

    #[test]
    fn callstack_text_orders_most_recent_first() {
        fn push(name: &str, file: &str, line: i32, col: i32) {
            willow_callstack_push(
                name.as_ptr(),
                name.len() as i64,
                file.as_ptr(),
                file.len() as i64,
                line,
                col,
            );
        }
        // main called helper (line 8), helper called deeper (line 5).
        push("helper", "main.wi", 8, 5);
        push("deeper", "main.wi", 5, 5);
        let text = current_call_stack_text();
        // Most recent call (deeper) is frame 0.
        let f0 = text.find("0: deeper at main.wi:5:5").unwrap();
        let f1 = text.find("1: helper at main.wi:8:5").unwrap();
        assert!(f0 < f1, "ordering wrong: {text}");
        willow_callstack_pop();
        willow_callstack_pop();
        assert_eq!(current_call_stack_text(), "");
    }

    #[test]
    fn callstack_empty_renders_nothing() {
        // A fresh stack (no frames) renders the empty string.
        assert_eq!(current_call_stack_text(), "");
    }
}
