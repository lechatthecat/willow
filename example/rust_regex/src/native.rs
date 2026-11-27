pub fn regex_is_match(pattern: &str, text: &str) -> Result<bool, String> {
    regex::Regex::new(pattern)
        .map(|regex| regex.is_match(text))
        .map_err(|error| error.to_string())
}
