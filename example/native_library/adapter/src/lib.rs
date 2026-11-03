unsafe extern "C" {
    fn native_add(a: i64, b: i64) -> i64;
}
pub fn add(a: i64, b: i64) -> i64 {
    // The linked C implementation accepts every i64 pair and wraps addition.
    unsafe { native_add(a, b) }
}
