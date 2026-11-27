pub fn probe() -> bool {
    regex::Regex::new("^willow$").unwrap().is_match("willow")
}
