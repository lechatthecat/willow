#!/usr/bin/env python3
"""Measure native linker dead stripping with identical objects and archives."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def checked(command, *, env=None):
    result = subprocess.run(command, cwd=ROOT, env=env, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(f"command failed: {command!r}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}")
    return result


def windows_linker():
    env = os.environ.copy()
    vswhere = shutil.which("vswhere.exe")
    if not vswhere:
        vswhere = str(Path(env.get("ProgramFiles(x86)", r"C:\Program Files (x86)")) / "Microsoft Visual Studio/Installer/vswhere.exe")
    installation = checked([vswhere, "-latest", "-products", "*", "-requires", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-property", "installationPath"]).stdout.strip()
    if not installation:
        raise RuntimeError("vswhere found no Visual Studio C++ toolchain")
    arch = {"AMD64": "x64", "x86_64": "x64", "ARM64": "arm64", "aarch64": "arm64", "x86": "x86"}.get(platform.machine())
    if not arch:
        raise RuntimeError(f"unsupported Windows architecture: {platform.machine()}")
    devcmd = Path(installation) / "Common7/Tools/VsDevCmd.bat"
    # A batch file avoids cmd.exe's distinct /c quoting rules. Keep setup
    # diagnostics separate from the environment so failures never dump secrets.
    marker = "WILLOW_LINK_ENVIRONMENT_START"
    with tempfile.TemporaryDirectory(prefix="willow-msvc-env-") as directory:
        batch = Path(directory) / "setup.cmd"
        batch.write_text(f'@echo off\ncall "{devcmd}" -no_logo -arch={arch}\n'
                         f'if errorlevel 1 exit /b 1\necho {marker}\nset\n')
        gathered = subprocess.run(["cmd.exe", "/d", "/c", str(batch)], cwd=ROOT,
                                  text=True, capture_output=True)
    diagnostics, separator, values = gathered.stdout.partition(marker)
    if gathered.returncode or not separator:
        raise RuntimeError("VsDevCmd.bat failed:\n" + diagnostics + gathered.stderr)
    for line in values.splitlines():
        key, separator, value = line.partition("=")
        if separator and key:
            env[key.upper()] = value
    located = checked(["where.exe", "link.exe"], env=env).stdout.splitlines()
    linker = next((line.strip() for line in located if "MSVC" in line.upper()), None)
    if not linker:
        raise RuntimeError("MSVC link.exe missing from initialized PATH")
    return linker, env


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--compiler", type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--iterations", type=int, default=7)
    args = parser.parse_args()
    if args.iterations < 1:
        parser.error("--iterations must be positive")
    system = platform.system()
    if system not in {"Linux", "Darwin", "Windows"}:
        parser.error(f"unsupported native platform: {system}")
    windows = system == "Windows"
    compiler = (args.compiler or ROOT / "target/debug" / ("willowc.exe" if windows else "willowc")).resolve()
    linker, env = windows_linker() if windows else (shutil.which("cc") or "cc", os.environ.copy())
    report = {"platform": system, "architecture": platform.machine(), "compiler": str(compiler), "linker": linker, "iterations": args.iterations, "method": "Same object/archive per profile; before/after order alternates each iteration; link wall time excludes executable validation; every result must print 42.", "profiles": []}
    with tempfile.TemporaryDirectory(prefix="willow-native-link-") as directory:
        work = Path(directory)
        source = work / "main.wi"
        source.write_text('fn main() { println(42); }\n')
        for profile in ["debug", "release"]:
            runtime = ROOT / "target" / profile / ("willow_runtime.lib" if windows else "libwillow_runtime.a")
            if not runtime.is_file():
                raise RuntimeError(f"build the {profile} runtime first: {runtime}")
            executable = work / (profile + (".exe" if windows else ""))
            compile_env = env.copy()
            compile_env["WILLOW_KEEP_OBJECT"] = "1"
            build = [str(compiler), "build", str(source), "--runtime-lib", str(runtime), "-o", str(executable)]
            if profile == "release":
                build.append("--release")
            checked(build, env=compile_env)
            obj = Path(str(executable) + (".obj" if windows else ".o"))
            if windows:
                base = [str(obj), str(runtime), f"/OUT:{executable}", "/NOLOGO", "/SUBSYSTEM:CONSOLE", "legacy_stdio_definitions.lib", "kernel32.lib", "ntdll.lib", "userenv.lib", "ws2_32.lib", "dbghelp.lib", "psapi.lib", "/defaultlib:msvcrt"]
                # Match the original driver flags. MSVC may already enable
                # elimination by default; do not manufacture a before/after win.
                before = []
                after = ["/OPT:REF", "/OPT:ICF"] + (["/INCLUDE:willow_runtime_metadata_v1"] if profile == "debug" else [])
            else:
                base = [str(obj), str(runtime), "-o", str(executable), "-lm", "-lpthread"]
                if system == "Linux":
                    base += ["-no-pie", "-ldl"]
                    after = ["-Wl,--gc-sections"] + (["-Wl,--undefined=willow_runtime_metadata_v1"] if profile == "debug" else [])
                else:
                    base += ["-liconv"]
                    after = ["-Wl,-dead_strip"] + (["-Wl,-u,_willow_runtime_metadata_v1"] if profile == "debug" else [])
                before = []
            samples = []
            for iteration in range(args.iterations):
                for mode in (["before", "after"] if iteration % 2 == 0 else ["after", "before"]):
                    flags = before if mode == "before" else after
                    started = time.perf_counter_ns()
                    checked([linker, *base, *flags], env=env)
                    elapsed_ms = (time.perf_counter_ns() - started) / 1e6
                    result = checked([str(executable)], env=env)
                    if result.stdout.strip() != "42":
                        raise RuntimeError(f"unexpected executable output: {result.stdout!r}")
                    samples.append({"iteration": iteration, "mode": mode, "elapsed_ms": elapsed_ms, "bytes": executable.stat().st_size, "stdout": result.stdout})
            medians = {mode: {"elapsed_ms": statistics.median(x["elapsed_ms"] for x in samples if x["mode"] == mode), "bytes": statistics.median(x["bytes"] for x in samples if x["mode"] == mode)} for mode in ["before", "after"]}
            report["profiles"].append({"profile": profile, "runtime": str(runtime), "before_flags": before, "after_flags": after, "samples": samples, "medians": medians})
            print(json.dumps({"profile": profile, "medians": medians}), flush=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
