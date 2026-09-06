mod cli;

use anyhow::Result;

/// The front end walks the syntax tree recursively, so the driver runs on a
/// thread with an explicit stack instead of the process stack: Windows reserves
/// only 1 MiB for `main`, which a dozen chained `&&` operands can exhaust long
/// before the program itself is wrong.
const COMPILER_STACK_BYTES: usize = 64 * 1024 * 1024;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::thread::Builder::new()
        .name("willowc".to_string())
        .stack_size(COMPILER_STACK_BYTES)
        .spawn(move || cli::run(args))
        .expect("failed to spawn the compiler thread")
        .join()
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
}
