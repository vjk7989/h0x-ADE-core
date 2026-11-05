//! Just enough HTTP/1.1 and RFC 6455 for the web panel: GET a page, upgrade
//! to a WebSocket, exchange text frames. Pure functions over bytes.

/// Longest request head (request line and headers).
pub const MAX_HEAD: usize = 8 * 1024;
/// Longest WebSocket message a client may send.
pub const MAX_FRAME: usize = 64 * 1024;
const MAX_HEADERS: usize = 64;
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

#[derive(Debug, PartialEq)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Names in lowercase.
    pub headers: Vec<(String, String)>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// A request head, up to and without the blank line; Err: the status to answer.
pub fn parse(head: &[u8]) -> Result<Request, u16> {
    let text = std::str::from_utf8(head).map_err(|_| 400u16)?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (Some(method), Some(path), Some(version), None) = (first.next(), first.next(), first.next(), first.next())
    else {
        return Err(400);
    };
    if version != "HTTP/1.1" {
        return Err(505);
    }
    if method.is_empty() || !path.starts_with('/') {
        return Err(400);
    }
    let mut headers = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let (name, value) = line.split_once(':').ok_or(400u16)?;
        if name.is_empty() || name.contains(|c: char| c.is_whitespace() || c.is_control()) {
            return Err(400);
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
        if headers.len() > MAX_HEADERS {
            return Err(431);
        }
    }
    if headers.iter().filter(|(n, _)| n == "host").count() != 1 {
        return Err(400);
    }
    Ok(Request { method: method.into(), path: path.into(), headers })
}

/// Whether a request may be served: its Host must be one of ours (no DNS
/// rebinding), an Origin, if any, our own page, and it carries no body.
pub fn allowed(req: &Request, hosts: &[String]) -> Result<(), u16> {
    let ours = |h: &str| hosts.iter().any(|ok| ok.eq_ignore_ascii_case(h));
    if !req.header("host").is_some_and(ours) {
        return Err(403);
    }
    if let Some(origin) = req.header("origin")
        && !origin.strip_prefix("http://").is_some_and(ours)
    {
        return Err(403);
    }
    if req.header("transfer-encoding").is_some() || req.header("content-length").is_some_and(|n| n != "0") {
        return Err(413);
    }
    if req.method != "GET" {
        return Err(405);
    }
    Ok(())
}

pub fn accept_key(key: &str) -> String {
    crate::status::base64(&super::sha::sha1(format!("{key}{GUID}").as_bytes()))
}

fn reason(status: u16) -> &'static str {
    match status {
        101 => "Switching Protocols",
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Content Too Large",
        426 => "Upgrade Required",
        431 => "Request Header Fields Too Large",
        505 => "HTTP Version Not Supported",
        _ => "Error",
    }
}

/// Every answer's headers: no framing, no referrer, nothing from elsewhere.
const SECURITY: &str = "Content-Security-Policy: default-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'\r\n\
    Referrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nCache-Control: no-store\r\n";

/// A whole response; the connection closes after it.
pub fn response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{SECURITY}Connection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    [head.as_bytes(), body].concat()
}

pub fn error(status: u16) -> Vec<u8> {
    response(status, "text/plain; charset=utf-8", reason(status).as_bytes())
}

/// The 101 answer to a WebSocket upgrade, or the status refusing it.
pub fn upgrade(req: &Request) -> Result<Vec<u8>, u16> {
    let has = |name: &str, word: &str| {
        req.header(name).is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(word)))
    };
    if !has("upgrade", "websocket") || !has("connection", "upgrade") {
        return Err(426);
    }
    let key = req.header("sec-websocket-key").ok_or(400u16)?;
    if req.header("sec-websocket-version") != Some("13") || crate::put::decode(key).is_none_or(|k| k.len() != 16) {
        return Err(400);
    }
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n{SECURITY}\r\n",
        accept_key(key)
    );
    Ok(head.into_bytes())
}

#[derive(Debug, PartialEq)]
pub enum Frame {
    Text(String),
    Ping(Vec<u8>),
    Pong,
    Close,
}

/// One client frame from the front of `buf`: the frame and the bytes it
/// took, None until it has all arrived, Err when it breaks the rules (not
/// masked, fragmented, binary, too big, not UTF-8): the connection closes.
pub fn decode(buf: &[u8], max: usize) -> Result<Option<(Frame, usize)>, &'static str> {
    let [b0, b1, ..] = *buf else { return Ok(None) };
    if b0 & 0x70 != 0 {
        return Err("reserved bits set");
    }
    if b0 & 0x80 == 0 {
        return Err("fragmented messages are not accepted");
    }
    if b1 & 0x80 == 0 {
        return Err("client frames must be masked");
    }
    let opcode = b0 & 0x0f;
    let (len, mut at) = match b1 & 0x7f {
        126 => match buf.get(2..4) {
            Some(n) => (u64::from(u16::from_be_bytes([n[0], n[1]])), 4),
            None => return Ok(None),
        },
        127 => match buf.get(2..10) {
            Some(n) => (u64::from_be_bytes(n.try_into().expect("8 bytes")), 10),
            None => return Ok(None),
        },
        n => (u64::from(n), 2),
    };
    if len > max as u64 || (opcode >= 8 && len > 125) {
        return Err("frame too big");
    }
    let len = len as usize;
    let Some(mask) = buf.get(at..at + 4) else { return Ok(None) };
    let mask = [mask[0], mask[1], mask[2], mask[3]];
    at += 4;
    let Some(payload) = buf.get(at..at + len) else { return Ok(None) };
    let data: Vec<u8> = payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]).collect();
    let frame = match opcode {
        1 => Frame::Text(String::from_utf8(data).map_err(|_| "text is not UTF-8")?),
        8 => Frame::Close,
        9 => Frame::Ping(data),
        10 => Frame::Pong,
        2 => return Err("binary frames are not accepted"),
        _ => return Err("unknown opcode"),
    };
    Ok(Some((frame, at + len)))
}

/// A server frame (never masked): text 1, close 8, ping 9, pong 10.
pub fn encode(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x80 | opcode];
    match payload.len() {
        n @ 0..126 => out.push(n as u8),
        n @ 126..65536 => {
            out.push(126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    out.extend_from_slice(payload);
    out
}

/// A masked client frame (tests).
#[cfg(test)]
pub(crate) fn client_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [0x37, 0xfa, 0x21, 0x3d];
    let mut out = encode(opcode, payload);
    let at = out.len() - payload.len();
    out[1] |= 0x80;
    out.splice(at..at, mask);
    for (i, b) in out[at + 4..].iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> Vec<String> {
        vec!["127.0.0.1:8377".into(), "localhost:8377".into(), "192.168.1.20:8377".into()]
    }

    fn req(head: &str) -> Result<Request, u16> {
        parse(head.replace('\n', "\r\n").as_bytes())
    }

    #[test]
    fn the_accept_key_is_rfc_6455s_example() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn requests_parse_strictly() {
        let r = req("GET /ws?x=1 HTTP/1.1\nHost: 127.0.0.1:8377\nUpgrade:  websocket \n").unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str(), r.header("upgrade")), ("GET", "/ws?x=1", Some("websocket")));
        for (bad, status) in [
            ("", 400),
            ("GET / HTTP/1.0\nHost: a", 505),
            ("GET / HTTP/2\nHost: a", 505),
            ("GET  / HTTP/1.1\nHost: a", 400),
            ("GET http://evil/ HTTP/1.1\nHost: a", 400),
            ("GET / HTTP/1.1", 400),
            ("GET / HTTP/1.1\nHost: a\nHost: b", 400),
            ("GET / HTTP/1.1\nHost: a\nno colon", 400),
            ("GET / HTTP/1.1\nHost: a\nbad name: x", 400),
        ] {
            assert_eq!(req(bad), Err(status), "{bad:?}");
        }
        let many = format!("GET / HTTP/1.1\nHost: a\n{}", "X-A: 1\n".repeat(70));
        assert_eq!(req(&many), Err(431));
        assert_eq!(parse(b"GET / HTTP/1.1\r\nHost: \xff"), Err(400));
    }

    #[test]
    fn only_our_host_and_our_own_page_get_through() {
        let ok = |head: &str| allowed(&req(head).unwrap(), &hosts());
        assert_eq!(ok("GET / HTTP/1.1\nHost: 127.0.0.1:8377"), Ok(()));
        assert_eq!(ok("GET / HTTP/1.1\nHost: LOCALHOST:8377\nOrigin: http://localhost:8377"), Ok(()));
        assert_eq!(ok("GET /ws HTTP/1.1\nHost: 192.168.1.20:8377\nOrigin: http://192.168.1.20:8377"), Ok(()));
        for (head, status) in [
            ("GET / HTTP/1.1\nHost: evil.example:8377", 403),
            ("GET / HTTP/1.1\nHost: 127.0.0.1", 403),
            ("GET / HTTP/1.1\nHost: 127.0.0.1:8378", 403),
            ("GET / HTTP/1.1\nHost: 127.0.0.1:8377\nOrigin: http://evil.example", 403),
            ("GET / HTTP/1.1\nHost: 127.0.0.1:8377\nOrigin: https://127.0.0.1:8377", 403),
            ("GET / HTTP/1.1\nHost: 127.0.0.1:8377\nOrigin: null", 403),
            ("GET / HTTP/1.1\nHost: 127.0.0.1:8377\nOrigin: http://127.0.0.1:8377.evil.example", 403),
            ("POST / HTTP/1.1\nHost: 127.0.0.1:8377", 405),
            ("GET / HTTP/1.1\nHost: 127.0.0.1:8377\nContent-Length: 5", 413),
            ("GET / HTTP/1.1\nHost: 127.0.0.1:8377\nTransfer-Encoding: chunked", 413),
        ] {
            assert_eq!(ok(head), Err(status), "{head:?}");
        }
    }

    #[test]
    fn answers_carry_the_security_headers_and_upgrades_check_the_handshake() {
        let page = String::from_utf8(response(200, "text/html", b"<p>")).unwrap();
        for header in [
            "Content-Security-Policy: default-src 'self'; frame-ancestors 'none'",
            "Referrer-Policy: no-referrer",
            "X-Content-Type-Options: nosniff",
            "Content-Length: 3\r\n",
        ] {
            assert!(page.contains(header), "{header}: {page}");
        }
        assert!(!page.to_ascii_lowercase().contains("access-control"), "no CORS");
        let ws = "GET /ws HTTP/1.1\nHost: 127.0.0.1:8377\nUpgrade: websocket\nConnection: keep-alive, Upgrade\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\nSec-WebSocket-Version: 13";
        let answer = String::from_utf8(upgrade(&req(ws).unwrap()).unwrap()).unwrap();
        assert!(
            answer.starts_with("HTTP/1.1 101 ")
                && answer.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        );
        assert_eq!(upgrade(&req("GET /ws HTTP/1.1\nHost: a").unwrap()), Err(426));
        assert_eq!(upgrade(&req(&ws.replace("Version: 13", "Version: 8")).unwrap()), Err(400));
        assert_eq!(upgrade(&req(&ws.replace("dGhlIHNhbXBsZSBub25jZQ==", "c2hvcnQ=")).unwrap()), Err(400));
    }

    #[test]
    fn frames_decode_only_when_whole_masked_and_small() {
        let hi = client_frame(1, b"hello");
        for cut in 0..hi.len() {
            assert_eq!(decode(&hi[..cut], MAX_FRAME), Ok(None), "{cut} bytes");
        }
        assert_eq!(decode(&hi, MAX_FRAME), Ok(Some((Frame::Text("hello".into()), hi.len()))));
        for size in [125, 126, 65535, 65536] {
            let big = client_frame(1, &vec![b'x'; size]);
            assert_eq!(decode(&big, 70_000).unwrap().unwrap().1, big.len(), "{size}");
        }
        assert_eq!(decode(&client_frame(1, &vec![b'x'; MAX_FRAME + 1]), MAX_FRAME), Err("frame too big"));
        // The length alone is enough to refuse: no need to wait for the bytes.
        assert_eq!(decode(&client_frame(1, &vec![b'x'; 70_000])[..14], MAX_FRAME), Err("frame too big"));
        assert_eq!(decode(&client_frame(9, &[1; 126]), MAX_FRAME), Err("frame too big"));
        assert_eq!(decode(&encode(1, b"hi"), MAX_FRAME), Err("client frames must be masked"));
        let mut part = client_frame(1, b"hi");
        part[0] &= 0x7f;
        assert_eq!(decode(&part, MAX_FRAME), Err("fragmented messages are not accepted"));
        assert_eq!(decode(&client_frame(2, b"hi"), MAX_FRAME), Err("binary frames are not accepted"));
        assert_eq!(decode(&client_frame(1, &[0xff, 0xfe]), MAX_FRAME), Err("text is not UTF-8"));
        assert_eq!(decode(&client_frame(9, b"p"), MAX_FRAME), Ok(Some((Frame::Ping(b"p".to_vec()), 7))));
        assert_eq!(decode(&client_frame(8, b""), MAX_FRAME).unwrap().unwrap().0, Frame::Close);
        assert_eq!(encode(1, &[0; 200])[..4], [0x81, 126, 0, 200]);
        assert_eq!(encode(1, &vec![0; 70_000])[..2], [0x81, 127]);
    }
}
