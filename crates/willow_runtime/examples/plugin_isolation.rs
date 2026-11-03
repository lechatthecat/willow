//! Run without arguments for WASM, or pass a statically linked native guest.
#![no_main]
use std::collections::HashMap;
use std::io::Read;
use willow_abi::plugin::{NATIVE_ENGINE, VERSION, WASM_ENGINE};
use willow_runtime::plugin::{Budgets, Manifest, Mode, Session, digest};
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let budgets = Budgets::default();
    let (mode, engine, target, bytes) = if let Some(path) = std::env::args_os().nth(1) {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(budgets.artifact_bytes as u64 + 1)
            .read_to_end(&mut bytes)?;
        (
            Mode::NativeProcess,
            NATIVE_ENGINE,
            "x86_64-unknown-linux-gnu",
            bytes,
        )
    } else {
        (
            Mode::Wasm,
            WASM_ENGINE,
            "wasm32",
            wat::parse_str(include_str!("plugin/echo.wat"))?,
        )
    };
    let manifest = Manifest {
        version: VERSION,
        digest: digest(&bytes),
        mode,
        engine: engine.into(),
        target: target.into(),
        methods: vec![1],
        read_resources: vec![],
        budgets,
    };
    let mut session = Session::start(manifest, &bytes, HashMap::new())?;
    println!("{}", session.diagnostics());
    let (request, _) = session.submit(1, None, b"hello isolated plugin")?;
    session.drive_one();
    let response = session.take(request).ok_or("missing response")??;
    assert_eq!(response, b"hello isolated plugin");
    println!("isolated round trip: {} bytes", response.len());
    session.shutdown();
    Ok(())
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_user_main() {
    if let Err(error) = run() {
        eprintln!("plugin example: {error}");
        std::process::exit(1);
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn __willow_static_init() {}
