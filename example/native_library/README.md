# Native C library example

Requires Python 3, Rust/Cargo, and a host C compiler and archiver (MSVC developer shell on Windows).
From the repository root:

```sh
python3 example/native_library/prepare.py target/native-library-example
cargo run --bin willow -- run target/native-library-example
cargo run --bin willow -- run target/native-library-example --frozen
```

Both runs print `42`. The Rust adapter exposes the C function through Willow's
existing scalar bridge. The explicit native entry supplies the C archive to the
final linker. Regenerate the project after rebuilding C inputs; `--locked` rejects
changes to its recorded native identity. Generated archives and locks stay in the
output directory.

Native entries are root-project dependencies, separate from Cargo and Willow
modules. Each selected target records its ABI, source checksum, dependency names,
and ordered file/search inputs in `project.lock`. Link input repetitions are
preserved. Static cycles use GNU linker groups; unsupported linker families report
`UnsupportedLinkCycle`.

Pinned HTTPS sources use `source = { kind = "artifact", url = "https://…" }`,
a SHA-256 of the downloaded file, and a file input named `artifact`. They accept
raw static archives or shared libraries, not tar/zip bundles or thin archives. `--offline` requires a
verified cached file. Downloads need `curl`; local dependencies do not.

Explicit system libraries require `reproducible = false`, provider/version
metadata, `linkage = "system"`, and system-name inputs. They produce a
non-hermetic notice. For shared libraries, prepare a separate project:

```sh
python3 example/native_library/prepare.py --shared target/native-shared-example
cargo run --bin willow -- run target/native-shared-example
```

Shared entries require a `loader` table: deployment filename, negotiation export,
requested ABI version, allowlisted symbols, and explicit platform dependency
search with provider/version metadata. This mode requires `reproducible = false`
and reports a non-hermetic notice for transitive platform loader dependencies.
The runtime validates copied bytes and calls `uint32_t negotiate(uint32_t)`;
only a return equal to the requested version permits symbol access. Rust adapters
use the documented `willow_native_symbol_v1(library, symbol)` C API, which returns
null on rejection. Libraries remain loaded for the process lifetime.

Deploy the executable and its `<executable-name>.native/` directory together.
The primary libraries are resolved relative to the executable, never the working
directory or a caller-provided host path. The policy explicitly trusts library
code, including initialization routines; it is not a process sandbox.
