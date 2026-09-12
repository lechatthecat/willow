#!/usr/bin/env python3
"""Supply an unused new ABI name required by MSVC even without relocations.

Only the isolated old-runtime checkout is changed. Aborting proves the depth
benchmark cannot accidentally execute this newer channel operation.
"""
import os
from pathlib import Path
import sys

if os.name == "nt":
    channel = Path(sys.argv[1]) / "crates/willow_runtime/src/channel.rs"
    with channel.open("a") as stream:
        stream.write("""
// Benchmark-only compatibility export: must never execute.
#[unsafe(no_mangle)]
pub extern "C" fn willow_channel_select_cleanup(
    _raw: *mut std::ffi::c_void,
    _winner: *mut std::ffi::c_void,
    _direction: i64,
) {
    std::process::abort();
}
""")
