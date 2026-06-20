mod cli;

use anyhow::Result;

fn main() -> Result<()> {
    cli::run(std::env::args().skip(1).collect())
}
