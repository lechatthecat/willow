#!/usr/bin/env python3
"""Build the C input and create a standalone Willow project in an output directory."""
import argparse
import hashlib
import os
import platform
from pathlib import Path
import shutil
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("output", type=Path)
parser.add_argument("--shared", action="store_true")
args = parser.parse_args()
root = args.output.resolve()
source = Path(__file__).resolve().parent
root.mkdir(parents=True, exist_ok=True)
for name in ["src", "adapter"]:
    shutil.copytree(source / name, root / name, dirs_exist_ok=True)
shutil.copyfile(source / "bridge.rs", root / "bridge.rs")
artifact = root / "artifacts"
artifact.mkdir(exist_ok=True)
host = next(line[6:] for line in subprocess.check_output(["rustc", "-vV"], text=True).splitlines() if line.startswith("host: "))
arch = host.split("-")[0]
if args.shared:
    if host.endswith("windows-msvc"):
        archive, abi = "addition.dll", "c-msvc"
        subprocess.run([os.environ.get("CC", "cl"), "/nologo", "/LD", str(source / "add.c"), "/link", "/OUT:" + str(artifact / archive)], cwd=root, check=True)
    else:
        apple = host.endswith("apple-darwin")
        archive, abi = ("libaddition.dylib", "c-darwin") if apple else ("libaddition.so", "c-sysv")
        subprocess.run([os.environ.get("CC", "cc"), "-dynamiclib" if apple else "-shared", "-fPIC", str(source / "add.c"), "-o", str(artifact / archive)], check=True)
    (root / "adapter/src/lib.rs").write_text('''use std::ffi::{c_char, c_void};
unsafe extern "C" {
    fn willow_native_symbol_v1(library: *const c_char, symbol: *const c_char) -> *mut c_void;
}
pub fn add(a: i64, b: i64) -> i64 {
    let pointer = unsafe { willow_native_symbol_v1(c"addition".as_ptr(), c"native_add".as_ptr()) };
    assert!(!pointer.is_null(), "native addition could not be loaded");
    // The explicit library policy negotiates version 1 of this C signature.
    let add: unsafe extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(pointer) };
    unsafe { add(a, b) }
}
''')
elif host.endswith("windows-msvc"):
    obj = root / "add.obj"
    subprocess.run([os.environ.get("CC", "cl"), "/nologo", "/c", str(source / "add.c"), "/Fo" + str(obj)], check=True)
    subprocess.run(["lib", "/nologo", "/OUT:" + str(artifact / "add.lib"), str(obj)], check=True)
    abi, archive = "c-msvc", "add.lib"
    obj.unlink()
else:
    obj = root / "add.o"
    subprocess.run([os.environ.get("CC", "cc"), "-c", str(source / "add.c"), "-o", str(obj)], check=True)
    subprocess.run([os.environ.get("AR", "ar"), "rcs", str(artifact / "libadd.a"), str(obj)], check=True)
    abi, archive = ("c-darwin" if host.endswith("apple-darwin") else "c-sysv"), "libadd.a"
    obj.unlink()
def tree_digest(directory):
    value = hashlib.sha256(b"willow-native-tree-v1")
    for path in sorted(directory.iterdir(), key=lambda p: p.name):
        name = path.name.encode()
        is_directory = path.is_dir()
        value.update(bytes([int(is_directory)]))
        value.update(len(name).to_bytes(8, "little"))
        value.update(name)
        child = tree_digest(path) if is_directory else hashlib.file_digest(path.open("rb"), "sha256").hexdigest()
        value.update(child.encode())
    return value.hexdigest()

source_digest = tree_digest(artifact)
(root / "project.toml").write_text(f'''[project]
name = "native_library"
version = "0.1.0"
[rust-dependencies]
native_adapter = {{ path = "adapter" }}
[rust]
bridge = "bridge.rs"
[native]
roots = ["addition"]
reproducible = {str(not args.shared).lower()}
[native.libraries.addition]
name = "addition"
version = "1.0.0"
license = "MIT"
provenance = "locally compiled add.c"
[native.libraries.addition.targets."{host}"]
source = {{ kind = "local", path = "artifacts" }}
digest = "{source_digest}"
architecture = "{arch}"
abi = "{abi}"
abi-version = "1"
linkage = "{'shared' if args.shared else 'static'}"
inputs = [{{ kind = "file", value = "{archive}" }}]
''')
if args.shared:
    with (root / "project.toml").open("a") as manifest:
        manifest.write(f'''[native.libraries.addition.targets."{host}".loader]
distribution-name = "{archive}"
abi-symbol = "native_negotiate"
version = 1
symbols = ["native_add"]
dependency-search = "platform"
platform-provider = "{platform.system()}"
platform-version = "{platform.release()}"
''')
print(root)
