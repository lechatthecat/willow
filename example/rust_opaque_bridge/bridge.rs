pub type Regex = regex::Regex;
pub fn regex_new(pattern: &str) -> Result<Box<Regex>, String> {
    Regex::new(pattern).map(Box::new).map_err(|error| error.to_string())
}
pub fn regex_is_match(regex: &Regex, text: &str) -> bool {
    regex.is_match(text)
}
