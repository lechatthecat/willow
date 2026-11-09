// Fixed pattern and bounded input keep this synchronous task call short.
// No Willow callbacks or managed pointers are retained by the Rust body.
pub fn is_identifier(text: &str) -> bool {
    if text.len() > 64 {
        return false;
    }
    regex::Regex::new(r"^[a-zA-Z_][a-zA-Z0-9_]*$")
        .expect("fixed pattern")
        .is_match(text)
}
