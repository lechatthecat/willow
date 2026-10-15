# Willow prebuilt distribution

Extract the archive for your operating system and architecture into a permanent
directory. Keep the entire directory together:

```text
willow-<release>-<platform>/
  bin/willow             # willow.exe on Windows
  lib/libwillow_runtime.a # willow_runtime.lib on Windows
  README.md
  LICENSE
```

## Linux and macOS

For example, after extracting to `$HOME/tools/willow`:

```sh
export PATH="$HOME/tools/willow/bin:$PATH"
willow --version
willow init hello
cd hello
willow run
```

Add the export to your shell startup file to retain it in new terminals. A system
C compiler/linker (`cc`) is required to build Willow programs. On macOS, install
the Xcode Command Line Tools; on Linux, install your distribution's C toolchain.
No Rust toolchain or Cargo build is needed for this distribution.

## Windows (PowerShell)

Extract the ZIP, for example to `C:\tools\willow`, then run:

```powershell
$env:Path = "C:\tools\willow\bin;" + $env:Path
willow --version
willow init hello
cd hello
willow run
```

Add `C:\tools\willow\bin` to your user PATH in Environment Variables for future
terminals. Install MSVC Build Tools with the C++ tools and Windows SDK, and use
a developer terminal where the linker is available.

## Runtime and build identity

Keep `bin/` next to `lib/`: Willow locates its bundled runtime in `../lib` relative
to the executable. Move the whole distribution when relocating it. Use the runtime
from the same archive as the compiler.

`willow --version`, `willow -V`, and `willow version` print the Cargo package
version and full build commit. Include this output in bug reports. The package
version can differ from the release tag used in the archive filename.

Source checkout builds also embed their HEAD commit. Source archives without Git
metadata report `commit unknown`, unless built with `WILLOW_BUILD_COMMIT` set to
the full 40- or 64-digit hexadecimal commit ID. This identifies the base commit;
it does not certify an unmodified working tree.
