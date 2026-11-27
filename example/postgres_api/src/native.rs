//! Socket-free PostgreSQL v3 codec. Willow owns transport and cancellation.
use bytes::BytesMut;
use fallible_iterator::FallibleIterator;
pub struct Tls(https_api_bridge_tests::Tls);
pub struct Http(https_api_bridge_tests::Http);
pub fn tls_drops() -> i64 {
    https_api_bridge_tests::tls_drops()
}
impl Tls {
    pub fn server(cert: &str, key: &str) -> Result<Box<Tls>, String> {
        https_api_bridge_tests::Tls::server(cert, key).map(|value| Box::new(Self(*value)))
    }
}
impl Tls {
    pub fn client(cert: &str, name: &str) -> Result<Box<Tls>, String> {
        https_api_bridge_tests::Tls::client(cert, name).map(|value| Box::new(Self(*value)))
    }
}
impl Tls {
    pub fn step(&self, input: &[u8]) -> Result<Vec<u8>, String> {
        self.0.step(input)
    }
}
impl Tls {
    pub fn ready(&self) -> Result<bool, String> {
        self.0.ready()
    }
}
impl Tls {
    pub fn send(&self, text: &str) -> Result<Vec<u8>, String> {
        self.0.send(text)
    }
}
impl Tls {
    pub fn send_bytes(&self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        self.0.send_bytes(bytes)
    }
}
impl Tls {
    pub fn plain(&self) -> Result<Vec<u8>, String> {
        self.0.plain()
    }
}
impl Http {
    pub fn new() -> Box<Http> {
        Box::new(Self(*https_api_bridge_tests::Http::new()))
    }
}
impl Http {
    pub fn feed(&self, bytes: &[u8], response: bool) -> Result<bool, String> {
        self.0.feed(bytes, response)
    }
}
impl Http {
    pub fn body(&self) -> Result<String, String> {
        self.0.body()
    }
}
impl Http {
    pub fn route(&self) -> Result<String, String> {
        self.0.route()
    }
}
impl Http {
    pub fn encode(body: &str, response: bool) -> Result<String, String> {
        https_api_bridge_tests::Http::encode(body, response)
    }
}

use postgres_protocol::{
    IsNull, authentication,
    message::{backend, frontend},
};
use std::sync::{
    Mutex,
    atomic::{AtomicI64, Ordering},
};
static PG_DROPS: AtomicI64 = AtomicI64::new(0);
static PARAM_DROPS: AtomicI64 = AtomicI64::new(0);
pub fn pg_drops() -> i64 {
    PG_DROPS.load(Ordering::SeqCst)
}
pub fn param_drops() -> i64 {
    PARAM_DROPS.load(Ordering::SeqCst)
}
const LIMIT: usize = 1024 * 1024;
fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
#[derive(Default)]
struct State {
    input: BytesMut,
    ready: bool,
    authenticated: bool,
    auth_phase: u8,
    scram: Option<authentication::sasl::ScramSha256>,
    input_bytes: usize,
    framing_checks: usize,
    querying: bool,
    failed: bool,
    status: u8,
    sql_error: Option<String>,
    rows: Vec<Vec<Option<String>>>,
    stored: usize,
    messages: usize,
    cells: usize,
}
pub struct Pg(Mutex<State>);
pub struct Params(Mutex<Vec<Option<String>>>);
impl Drop for Pg {
    fn drop(&mut self) {
        PG_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}
impl Drop for Params {
    fn drop(&mut self) {
        PARAM_DROPS.fetch_add(1, Ordering::SeqCst);
    }
}
impl Pg {
    pub fn new() -> Box<Pg> {
        Box::new(Pg(Mutex::new(State::default())))
    }
}
impl Params {
    pub fn new() -> Box<Params> {
        Box::new(Params(Mutex::new(Vec::new())))
    }
}
impl Params {
    pub fn add(&self, value: Option<&str>) -> Result<bool, String> {
        let mut state = self.0.try_lock().map_err(error)?;
        // Bound count and individual values before allocation; overall bytes are
        // checked once during encoding, rather than re-scanning on every append.
        if state.len() >= 1024 || value.is_some_and(|v| v.len() > LIMIT / 1024) {
            return Err("parameter limit exceeded".into());
        }
        state.push(value.map(str::to_owned));
        Ok(true)
    }
}
impl Pg {
    pub fn ssl_request() -> Vec<u8> {
        let mut out = BytesMut::new();
        frontend::ssl_request(&mut out);
        out.to_vec()
    }
}
impl Pg {
    pub fn start(user: &str, database: &str) -> Result<Vec<u8>, String> {
        if user.len() > 1024 || database.len() > 1024 {
            return Err("startup value too long".into());
        }
        let mut out = BytesMut::new();
        frontend::startup_message(
            [
                ("user", user),
                ("database", database),
                ("client_encoding", "UTF8"),
            ],
            &mut out,
        )
        .map_err(error)?;
        Ok(out.to_vec())
    }
}
impl Pg {
    pub fn query(&self, sql: &str, params: &Params) -> Result<Vec<u8>, String> {
        if sql.len() > 65536 {
            return Err("SQL too long".into());
        }
        let mut state = self.0.try_lock().map_err(error)?;
        if !state.ready || state.failed {
            return Err("connection is not ready".into());
        }
        let params = params.0.try_lock().map_err(error)?;
        let mut out = BytesMut::new();
        frontend::parse("", sql, [], &mut out).map_err(error)?;
        frontend::bind(
            "",
            "",
            [],
            params.iter(),
            |value, out| match value {
                Some(value) => {
                    out.extend_from_slice(value.as_bytes());
                    Ok(IsNull::No)
                }
                None => Ok(IsNull::Yes),
            },
            [],
            &mut out,
        )
        .map_err(|_| "parameter encoding failed")?;
        frontend::describe(b'P', "", &mut out).map_err(error)?;
        frontend::execute("", 0, &mut out).map_err(error)?;
        frontend::sync(&mut out);
        if out.len() > LIMIT {
            return Err("query too large".into());
        }
        state.ready = false;
        state.querying = true;
        state.sql_error = None;
        state.rows.clear();
        state.stored = 0;
        Ok(out.to_vec())
    }
}
impl Pg {
    pub fn ready(&self) -> Result<bool, String> {
        let state = self.0.try_lock().map_err(error)?;
        if state.failed {
            return Err("connection is unusable".into());
        }
        Ok(state.ready)
    }
}
impl Pg {
    pub fn status(&self) -> Result<i64, String> {
        Ok(i64::from(self.0.try_lock().map_err(error)?.status))
    }
}
impl Pg {
    pub fn result(&self) -> Result<i64, String> {
        let state = self.0.try_lock().map_err(error)?;
        if state.failed || !state.ready {
            return Err("connection is not ready".into());
        }
        if let Some(e) = &state.sql_error {
            return Err(e.clone());
        }
        Ok(state.rows.len() as i64)
    }
}
impl Pg {
    pub fn cell(&self, row: i64, column: i64) -> Result<Option<String>, String> {
        let state = self.0.try_lock().map_err(error)?;
        if state.failed || !state.ready || state.sql_error.is_some() {
            return Err("result unavailable".into());
        }
        state
            .rows
            .get(usize::try_from(row).map_err(error)?)
            .and_then(|r| r.get(usize::try_from(column).ok()?))
            .cloned()
            .ok_or_else(|| "cell out of bounds".into())
    }
}
impl Pg {
    pub fn feed(&self, input: &[u8], user: &str, password: &str) -> Result<Vec<u8>, String> {
        let mut state = self.0.try_lock().map_err(error)?;
        if state.failed {
            return Err("connection is unusable".into());
        }
        let result = feed(&mut state, input, user, password);
        if result.is_err() {
            state.failed = true;
        }
        result
    }
}
fn feed(state: &mut State, input: &[u8], user: &str, password: &str) -> Result<Vec<u8>, String> {
    if input.len() > 65536
        || state.input.len() + input.len() > LIMIT + 65536
        || user.len() > 1024
        || password.len() > 1024
    {
        return Err("input limit exceeded".into());
    }
    state.input_bytes += input.len();
    state.input.extend_from_slice(input);
    let mut out = BytesMut::new();
    while let Some(header) = backend::Header::parse(&state.input).map_err(error)? {
        state.framing_checks += 1;
        let length = header.len() as usize + 1;
        if length > LIMIT {
            return Err("backend frame too large".into());
        }
        if state.input.len() < length {
            break;
        }
        let message = backend::Message::parse(&mut state.input)
            .map_err(error)?
            .ok_or("incomplete frame")?;
        state.messages += 1;
        use backend::Message::{
            AuthenticationCleartextPassword, AuthenticationMd5Password, AuthenticationOk,
            AuthenticationSasl, AuthenticationSaslContinue, AuthenticationSaslFinal,
            BackendKeyData, BindComplete, CommandComplete, DataRow, EmptyQueryResponse,
            ErrorResponse, NoData, NoticeResponse, ParameterStatus, ParseComplete, ReadyForQuery,
            RowDescription,
        };
        match message {
            AuthenticationCleartextPassword if state.auth_phase == 0 && !state.authenticated => {
                state.auth_phase = 1;
                frontend::password_message(password.as_bytes(), &mut out).map_err(error)?;
            }
            AuthenticationMd5Password(body) if state.auth_phase == 0 && !state.authenticated => {
                state.auth_phase = 1;
                let hash =
                    authentication::md5_hash(user.as_bytes(), password.as_bytes(), body.salt());
                frontend::password_message(hash.as_bytes(), &mut out).map_err(error)?;
            }
            AuthenticationSasl(body) if state.auth_phase == 0 && !state.authenticated => {
                use authentication::sasl::{ChannelBinding, SCRAM_SHA_256, ScramSha256};
                let mut mechanisms = body.mechanisms();
                let mut supported = false;
                while let Some(mechanism) = mechanisms.next().map_err(error)? {
                    supported |= mechanism == SCRAM_SHA_256;
                }
                if !supported {
                    return Err("SCRAM-SHA-256 not offered".into());
                }
                let scram = ScramSha256::new(password.as_bytes(), ChannelBinding::unsupported());
                frontend::sasl_initial_response(SCRAM_SHA_256, scram.message(), &mut out)
                    .map_err(error)?;
                state.scram = Some(scram);
                state.auth_phase = 2;
            }
            AuthenticationSaslContinue(body) if state.auth_phase == 2 => {
                let scram = state.scram.as_mut().ok_or("missing SCRAM state")?;
                scram.update(body.data()).map_err(error)?;
                frontend::sasl_response(scram.message(), &mut out).map_err(error)?;
                state.auth_phase = 3;
            }
            AuthenticationSaslFinal(body) if state.auth_phase == 3 => {
                let mut scram = state.scram.take().ok_or("missing SCRAM state")?;
                scram.finish(body.data()).map_err(error)?;
                state.auth_phase = 4;
            }
            AuthenticationOk if !state.authenticated && [0, 1, 4].contains(&state.auth_phase) => {
                state.authenticated = true;
                state.auth_phase = 5;
            }
            ReadyForQuery(body) if state.authenticated && !state.ready => {
                let status = body.status();
                if ![b'I', b'T', b'E'].contains(&status) {
                    return Err("invalid transaction status".into());
                }
                state.status = status;
                state.ready = true;
                state.querying = false;
            }
            DataRow(body) if state.querying && state.sql_error.is_none() => {
                let mut row = Vec::new();
                let mut ranges = body.ranges();
                while let Some(range) = ranges.next().map_err(error)? {
                    state.cells += 1;
                    state.stored += std::mem::size_of::<Option<String>>()
                        + range.as_ref().map_or(0, |r| r.len());
                    if state.stored > LIMIT {
                        return Err("result too large".into());
                    }
                    row.push(
                        range
                            .map(|r| {
                                std::str::from_utf8(&body.buffer()[r])
                                    .map(str::to_owned)
                                    .map_err(error)
                            })
                            .transpose()?,
                    );
                }
                state.stored += std::mem::size_of::<Vec<Option<String>>>();
                if state.stored > LIMIT {
                    return Err("result too large".into());
                }
                state.rows.push(row);
            }
            ErrorResponse(body) => {
                let mut fields = body.fields();
                let mut code = String::new();
                let mut message = String::new();
                while let Some(field) = fields.next().map_err(error)? {
                    match field.type_() {
                        b'C' => {
                            code = std::str::from_utf8(field.value_bytes())
                                .map_err(error)?
                                .into()
                        }
                        b'M' => {
                            message = std::str::from_utf8(field.value_bytes())
                                .map_err(error)?
                                .into()
                        }
                        _ => {}
                    }
                }
                let e = format!("PostgreSQL {code}: {message}");
                if !state.querying {
                    return Err(e);
                }
                state.sql_error = Some(e);
            }
            ParameterStatus(_) | BackendKeyData(_) | NoticeResponse(_) => {}
            ParseComplete | BindComplete | RowDescription(_) | NoData | CommandComplete(_)
            | EmptyQueryResponse
                if state.querying => {}
            _ => return Err("unsupported or out-of-order PostgreSQL message".into()),
        }
    }
    Ok(out.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![tag];
        bytes.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
        bytes.extend_from_slice(body);
        bytes
    }
    fn ready() -> Box<Pg> {
        let pg = Pg::new();
        (pg).feed(&frame(b'R', &0_i32.to_be_bytes()), "", "")
            .unwrap();
        (pg).feed(&frame(b'Z', b"I"), "", "").unwrap();
        pg
    }
    fn row(values: &[Option<&str>]) -> Vec<u8> {
        let mut body = (values.len() as u16).to_be_bytes().to_vec();
        for value in values {
            match value {
                Some(v) => {
                    body.extend_from_slice(&(v.len() as i32).to_be_bytes());
                    body.extend_from_slice(v.as_bytes());
                }
                None => body.extend_from_slice(&(-1_i32).to_be_bytes()),
            }
        }
        frame(b'D', &body)
    }
    #[test]
    fn pg_startup_and_ssl_encoding() {
        assert_eq!(Pg::ssl_request(), [0, 0, 0, 8, 4, 210, 22, 47]);
        let bytes = Pg::start("alice", "demo").unwrap();
        assert_eq!(&bytes[4..8], &[0, 3, 0, 0]);
        assert!(bytes.windows(6).any(|w| w == b"alice\0"));
        assert!(Pg::start("a\0b", "db").is_err());
        assert!(Pg::start(&"x".repeat(1025), "db").is_err());
    }
    #[test]
    fn pg_password_and_md5_authentication() {
        let pg = Pg::new();
        let reply = (pg)
            .feed(&frame(b'R', &3_i32.to_be_bytes()), "user", "secret")
            .unwrap();
        assert_eq!(reply, frame(b'p', b"secret\0"));
        let mut auth = 5_i32.to_be_bytes().to_vec();
        auth.extend_from_slice(&[1, 2, 3, 4]);
        let pg = Pg::new();
        let reply = (pg).feed(&frame(b'R', &auth), "user", "secret").unwrap();
        assert_eq!(&reply[5..8], b"md5");
        assert_eq!(reply.len(), 41);
        assert!(
            (Pg::new())
                .feed(&frame(b'R', &10_i32.to_be_bytes()), "", "")
                .is_err()
        );
        assert!((Pg::new()).feed(&frame(b'Z', b"I"), "", "").is_err());
    }
    #[test]
    fn pg_extended_query_parameters_are_separate_from_sql() {
        let pg = ready();
        let params = Params::new();
        let injection = "'; DROP TABLE users; -- 日本語";
        (params).add(Some(injection)).unwrap();
        (params).add(None).unwrap();
        let wire = (pg).query("SELECT $1::text, $2::text", &params).unwrap();
        let parse_length = u32::from_be_bytes(wire[1..5].try_into().unwrap()) as usize + 1;
        assert!(
            !wire[..parse_length]
                .windows(injection.len())
                .any(|w| w == injection.as_bytes())
        );
        assert_eq!(wire[parse_length], b'B');
        assert!(
            wire[parse_length..]
                .windows(injection.len())
                .any(|w| w == injection.as_bytes())
        );
        assert_eq!(&wire[wire.len() - 5..], &[b'S', 0, 0, 0, 4]);
        assert!((pg).query("SELECT 2", &params).is_err());
    }
    #[test]
    fn pg_parameter_and_query_limits() {
        let params = Params::new();
        assert!((params).add(Some(&"x".repeat(1025))).is_err());
        for _ in 0..1024 {
            (params).add(None).unwrap();
        }
        assert!((params).add(None).is_err());
        let pg = ready();
        assert!((pg).query(&"x".repeat(65537), &params).is_err());
        assert!((pg).query("bad\0sql", &params).is_err());
        assert!((pg).ready().unwrap());
    }
    #[test]
    fn pg_rows_null_unicode_empty_and_index_bounds() {
        let pg = ready();
        (pg).query("SELECT $1", &Params::new()).unwrap();
        (pg).feed(&row(&[Some("日本語🙂"), None, Some("")]), "", "")
            .unwrap();
        assert!((pg).cell(0, 0).is_err());
        (pg).feed(&frame(b'Z', b"I"), "", "").unwrap();
        assert_eq!((pg).result().unwrap(), 1);
        assert_eq!((pg).cell(0, 0).unwrap().as_deref(), Some("日本語🙂"));
        assert_eq!((pg).cell(0, 1).unwrap(), None);
        assert_eq!((pg).cell(0, 2).unwrap().as_deref(), Some(""));
        for (r, c) in [(-1, 0), (0, -1), (1, 0), (0, 3)] {
            assert!((pg).cell(r, c).is_err());
        }
    }
    #[test]
    fn pg_sql_error_drains_and_transaction_recovers() {
        let pg = ready();
        let params = Params::new();
        (pg).query("BEGIN", &params).unwrap();
        (pg).feed(&frame(b'Z', b"T"), "", "").unwrap();
        assert_eq!((pg).status().unwrap(), i64::from(b'T'));
        (pg).query("SELECT 1/0", &params).unwrap();
        (pg).feed(&frame(b'E', b"C22012\0Mdivision by zero\0\0"), "", "")
            .unwrap();
        assert!(!(pg).ready().unwrap());
        (pg).feed(&frame(b'Z', b"E"), "", "").unwrap();
        assert!((pg).result().unwrap_err().contains("22012"));
        assert!((pg).cell(0, 0).is_err());
        (pg).query("ROLLBACK", &params).unwrap();
        (pg).feed(&frame(b'Z', b"I"), "", "").unwrap();
        assert_eq!((pg).result().unwrap(), 0);
        assert_eq!((pg).status().unwrap(), i64::from(b'I'));
        assert!(
            (Pg::new())
                .feed(&frame(b'E', b"C28P01\0Mbad password\0\0"), "", "")
                .unwrap_err()
                .contains("28P01")
        );
    }
    #[test]
    fn pg_rejects_bad_frames_and_poisoned_session() {
        for bytes in [
            vec![b'D', 0, 0, 0, 3],
            vec![b'D', 127, 255, 255, 255],
            frame(b'?', b""),
            frame(b'Z', b"?"),
        ] {
            let pg = ready();
            (pg).query("SELECT 1", &Params::new()).unwrap();
            assert!((pg).feed(&bytes, "", "").is_err());
            assert!((pg).ready().is_err());
            assert!((pg).result().is_err());
            assert!((pg).feed(&[], "", "").is_err());
            assert!((pg).query("SELECT 1", &Params::new()).is_err());
        }
        let pg = ready();
        (pg).query("SELECT 1", &Params::new()).unwrap();
        assert!(
            (pg).feed(&frame(b'D', &[0, 1, 0, 0, 0, 1, 255]), "", "")
                .is_err()
        );
        assert!((Pg::new()).feed(&vec![0; 65537], "", "").is_err());
    }
    #[test]
    fn pg_fragmented_and_coalesced_rows_operation_counts() {
        for rows in [1, 16, 256, 2048] {
            for chunk in [1, 7, 65536] {
                let pg = ready();
                (pg).query("SELECT 1", &Params::new()).unwrap();
                let mut wire = Vec::new();
                for _ in 0..rows {
                    wire.extend(row(&[Some("123"), None]));
                }
                wire.extend(frame(b'Z', b"I"));
                for part in wire.chunks(chunk) {
                    (pg).feed(part, "", "").unwrap();
                }
                assert_eq!((pg).result().unwrap(), rows);
                let state = pg.0.try_lock().unwrap();
                assert_eq!(state.messages, rows as usize + 3);
                assert_eq!(state.cells, rows as usize * 2);
                assert_eq!(state.input_bytes, wire.len() + 15);
                assert!(state.framing_checks <= state.messages + wire.len());
                println!(
                    "rows={rows} chunk={chunk} messages={} cells={} stored={}",
                    state.messages, state.cells, state.stored
                );
            }
        }
    }
    #[test]
    fn pg_result_limit_and_try_lock_are_errors() {
        let pg = ready();
        (pg).query("SELECT 1", &Params::new()).unwrap();
        let wire = row(&[Some(&"x".repeat(1000))]);
        let mut rejected = false;
        for _ in 0..1100 {
            if (pg).feed(&wire, "", "").is_err() {
                rejected = true;
                break;
            }
        }
        assert!(rejected);
        let pg = ready();
        let _guard = pg.0.lock().unwrap();
        assert!((pg).ready().is_err());
        let params = Params::new();
        let _guard = params.0.lock().unwrap();
        assert!((params).add(None).is_err());
    }
    #[test]
    fn pg_authentication_replay_and_scram_state_errors() {
        let pg = Pg::new();
        (pg).feed(&frame(b'R', &3_i32.to_be_bytes()), "u", "p")
            .unwrap();
        assert!(
            (pg).feed(&frame(b'R', &3_i32.to_be_bytes()), "u", "p")
                .is_err()
        );
        for stage in [11_i32, 12] {
            let mut body = stage.to_be_bytes().to_vec();
            body.extend_from_slice(b"invalid");
            assert!((Pg::new()).feed(&frame(b'R', &body), "u", "p").is_err());
        }
        let pg = Pg::new();
        let mut body = 10_i32.to_be_bytes().to_vec();
        body.extend_from_slice(b"SCRAM-SHA-256\0\0");
        let reply = (pg).feed(&frame(b'R', &body), "u", "p").unwrap();
        assert!(reply.windows(14).any(|w| w == b"SCRAM-SHA-256\0"));
        // AuthenticationOk cannot skip server proof verification.
        assert!(
            (pg).feed(&frame(b'R', &0_i32.to_be_bytes()), "u", "p")
                .is_err()
        );
        let pg = Pg::new();
        (pg).feed(&frame(b'R', &body), "u", "p").unwrap();
        let mut bad = 11_i32.to_be_bytes().to_vec();
        bad.extend_from_slice(b"r=wrong,s=YQ==,i=4096");
        assert!((pg).feed(&frame(b'R', &bad), "u", "p").is_err());
    }
    #[test]
    fn pg_wide_rows_and_cell_bytes_operation_counts() {
        for rows in [1, 8] {
            for columns in [1, 16, 256] {
                for width in [0, 16, 128] {
                    let value = "x".repeat(width);
                    let values = vec![Some(value.as_str()); columns];
                    let pg = ready();
                    (pg).query("SELECT 1", &Params::new()).unwrap();
                    let mut bytes = 15;
                    for _ in 0..rows {
                        let frame = row(&values);
                        bytes += frame.len();
                        for part in frame.chunks(7) {
                            (pg).feed(part, "", "").unwrap();
                        }
                    }
                    (pg).feed(&frame(b'Z', b"I"), "", "").unwrap();
                    bytes += 6;
                    let state = pg.0.try_lock().unwrap();
                    assert_eq!(state.messages, rows + 3);
                    assert_eq!(state.cells, rows * columns);
                    assert_eq!(state.input_bytes, bytes);
                    assert_eq!(
                        state.stored,
                        rows * (std::mem::size_of::<Vec<Option<String>>>()
                            + columns * (std::mem::size_of::<Option<String>>() + width))
                    );
                    assert!(state.framing_checks <= bytes + state.messages);
                    println!(
                        "rows={rows} columns={columns} width={width} cells={} bytes={bytes}",
                        state.cells
                    );
                }
            }
        }
    }
}
