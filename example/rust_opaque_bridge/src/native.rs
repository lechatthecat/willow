pub struct Regex(regex::Regex);
impl Regex {
    pub fn new(pattern: &str) -> Result<Box<Self>, String> {
        regex::Regex::new(pattern)
            .map(|r| Box::new(Self(r)))
            .map_err(|e| e.to_string())
    }
    pub fn is_match(&self, text: &str) -> bool {
        self.0.is_match(text)
    }
}
