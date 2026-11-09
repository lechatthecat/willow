# PostgreSQL from Willow Tasks over TLS

This runnable example serves an HTTPS POST, binds its body to `$1::text`, reads
that value from PostgreSQL, and returns it over HTTPS. The PostgreSQL v3 codec
uses `postgres-protocol`; all socket I/O stays in Willow `std::net` Tasks.
There is no Tokio, tokio-postgres or sqlx. TLS and HTTP framing reuse the adjacent
`https_api` bridge through a local Cargo dependency.

Provide a PostgreSQL server accepting TLS and password authentication (SCRAM-SHA-256,
MD5 or password), a PEM trust certificate for its `localhost` server certificate,
a matching PEM private key for the sample HTTPS listener, and a password file
containing the exact password **without a trailing newline**:

```sh
willow run example/postgres_api -- /tmp/cert.pem /tmp/key.pem \
  127.0.0.1:5432 postgres /tmp/password postgres
```

The first two paths also configure the local HTTPS endpoint: for this local
example, use the same certificate/key for the PostgreSQL test server. The
certificate must contain `DNS:localhost`. Certificate and hostname verification
are enabled. The database address must be numeric; the example does not perform
blocking DNS resolution. Use OS-appropriate paths on Windows.

Expected output is a JSON string containing a quote and SQL-looking text, followed
by `served PostgreSQL result over HTTPS`. The SQL text and parameter values are
encoded separately using Parse/Bind/Describe/Execute/Sync. Never interpolate user
input into the SQL argument of `query_db`.

Optional final arguments:

- `transactions`: verifies insert rollback, insert commit, a NULL parameter and
  2048 result rows on a temporary table.
- `sql-error`: triggers division by zero, drains through ReadyForQuery, rolls back
  and runs a fresh query on the same connection; prints `42`.
- `cancel`: cancels a Task waiting on a query; verifies cancellation and TLS,
  codec and parameter destructor counts, then uses a new connection successfully.
- `tls-failure`: uses an invalid hostname and prints `true` for the Result error.
- Any other nonempty mode executes the direct database path and prints whether
  it returned an error (useful with a wrong password or disconnected test peer).

A database Task owns the entire connection and transaction. Its deferred cleanup
shuts down the socket and closes Rust handles on success, error and cancellation.
The GC-managed socket descriptor is reclaimed by the runtime finalizer.
Cancelled connections are discarded, never returned to a pool. PostgreSQL detects
the closed peer according to its own connection-check settings; this API does not
send a separate PostgreSQL CancelRequest or promise instantaneous server-side
query cancellation. No connection handle escapes this Task scope.

The bridge uses non-waiting `try_lock`, bounded buffers, and no socket calls.
Results are UTF-8 text with `Option<String>` preserving NULL. Row/column lookup
uses indexed vectors. Limits: 1 MiB backend frame/result storage, 64 KiB input
chunks/SQL, 1024 parameters of at most 1024 bytes each. A query has one statement
and one active result. COPY, notifications, replication, binary result decoding,
SCRAM channel binding and connection pooling are not provided. Unsupported
messages return errors; framing/decoding errors permanently invalidate the codec.
SQL errors retain SQLSTATE and drain to ReadyForQuery before allowing reuse.

Run focused codec tests with:

```sh
cargo test --manifest-path example/postgres_api/Cargo.toml pg_
```

The reviewed nonblocking fingerprints cover this bridge and the dependency set.
After changing either bridge, inspect `willow query rust-bridge --project-dir
example/postgres_api` and refresh the declarations after review. The shared TLS
bridge also has declarations in `example/https_api` which must remain current.
