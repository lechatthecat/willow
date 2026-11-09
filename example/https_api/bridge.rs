//! Socket-free TLS/HTTP bridge. All calls use try_lock and bounded buffers.
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject};
use std::io::{Cursor, Read, Write};
use std::sync::{Arc, Mutex};

const LIMIT: usize = 1024 * 1024;
static TLS_DROPS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
static TLS_CREATED: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
pub struct Tls(Mutex<rustls::Connection>);
pub fn tls_created() -> i64 {
    TLS_CREATED.load(std::sync::atomic::Ordering::SeqCst)
}
fn new_tls(connection: rustls::Connection) -> Box<Tls> {
    TLS_CREATED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Box::new(Tls(Mutex::new(connection)))
}
impl Drop for Tls {
    fn drop(&mut self) {
        TLS_DROPS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}
pub fn tls_drops() -> i64 {
    TLS_DROPS.load(std::sync::atomic::Ordering::SeqCst)
}
fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}
pub fn tls_server(cert: &str, key: &str) -> Result<Box<Tls>, String> {
    if cert.len() > LIMIT || key.len() > LIMIT {
        return Err("certificate too large".into());
    }
    let cert = CertificateDer::from_pem_slice(cert.as_bytes()).map_err(error)?;
    let key = PrivateKeyDer::from_pem_slice(key.as_bytes()).map_err(error)?;
    let config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(error)?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(error)?;
    Ok(new_tls(
        rustls::ServerConnection::new(Arc::new(config))
            .map_err(error)?
            .into(),
    ))
}
pub fn tls_client(cert: &str, name: &str) -> Result<Box<Tls>, String> {
    if cert.len() > LIMIT {
        return Err("certificate too large".into());
    }
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(cert.as_bytes()).map_err(error)?)
        .map_err(error)?;
    let config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(error)?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = ServerName::try_from(name.to_owned()).map_err(error)?;
    Ok(new_tls(
        rustls::ClientConnection::new(Arc::new(config), name)
            .map_err(error)?
            .into(),
    ))
}
pub fn tls_step(tls: &Tls, input: &[u8]) -> Result<Vec<u8>, String> {
    if input.len() > 65536 {
        return Err("TLS input chunk too large".into());
    }
    let mut tls = tls.0.try_lock().map_err(error)?;
    if !input.is_empty() {
        let mut input = Cursor::new(input);
        while input.position() < input.get_ref().len() as u64 {
            if tls.read_tls(&mut input).map_err(error)? == 0 {
                return Err("TLS input made no progress".into());
            }
            tls.process_new_packets().map_err(error)?;
        }
    }
    let mut output = Vec::new();
    while tls.wants_write() {
        tls.write_tls(&mut output).map_err(error)?;
    }
    Ok(output)
}
pub fn tls_ready(tls: &Tls) -> Result<bool, String> {
    Ok(!tls.0.try_lock().map_err(error)?.is_handshaking())
}
pub fn tls_send(tls: &Tls, text: &str) -> Result<Vec<u8>, String> {
    if text.len() > LIMIT {
        return Err("message too large".into());
    }
    let mut tls = tls.0.try_lock().map_err(error)?;
    tls.set_buffer_limit(Some(LIMIT + 65536));
    tls.writer().write_all(text.as_bytes()).map_err(error)?;
    let mut output = Vec::new();
    while tls.wants_write() {
        tls.write_tls(&mut output).map_err(error)?;
    }
    Ok(output)
}
pub fn tls_plain(tls: &Tls) -> Result<Vec<u8>, String> {
    let mut tls = tls.0.try_lock().map_err(error)?;
    let mut output = vec![0; 65536];
    match tls.reader().read(&mut output) {
        Ok(0) => Err("TLS peer closed".into()),
        Ok(n) => {
            output.truncate(n);
            Ok(output)
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Vec::new()),
        Err(e) => Err(error(e)),
    }
}

#[derive(Default)]
struct Message {
    bytes: Vec<u8>,
    scanned: usize,
    body: Option<(usize, usize)>,
    route: String,
}
pub struct Http(Mutex<Message>);
pub fn http_new() -> Box<Http> {
    Box::new(Http(Mutex::new(Message::default())))
}
pub fn http_feed(http: &Http, bytes: &[u8], response: bool) -> Result<bool, String> {
    let mut state = http.0.try_lock().map_err(error)?;
    if bytes.len() > LIMIT.saturating_sub(state.bytes.len()) {
        return Err("HTTP message too large".into());
    }
    state.bytes.extend_from_slice(bytes);
    if state.body.is_none() {
        while state.scanned < state.bytes.len() {
            state.scanned += 1;
            let end = state.scanned;
            if end >= 4 && &state.bytes[end - 4..end] == b"\r\n\r\n" {
                let mut headers = [httparse::EMPTY_HEADER; 64];
                let (route, headers) = if response {
                    let mut message = httparse::Response::new(&mut headers);
                    if !message
                        .parse(&state.bytes[..end])
                        .map_err(error)?
                        .is_complete()
                    {
                        return Err("incomplete response".into());
                    }
                    if message.code != Some(200) {
                        return Err("HTTP response status is not 200".into());
                    }
                    (String::new(), message.headers)
                } else {
                    let mut message = httparse::Request::new(&mut headers);
                    if !message
                        .parse(&state.bytes[..end])
                        .map_err(error)?
                        .is_complete()
                    {
                        return Err("incomplete request".into());
                    }
                    (
                        format!(
                            "{} {}",
                            message.method.unwrap_or(""),
                            message.path.unwrap_or("")
                        ),
                        message.headers,
                    )
                };
                let mut length = None;
                for h in headers {
                    if h.name.eq_ignore_ascii_case("transfer-encoding") {
                        return Err("transfer encoding unsupported".into());
                    }
                    if h.name.eq_ignore_ascii_case("content-length") {
                        if length.is_some() {
                            return Err("duplicate content length".into());
                        }
                        let text = std::str::from_utf8(h.value).map_err(error)?;
                        if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                            return Err("invalid content length".into());
                        }
                        length = Some(text.parse::<usize>().map_err(error)?);
                    }
                }
                let length = length.ok_or("missing content length")?;
                if length > LIMIT - end {
                    return Err("body too large".into());
                }
                state.route = route;
                state.body = Some((end, length));
                break;
            }
            if end >= 16384 {
                return Err("HTTP headers too large".into());
            }
        }
    }
    if let Some((start, length)) = state.body {
        if state.bytes.len() > start + length {
            return Err("unexpected trailing HTTP bytes".into());
        }
        return Ok(state.bytes.len() == start + length);
    }
    Ok(false)
}
pub fn http_body(http: &Http) -> Result<String, String> {
    let state = http.0.try_lock().map_err(error)?;
    let (start, length) = state.body.ok_or("incomplete HTTP headers")?;
    if state.bytes.len() != start + length {
        return Err("incomplete HTTP body".into());
    }
    String::from_utf8(state.bytes[start..].to_vec()).map_err(error)
}
pub fn http_route(http: &Http) -> Result<String, String> {
    Ok(http.0.try_lock().map_err(error)?.route.clone())
}
pub fn http_encode(body: &str, response: bool) -> Result<String, String> {
    if body.len() > LIMIT - 256 {
        return Err("body too large".into());
    }
    let first = if response {
        "HTTP/1.1 200 OK"
    } else {
        "POST /echo HTTP/1.1"
    };
    Ok(format!(
        "{first}\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_framing_boundaries() {
        for body in ["", "{}", "{\"s\":\"日本語🙂\"}"] {
            for response in [false, true] {
                let encoded = http_encode(body, response).unwrap();
                for chunk in [1, 2, 3, 17, 16384] {
                    let state = http_new();
                    let mut complete = false;
                    for bytes in encoded.as_bytes().chunks(chunk) {
                        complete = http_feed(&state, bytes, response).unwrap();
                    }
                    assert!(complete);
                    assert_eq!(http_body(&state).unwrap(), body);
                    assert_eq!(
                        http_route(&state).unwrap(),
                        if response { "" } else { "POST /echo" }
                    );
                }
            }
        }
    }

    #[test]
    fn http_rejects_ambiguous_or_malformed_frames() {
        for bytes in [
            "nonsense\r\n\r\n",
            "POST / HTTP/1.1\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: 0\r\ncontent-length: 0\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: -1\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: +1\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: x\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: 99999999999999999999999999\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: 1048576\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n",
            "POST / HTTP/1.1\r\nContent-Length: 0\r\n\r\ntrailing",
        ] {
            assert!(
                http_feed(&http_new(), bytes.as_bytes(), false).is_err(),
                "{bytes}"
            );
        }
        assert!(
            http_feed(
                &http_new(),
                b"HTTP/1.1 500 Error\r\nContent-Length: 0\r\n\r\n",
                true
            )
            .is_err()
        );
        assert!(http_feed(&http_new(), &vec![b'a'; 16384], false).is_err());
        assert!(http_feed(&http_new(), &vec![0; LIMIT + 1], false).is_err());
        let state = http_new();
        assert!(
            !http_feed(
                &state,
                b"POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\nx",
                false
            )
            .unwrap()
        );
        assert!(http_body(&state).is_err());
        let state = http_new();
        assert!(
            http_feed(
                &state,
                b"POST / HTTP/1.1\r\nContent-Length: 1\r\n\r\n\xff",
                false
            )
            .unwrap()
        );
        assert!(http_body(&state).is_err());
        assert!(http_encode(&"x".repeat(LIMIT), false).is_err());
    }

    #[test]
    fn http_fragmentation_operation_counts_are_linear() {
        // scanned is the actual header-search cursor. Each byte increments it
        // once; body bytes are appended once and never scanned for delimiters.
        for connections in [1, 4, 16] {
            for body_len in [16, 256, 4096] {
                let encoded = http_encode(&"x".repeat(body_len), false).unwrap();
                let header = encoded.len() - body_len;
                let mut scans = 0;
                let mut stored = 0;
                for _ in 0..connections {
                    let state = http_new();
                    for byte in encoded.as_bytes() {
                        http_feed(&state, &[*byte], false).unwrap();
                    }
                    let state = state.0.try_lock().unwrap();
                    scans += state.scanned;
                    stored += state.bytes.len();
                }
                assert_eq!(scans, connections * header);
                assert_eq!(stored, connections * encoded.len());
                println!("connections={connections} body={body_len} scans={scans} stored={stored}");
            }
        }
    }

    #[test]
    fn tls_invalid_configuration_is_result_error() {
        assert!(tls_server("invalid", "invalid").is_err());
        assert!(tls_client("invalid", "localhost").is_err());
        assert!(tls_server(&"x".repeat(LIMIT + 1), "").is_err());
        assert!(tls_client(&"x".repeat(LIMIT + 1), "localhost").is_err());
    }
    #[test]
    fn tls_exchange_and_transfer_counts() {
        let cert = std::fs::read_to_string(
            std::env::var("HTTPS_TEST_CERT")
                .expect("set HTTPS_TEST_CERT to a localhost test certificate"),
        )
        .unwrap();
        let key = std::fs::read_to_string(
            std::env::var("HTTPS_TEST_KEY").expect("set HTTPS_TEST_KEY to its test key"),
        )
        .unwrap();
        for connections in [1, 4, 16] {
            for size in [16, 4096, 65536, 262144] {
                let mut wire_bytes = 0;
                let mut steps = 0;
                for _ in 0..connections {
                    let client = tls_client(&cert, "localhost").unwrap();
                    let server = tls_server(&cert, &key).unwrap();
                    let mut outbound = tls_step(&client, &[]).unwrap();
                    for _ in 0..8 {
                        let reply = tls_step(&server, &outbound).unwrap();
                        outbound = tls_step(&client, &reply).unwrap();
                        if tls_ready(&client).unwrap()
                            && tls_ready(&server).unwrap()
                            && outbound.is_empty()
                        {
                            break;
                        }
                    }
                    assert!(tls_ready(&client).unwrap() && tls_ready(&server).unwrap());
                    let body = "x".repeat(size);
                    let encrypted = tls_send(&client, &body).unwrap();
                    wire_bytes += encrypted.len();
                    let mut plain = Vec::new();
                    for chunk in encrypted.chunks(16384) {
                        steps += 1;
                        tls_step(&server, chunk).unwrap();
                        loop {
                            let part = tls_plain(&server).unwrap();
                            if part.is_empty() {
                                break;
                            }
                            plain.extend(part);
                        }
                    }
                    assert_eq!(plain, body.as_bytes());
                }
                // TLS 1.3 records add 22 bytes, one record per <=16 KiB.
                assert_eq!(wire_bytes, connections * (size + size.div_ceil(16384) * 22));
                assert_eq!(
                    steps,
                    connections * (size + size.div_ceil(16384) * 22).div_ceil(16384)
                );
                println!(
                    "connections={connections} plaintext={size} wire={wire_bytes} steps={steps}"
                );
            }
        }
    }
}
