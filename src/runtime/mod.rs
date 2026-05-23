// Runtime functions linked with Cranelift-generated object files.

#[unsafe(no_mangle)]
pub extern "C" fn willow_print_i64(value: i64) {
    print!("{}", value);
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_println_i64(value: i64) {
    println!("{}", value);
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_print_bool(value: u8) {
    print!("{}", if value != 0 { "true" } else { "false" });
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_println_bool(value: u8) {
    println!("{}", if value != 0 { "true" } else { "false" });
}
