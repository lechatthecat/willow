//! User-facing driver; compiler implementation remains in willow_compiler.
mod cli;

fn main() -> anyhow::Result<()> {
    cli::run(std::env::args().skip(1).collect())
}
