#!/usr/bin/env python3
"""Save native runtime accessor disassembly using Rust's LLVM tools."""
import pathlib
import subprocess
import sys

sysroot = pathlib.Path(subprocess.check_output(["rustc", "+1.95.0", "--print", "sysroot"], text=True).strip())
tools = list(sysroot.glob("lib/rustlib/*/bin/llvm-objdump*"))
if len(tools) != 1:
    raise RuntimeError(f"expected one native llvm-objdump, found {tools}")
output = pathlib.Path(sys.argv[3])
output.mkdir(parents=True, exist_ok=True)
for label, archive in zip(["before", "after"], sys.argv[1:3]):
    result = subprocess.run([str(tools[0]), "--disassemble-symbols=willow_panic_depth,willow_root_depth,_willow_panic_depth,_willow_root_depth", archive], check=True, capture_output=True)
    (output / f"{label}-accessors.txt").write_bytes(result.stdout)
