"""Shared Java toolchain helpers for the Willow/Go/Java benchmark harnesses."""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import subprocess


def java_binary() -> Path:
    override = os.environ.get("JAVA")
    if override:
        return Path(override)
    java_home = os.environ.get("JAVA_HOME")
    if java_home and (Path(java_home) / "bin" / "java").exists():
        return Path(java_home) / "bin" / "java"
    return Path(shutil.which("java") or "/usr/bin/java")


def javac_command() -> list[str]:
    """Return a javac invocation; fall back to the jdk.compiler module on JRE-only images."""
    java = java_binary()
    sibling = java.parent / "javac"
    if sibling.exists():
        return [str(sibling)]
    found = shutil.which("javac")
    if found:
        return [found]
    return [str(java), "-m", "jdk.compiler/com.sun.tools.javac.Main"]


def compile_java(source: Path, out_dir: Path) -> None:
    out_dir.mkdir(parents=True, exist_ok=True)
    subprocess.run([*javac_command(), "-d", str(out_dir), str(source)], check=True)


def java_command(class_dir: Path, workers: int, args: list[str]) -> list[str]:
    """JVM invocation with the worker limit applied to the processor count and virtual-thread scheduler."""
    return [
        str(java_binary()),
        f"-XX:ActiveProcessorCount={workers}",
        f"-Djdk.virtualThreadScheduler.parallelism={workers}",
        "-cp",
        str(class_dir),
        "Bench",
        *args,
    ]


def java_version() -> str:
    try:
        result = subprocess.run([str(java_binary()), "-version"], check=True, text=True, capture_output=True)
    except (subprocess.CalledProcessError, FileNotFoundError):
        return "unknown"
    return (result.stderr or result.stdout).splitlines()[0].strip()
