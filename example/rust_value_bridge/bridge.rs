pub fn text(value: &str) -> String { value_native::text(value) }
pub fn bytes(value: &[u8]) -> Vec<u8> { value.to_owned() }
pub fn maybe(value: Option<i64>) -> Option<i64> { value }
pub fn maybe_text(value: Option<&str>) -> Option<String> { value.map(str::to_owned) }
pub fn maybe_bytes(value: Option<&[u8]>) -> Option<Vec<u8>> { value.map(<[u8]>::to_vec) }
pub fn checked(value: bool) -> Result<bool, String> { if value { Ok(true) } else { Err("失敗".into()) } }
pub fn checked_bytes(value: &[u8]) -> Result<Vec<u8>, String> { if value.is_empty() { Err("empty".into()) } else { Ok(value.to_vec()) } }
pub fn nested(value: Option<Option<i64>>) -> Option<Option<i64>> { value }
pub fn nested_text(value: Option<Option<&str>>) -> Option<Option<String>> { value.map(|v| v.map(str::to_owned)) }
pub fn scalar_result(value: Result<f64, bool>) -> Result<f64, bool> { value }
pub fn result_input(value: Result<&[u8], &str>) -> Result<Vec<u8>, String> { value.map(<[u8]>::to_vec).map_err(str::to_owned) }
pub fn concurrent(value: &str) -> String {
    // Rust worker threads only see Rust-owned buffers, never Willow objects.
    let handles: Vec<_> = (0..8).map(|_| { let owned = value.to_owned(); std::thread::spawn(move || owned) }).collect();
    for handle in handles { assert_eq!(handle.join().unwrap(), value); }
    value.to_owned()
}
