#[unsafe(no_mangle)]
pub extern "C" fn willow_regex_probe() -> bool { regex::Regex::new("^willow$").unwrap().is_match("willow") }
