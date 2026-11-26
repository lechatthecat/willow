use std::cell::{Cell, RefCell};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeFrame {
    pub function: String,
    pub file: String,
    pub line: usize,
    pub col: usize,
    /// A prepared method call is hidden until its arguments have completed.
    pub active: bool,
}

/// Location published by `willow_trace_site_set`. The bytes are the generated
/// program's static string data, so a site is recorded without copying or
/// allocating; text is materialized only when a diagnostic is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TraceSite {
    function: &'static [u8],
    file: &'static [u8],
    line: usize,
    col: usize,
}

impl TraceSite {
    /// # Safety
    /// Non-null pointers must reference `len` bytes that live for the rest of
    /// the process (generated static data).
    unsafe fn from_static(
        function: *const u8,
        function_len: i64,
        file: *const u8,
        file_len: i64,
        line: i64,
        col: i64,
    ) -> Self {
        Self {
            // SAFETY: The generated call-site records supply process-lifetime byte strings with
            // their lengths. The helper handles absent strings.
            function: unsafe { static_bytes(function, function_len) },
            // SAFETY: The generated call-site records supply process-lifetime byte strings with
            // their lengths. The helper handles absent strings.
            file: unsafe { static_bytes(file, file_len) },
            line: line.max(0) as usize,
            col: col.max(0) as usize,
        }
    }

    pub(crate) fn file(&self) -> &'static [u8] {
        self.file
    }

    pub(crate) fn line(&self) -> usize {
        self.line
    }

    pub(crate) fn col(&self) -> usize {
        self.col
    }

    fn function_name(&self) -> std::borrow::Cow<'static, str> {
        String::from_utf8_lossy(self.function)
    }

    pub(crate) fn file_name(&self) -> std::borrow::Cow<'static, str> {
        String::from_utf8_lossy(self.file)
    }
}

/// Last known location of a task, read when another task renders its chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExecutionSite {
    /// Published by generated code (copied from the thread-local site when
    /// the task registers as an awaiter).
    Published(TraceSite),
    /// An explicit panic location that superseded the published site.
    Panic(RuntimeFrame),
}

impl ExecutionSite {
    pub(crate) fn location_text(&self) -> String {
        match self {
            Self::Published(site) => {
                format!(" at {}:{}:{}", site.file_name(), site.line, site.col)
            }
            Self::Panic(frame) => format!(" at {}:{}:{}", frame.file, frame.line, frame.col),
        }
    }
}

/// # Safety
/// See [`TraceSite::from_static`].
unsafe fn static_bytes(ptr: *const u8, len: i64) -> &'static [u8] {
    if ptr.is_null() || len <= 0 {
        return &[];
    }
    // SAFETY: The generated call-site records supply process-lifetime byte strings with their
    // lengths. The helper handles absent strings.
    unsafe { std::slice::from_raw_parts(ptr, len as usize) }
}

/// A suspended caller on the call-trace stack. It borrows the generated
/// program's static name and path bytes, so pushing a frame on every call
/// neither allocates nor copies; text is materialized only when rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallFrame {
    site: TraceSite,
    /// A prepared method call is hidden until its arguments have completed.
    active: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeStackTrace {
    frames: Vec<CallFrame>,
    /// Debug-build leaf: the executing function and its current statement.
    current: Option<TraceSite>,
    current_async: bool,
}

/// Diagnostics of the async task last polled on this thread: its async fn name
/// (tagged at every poll entry) and latest published site. They are kept here
/// rather than in the shared task table so publishing costs no lock or
/// allocation on the hot path. The owner id keeps another task
/// polled on this worker from reading them, and the cell is deliberately not
/// part of the native-stack trace exchange, so the scheduler can still read
/// them after a poll returns. A task copies them into its record when it
/// registers as an awaiter, where another task's panic chain reads them.
#[derive(Debug, Clone, Copy)]
struct AsyncTaskTrace {
    task: crate::task::RuntimeTaskId,
    name: Option<&'static [u8]>,
    site: Option<TraceSite>,
    /// `site` is a release call into another Willow function, which publishes
    /// no site of its own, so a fault reported here may lie inside that call.
    calling: bool,
}

thread_local! {
    static ASYNC_TRACE: Cell<Option<AsyncTaskTrace>> = const { Cell::new(None) };
}

fn update_async_trace(task: crate::task::RuntimeTaskId, update: impl FnOnce(&mut AsyncTaskTrace)) {
    ASYNC_TRACE.with(|slot| {
        let mut trace = match slot.get() {
            Some(trace) if trace.task == task => trace,
            _ => AsyncTaskTrace {
                task,
                name: None,
                site: None,
                calling: false,
            },
        };
        update(&mut trace);
        slot.set(Some(trace));
    });
}

/// Tag `task` with the static name of the async fn whose poll it entered.
///
/// # Safety
/// `name` must reference `len` bytes of process-lifetime static data.
pub(crate) unsafe fn tag_async_task(task: crate::task::RuntimeTaskId, name: *const u8, len: i64) {
    // SAFETY: The generated call-site records supply process-lifetime byte strings with their
    // lengths. The helper handles absent strings.
    let name = unsafe { static_bytes(name, len) };
    update_async_trace(task, |trace| trace.name = Some(name));
}

/// Whether the site `task` last published on this thread is a release call
/// whose callee published no site of its own.
pub(crate) fn async_trace_calls(task: crate::task::RuntimeTaskId) -> bool {
    matches!(ASYNC_TRACE.with(Cell::get), Some(trace) if trace.task == task && trace.calling)
}

/// The name and site `task` last published on this thread.
pub(crate) fn async_trace_of(
    task: crate::task::RuntimeTaskId,
) -> (Option<std::borrow::Cow<'static, str>>, Option<TraceSite>) {
    match ASYNC_TRACE.with(Cell::get) {
        Some(trace) if trace.task == task => (trace.name.map(String::from_utf8_lossy), trace.site),
        _ => (None, None),
    }
}

impl RuntimeStackTrace {
    fn push(&mut self, site: TraceSite) {
        self.frames.push(CallFrame { site, active: true });
    }

    fn pop(&mut self) -> Option<CallFrame> {
        self.frames.pop()
    }

    /// Active frames, most recent call first, as `(function, file, line, col)`.
    fn active_frames(
        &self,
    ) -> impl Iterator<
        Item = (
            std::borrow::Cow<'static, str>,
            std::borrow::Cow<'static, str>,
            usize,
            usize,
        ),
    > + '_ {
        self.frames
            .iter()
            .rev()
            .filter(|frame| frame.active)
            .map(|frame| {
                (
                    frame.site.function_name(),
                    frame.site.file_name(),
                    frame.site.line,
                    frame.site.col,
                )
            })
    }
}

thread_local! {
    /// Per-thread call stack maintained by debug and `--debug-info` builds: the compiler emits a
    /// `willow_callstack_push` with the caller name/location before each call and a matching
    /// `willow_callstack_pop` after it returns. When a call never returns (it
    /// panics/aborts), its frames remain on the stack and the panic handlers
    /// print the chain.
    static CALL_STACK: RefCell<RuntimeStackTrace> = RefCell::new(RuntimeStackTrace::default());
}

/// Exchange the native call trace at a task stack suspension boundary.
pub(crate) fn replace_current(trace: RuntimeStackTrace) -> RuntimeStackTrace {
    CALL_STACK.with(|slot| std::mem::replace(&mut *slot.borrow_mut(), trace))
}

pub(crate) fn snapshot_current() -> RuntimeStackTrace {
    CALL_STACK.with(|slot| slot.borrow().clone())
}

/// The generated task constructor is called inside a debug call frame.
/// Copy its location before publishing the task to other workers.
pub(crate) fn current_call_site() -> Option<(String, u32)> {
    CALL_STACK.with(|slot| {
        slot.borrow()
            .frames
            .last()
            .map(|frame| (frame.site.file_name().into_owned(), frame.site.line as u32))
    })
}

/// Push a call frame. `function`/`file` are pointers to raw static UTF-8 bytes
/// (NOT WillowStrings) with explicit lengths. The frame keeps the pointers, so
/// a call costs no allocation on either heap.
/// Each frame names the caller and a location inside that caller.
///
/// # Safety
/// For each non-null pointer with a positive length, provide that many initialized immutable
/// bytes in a single allocation at a stable address for the lifetime of the process.
/// Diagnostics retain the address.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_callstack_push(
    function: *const u8,
    function_len: i64,
    file: *const u8,
    file_len: i64,
    line: i32,
    col: i32,
) {
    // SAFETY: Generated code passes its static string data (see the ABI doc above).
    let site = unsafe {
        TraceSite::from_static(
            function,
            function_len,
            file,
            file_len,
            i64::from(line),
            i64::from(col),
        )
    };
    CALL_STACK.with(|s| s.borrow_mut().push(site));
}

/// Reserve a balanced shadow-stack slot without claiming the call has started.
///
/// # Safety
/// See [`willow_callstack_push`]: both byte ranges must live for the process.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_callstack_push_pending(
    function: *const u8,
    function_len: i64,
    file: *const u8,
    file_len: i64,
    line: i32,
    col: i32,
) {
    // SAFETY: The caller provides process-lifetime byte ranges, as the push requires.
    unsafe { willow_callstack_push(function, function_len, file, file_len, line, col) };
    CALL_STACK.with(|slot| slot.borrow_mut().frames.last_mut().unwrap().active = false);
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_callstack_activate() {
    CALL_STACK.with(|slot| {
        if let Some(frame) = slot.borrow_mut().frames.last_mut() {
            frame.active = true;
        }
    });
}

/// Publish the executing function separately from suspended caller frames.
/// Keeping it in the exchanged trace also isolates native task stacks.
/// Flags: bit 0 identifies async source code; bit 1 enables the debug call chain;
/// bit 2 marks a call into generated code. Release builds publish no fault
/// location for such a call: an async callee publishes its own, and a
/// synchronous callee's faults stay location-free.
///
/// # Safety
/// For each non-null pointer with a positive length, provide that many initialized immutable
/// bytes in a single allocation at a stable address for the lifetime of the process.
/// Diagnostics retain the address.
///
/// ```compile_fail,E0133
/// let name = b"f";
/// willow_runtime::stack_trace::willow_trace_site_set(name.as_ptr(), 1, name.as_ptr(), 1, 1, 1, 0);
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_trace_site_set(
    function: *const u8,
    function_len: i64,
    file: *const u8,
    file_len: i64,
    line: i64,
    col: i64,
    flags: i32,
) {
    let asynchronous = flags & 1 != 0;
    let debug = flags & 2 != 0;
    if !asynchronous && !debug {
        // Release synchronous code keeps runtime faults location-free; drop any site an async
        // caller published. Current
        // codegen does not publish release synchronous sites at all.
        crate::panic_context::willow_fault_site_clear();
        CALL_STACK.with(|slot| slot.borrow_mut().current = None);
        return;
    }
    // SAFETY: generated code passes its static string data (see the ABI doc).
    let site = unsafe { TraceSite::from_static(function, function_len, file, file_len, line, col) };
    let calling = flags & 4 != 0 && !debug;
    if calling {
        crate::panic_context::willow_fault_site_clear();
    } else {
        crate::panic_context::set_static_fault_site(site);
    }
    let task = if asynchronous {
        crate::scheduler::current_task()
    } else {
        None
    };
    if let Some(task) = task {
        update_async_trace(task, |trace| {
            trace.site = Some(site);
            trace.calling = calling;
        });
    }
    CALL_STACK.with(|slot| {
        let mut trace = slot.borrow_mut();
        trace.current = debug.then_some(site);
        trace.current_async = asynchronous;
    });
}

/// Capture the leaf at the actual panic site, followed by its suspended callers.
/// Explicit panic locations take precedence over the enclosing LIR statement.
pub(crate) fn panic_call_stack_text(file: &str, line: usize, col: usize) -> String {
    CALL_STACK.with(|slot| {
        let trace = slot.borrow();
        let Some(current) = trace.current else {
            drop(trace);
            return current_call_stack_text();
        };
        let current_async = trace.current_async;
        drop(trace);
        if current_async && !file.is_empty() {
            let function = current.function_name().into_owned();
            if let Some(task) = crate::scheduler::current_task() {
                // The explicit location supersedes the last published site.
                update_async_trace(task, |trace| {
                    trace.site = None;
                    trace.calling = false;
                });
            }
            crate::scheduler::set_current_execution_site(RuntimeFrame {
                function,
                file: file.to_owned(),
                line,
                col,
                active: true,
            });
        }
        let trace = slot.borrow();
        let mut out = String::from("call stack (most recent call first):");
        use std::fmt::Write;
        let (leaf_file, leaf_line, leaf_col) = if file.is_empty() {
            (current.file_name(), current.line, current.col)
        } else {
            (std::borrow::Cow::Borrowed(file), line, col)
        };
        let _ = write!(
            out,
            "\n  0: {} at {leaf_file}:{leaf_line}:{leaf_col}",
            current.function_name()
        );
        for (i, (function, file, line, col)) in trace.active_frames().enumerate() {
            let _ = write!(out, "\n  {}: {function} at {file}:{line}:{col}", i + 1);
        }
        out
    })
}

/// Pop the most recent call frame (matched with a successful return).
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_callstack_pop() {
    CALL_STACK.with(|s| {
        s.borrow_mut().pop();
    });
}

/// Render the current call chain (most recent call first). Returns the empty
/// string when no suspended callers are recorded. Panic reports prepend the leaf.
pub fn current_call_stack_text() -> String {
    CALL_STACK.with(|s| {
        let trace = s.borrow();
        let mut frames = trace.active_frames().peekable();
        if frames.peek().is_none() {
            return String::new();
        }
        use std::fmt::Write;
        let mut out = String::from("call stack (most recent call first):");
        for (i, (function, file, line, col)) in frames.enumerate() {
            let _ = write!(out, "\n  {i}: {function} at {file}:{line}:{col}");
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
    fn panic_frames_skip_unentered_calls_and_survive_stack_exchange() {
        let previous = replace_current(Default::default());
        // SAFETY: Byte literals have process lifetime.
        unsafe { willow_callstack_push(b"main".as_ptr(), 4, b"main.wi".as_ptr(), 7, 8, 5) };
        // SAFETY: Byte literals have process lifetime.
        unsafe {
            willow_callstack_push_pending(b"helper".as_ptr(), 6, b"helper.wi".as_ptr(), 9, 3, 5)
        };
        // SAFETY: The nonempty diagnostic names are byte literals with process lifetime and
        // the supplied lengths fit those literals.
        unsafe { willow_trace_site_set(b"argument".as_ptr(), 8, b"arg.wi".as_ptr(), 6, 4, 2, 2) };
        let trace = replace_current(Default::default());
        assert_eq!(panic_call_stack_text("", 0, 0), "");
        replace_current(trace);
        assert_eq!(
            panic_call_stack_text("arg.wi", 9, 3),
            "call stack (most recent call first):\n  0: argument at arg.wi:9:3\n  1: main at main.wi:8:5"
        );
        willow_callstack_activate();
        assert!(panic_call_stack_text("arg.wi", 9, 3).contains("1: helper at helper.wi:3:5"));
        willow_callstack_pop();
        willow_callstack_pop();
        replace_current(previous);
    }

    #[test]
    fn panic_frames_render_linear_activation_counts() {
        let previous = replace_current(Default::default());
        for depth in [1, 16, 128] {
            for _ in 0..depth {
                // SAFETY: The source strings cover their byte counts through the synchronous
                // copy.
                unsafe {
                    willow_callstack_push(b"recur".as_ptr(), 5, b"deep.wi".as_ptr(), 7, 3, 1)
                };
            }
            // SAFETY: The nonempty diagnostic names are byte literals with process lifetime
            // and the supplied lengths fit those literals.
            unsafe { willow_trace_site_set(b"recur".as_ptr(), 5, b"deep.wi".as_ptr(), 7, 2, 1, 2) };
            assert_eq!(snapshot_current().frames.len(), depth);
            assert_eq!(
                panic_call_stack_text("deep.wi", 2, 1).lines().count(),
                depth + 2
            );
            for _ in 0..depth {
                willow_callstack_pop();
            }
            assert!(snapshot_current().frames.is_empty());
        }
        replace_current(previous);
    }

    #[test]
    fn release_site_does_not_inherit_a_previous_debug_leaf() {
        let previous = replace_current(Default::default());
        // SAFETY: The nonempty diagnostic names are byte literals with process lifetime and
        // the supplied lengths fit those literals.
        unsafe { willow_trace_site_set(b"debug".as_ptr(), 5, b"old.wi".as_ptr(), 6, 2, 1, 2) };
        // SAFETY: The nonempty diagnostic names are byte literals with process lifetime and
        // the supplied lengths fit those literals.
        unsafe { willow_trace_site_set(b"release".as_ptr(), 7, b"new.wi".as_ptr(), 6, 4, 1, 0) };
        assert_eq!(panic_call_stack_text("new.wi", 4, 1), "");
        replace_current(previous);
    }

    /// a release async call into generated code keeps the task's
    /// own site but publishes no fault location for the callee; debug code
    /// still publishes it; and every poll boundary drops a stale location.
    #[test]
    fn release_async_willow_call_drops_only_the_fault_location() {
        use crate::panic_context::current_fault_site;
        let previous = replace_current(Default::default());
        // SAFETY: The nonempty diagnostic names are byte literals with process lifetime and
        // the supplied lengths fit those literals.
        unsafe { willow_trace_site_set(b"stale".as_ptr(), 5, b"old.wi".as_ptr(), 6, 2, 1, 2) };
        assert_eq!(current_fault_site(), ("old.wi".to_string(), 2, 1));
        crate::scheduler::with_current_task_for_test(41, || {
            assert_eq!(current_fault_site(), (String::new(), 0, 0));
            // SAFETY: The nonempty diagnostic names are byte literals with process lifetime
            // and the supplied lengths fit those literals.
            unsafe { willow_trace_site_set(b"caller".as_ptr(), 6, b"a.wi".as_ptr(), 4, 7, 3, 1) };
            assert_eq!(current_fault_site(), ("a.wi".to_string(), 7, 3));
            // SAFETY: The nonempty diagnostic names are byte literals with process lifetime
            // and the supplied lengths fit those literals.
            unsafe {
                willow_trace_site_set(b"caller".as_ptr(), 6, b"a.wi".as_ptr(), 4, 8, 5, 1 | 4)
            };
            assert_eq!(current_fault_site(), (String::new(), 0, 0));
            let site = async_trace_of(41).1.expect("task site kept");
            assert_eq!((site.line(), site.col()), (8, 5));
            assert!(async_trace_calls(41), "release call site is labelled");
            // SAFETY: The nonempty diagnostic names are byte literals with process lifetime
            // and the supplied lengths fit those literals.
            unsafe {
                willow_trace_site_set(b"caller".as_ptr(), 6, b"d.wi".as_ptr(), 4, 9, 2, 1 | 2 | 4)
            };
            assert_eq!(current_fault_site(), ("d.wi".to_string(), 9, 2));
            assert!(
                !async_trace_calls(41),
                "traced callees publish their own sites"
            );
            // SAFETY: The nonempty diagnostic names are byte literals with process lifetime
            // and the supplied lengths fit those literals.
            unsafe {
                willow_trace_site_set(b"caller".as_ptr(), 6, b"a.wi".as_ptr(), 4, 8, 5, 1 | 4)
            };
            // SAFETY: The nonempty diagnostic names are byte literals with process lifetime
            // and the supplied lengths fit those literals.
            unsafe { willow_trace_site_set(b"caller".as_ptr(), 6, b"a.wi".as_ptr(), 4, 9, 1, 1) };
            assert!(
                !async_trace_calls(41),
                "a later plain site clears the label"
            );
        });
        assert_eq!(current_fault_site(), (String::new(), 0, 0));
        replace_current(previous);
    }

    #[test]
    fn async_trace_is_owned_by_the_last_publishing_task() {
        // SAFETY: The generated call-site records supply process-lifetime byte strings with their
        // lengths. The helper handles absent strings.
        let site = unsafe { TraceSite::from_static(b"f".as_ptr(), 1, b"a.wi".as_ptr(), 4, 3, 2) };
        // SAFETY: The generated call-site records supply process-lifetime byte strings with their
        // lengths. The helper handles absent strings.
        unsafe { tag_async_task(7, b"seven".as_ptr(), 5) };
        update_async_trace(7, |trace| trace.site = Some(site));
        let (name, published) = async_trace_of(7);
        assert_eq!(name.as_deref(), Some("seven"));
        assert_eq!(published, Some(site));
        // Re-tagging the same task keeps its site; another task resets both.
        // SAFETY: The generated call-site records supply process-lifetime byte strings with their
        // lengths. The helper handles absent strings.
        unsafe { tag_async_task(7, b"seven".as_ptr(), 5) };
        assert_eq!(async_trace_of(7).1, Some(site));
        // SAFETY: The generated call-site records supply process-lifetime byte strings with their
        // lengths. The helper handles absent strings.
        unsafe { tag_async_task(8, b"eight".as_ptr(), 5) };
        assert_eq!(async_trace_of(7), (None, None));
        assert_eq!(async_trace_of(8).0.as_deref(), Some("eight"));
        assert_eq!(async_trace_of(8).1, None);
        // Empty or null names never become a dangling slice.
        // SAFETY: The helper accepts a null name and returns an empty byte slice.
        unsafe { tag_async_task(9, std::ptr::null(), 4) };
        assert_eq!(async_trace_of(9).0.as_deref(), Some(""));
    }

    #[test]
    fn current_call_site_tracks_innermost_frame_and_empty_stack() {
        let previous = replace_current(Default::default());
        assert_eq!(current_call_site(), None);
        // SAFETY: Byte literals have process lifetime.
        unsafe { willow_callstack_push(b"outer".as_ptr(), 5, b"outer.wi".as_ptr(), 8, 12, 1) };
        // SAFETY: Byte literals have process lifetime.
        unsafe { willow_callstack_push(b"inner".as_ptr(), 5, b"inner.wi".as_ptr(), 8, 34, 1) };
        let inner = current_call_site();
        willow_callstack_pop();
        let outer = current_call_site();
        willow_callstack_pop();
        let empty = current_call_site();
        replace_current(previous);
        assert_eq!(inner, Some(("inner.wi".to_owned(), 34)));
        assert_eq!(outer, Some(("outer.wi".to_owned(), 12)));
        assert_eq!(empty, None);
    }

    #[test]
    fn stack_trace_records_source_frames() {
        let mut trace = RuntimeStackTrace::default();
        // SAFETY: Byte literals have process lifetime.
        let site =
            unsafe { TraceSite::from_static(b"main".as_ptr(), 4, b"main.wi".as_ptr(), 7, 3, 5) };
        trace.push(site);
        let frame = trace.active_frames().next().expect("frame");
        assert_eq!(
            (frame.0.as_ref(), frame.1.as_ref(), frame.2, frame.3),
            ("main", "main.wi", 3, 5)
        );
        assert_eq!(trace.pop().unwrap().site.line, 3);
    }

    #[test]
    fn callstack_text_orders_most_recent_first() {
        fn push(name: &'static str, file: &'static str, line: i32, col: i32) {
            // SAFETY: Both strings have process lifetime.
            unsafe {
                willow_callstack_push(
                    name.as_ptr(),
                    name.len() as i64,
                    file.as_ptr(),
                    file.len() as i64,
                    line,
                    col,
                )
            };
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
