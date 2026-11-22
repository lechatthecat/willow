//! User-facing driver; compiler implementation remains in willow_compiler.
mod cli;

fn main() {
    let code = cli::run(std::env::args().skip(1).collect());
    std::process::exit(code)
}
