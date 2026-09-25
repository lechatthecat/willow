# Path dependency example

Build from the repository root:

```sh
cargo run -p willowc -- build example/package_paths/app -o /tmp/willow-package-paths
/tmp/willow-package-paths
```

Expected output: `11`, `22`, `11`, `1`, `13` on separate lines.
The app imports two packages with the same `util::Value` and `util::value`
names. `again` is a second alias of the left package; the right package imports
that package under its own `base` alias.
