# HTTPS API over Willow Tasks

The example runs a local HTTPS `POST /echo` server and client. Willow handles
socket readiness through `std::net`, routing and JSON parsing/serialization.
The socket-free Rust bridge uses rustls (ring provider) and httparse. It uses
`try_lock`, bounded buffers and no Tokio, blocking socket calls or callbacks.

Generate a short-lived local test certificate (OpenSSL is needed for this step):

```sh
openssl req -x509 -newkey rsa:2048 -nodes -keyout /tmp/https-key.pem \
  -out /tmp/https-cert.pem -days 1 -subj /CN=localhost \
  -addext subjectAltName=DNS:localhost -addext basicConstraints=critical,CA:FALSE
willow run example/https_api -- /tmp/https-cert.pem /tmp/https-key.pem
```

Expected output is the echoed JSON followed by `served POST /echo`. No test
private key is checked in. Delete the temporary certificate/key after use.
Use OS-appropriate temporary paths on Windows. The ring dependency requires
a native C toolchain (MSVC on Windows, Xcode command-line tools on macOS).

Optional third argument `cancel` verifies progress during a stalled handshake,
TLS destructor execution and peer EOF after cancellation. `partial`, `malformed`,
`tls-failure` and `invalid-json` exercise Result errors on both sides. These modes
are diagnostic examples and return `true` assertions on stdout.

The HTTP subset is deliberately one Content-Length message per connection:
64 headers, 16 KiB header limit, 1 MiB total-message limit, no chunked transfer,
HTTP/2 or persistent/pipelined connections. Duplicate Content-Length and any
Transfer-Encoding are rejected. Certificate validation and hostname validation
remain enabled. Numeric socket addresses avoid synchronous DNS resolution.

`nonblocking("<review fingerprint>") opaque Tls;` explicitly covers the generated
close operation and destructor. Ordinary unannotated opaque declarations retain
their conservative effects. The same source/dependency review fingerprint
validation applies to close and the other bridge functions. After reviewing
bridge changes, obtain the current fingerprint with
`willow query rust-bridge --project-dir example/https_api` and update declarations.

Focused standalone bridge tests:

```sh
cargo test --manifest-path example/https_api/Cargo.toml http_
cargo test --manifest-path example/https_api/Cargo.toml tls_invalid_configuration_is_result_error
HTTPS_TEST_CERT=/tmp/https-cert.pem HTTPS_TEST_KEY=/tmp/https-key.pem \
  cargo test --manifest-path example/https_api/Cargo.toml tls_exchange_and_transfer_counts -- --nocapture
```
