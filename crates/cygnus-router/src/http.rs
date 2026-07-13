//! Minimal HTTP/1.1 request-head parsing: enough to route and log a request
//! (method, target, host), not a full parser. The body is never parsed here —
//! once routed, payload bytes are spliced through to the cage (spec §6).

/// Largest request head accepted before the terminating `CRLFCRLF`. A head
/// that grows past this without terminating is treated as malformed rather
/// than buffered without bound.
pub const MAX_HEAD_LEN: usize = 64 * 1024;

/// The parsed head of an HTTP/1.1 request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestHead {
    pub method: String,
    pub target: String,
    /// Routing host: the authority of an absolute-form target if present,
    /// otherwise the `Host` header. `None` if neither is given.
    pub host: Option<String>,
    /// Number of bytes from the start of the buffer through the terminating
    /// `CRLFCRLF`; payload (if any) begins here.
    pub head_len: usize,
}

/// Outcome of parsing a request head from a buffer that may not yet be complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeadParse {
    /// A full head was parsed.
    Complete(RequestHead),
    /// The head is not fully received yet; read more and retry.
    Incomplete,
    /// The bytes are not a valid request head, or the head is too large.
    Malformed,
}

/// Parse the request head at the start of `buf`.
pub fn parse_request_head(buf: &[u8]) -> HeadParse {
    let Some(head_len) = find_head_end(buf) else {
        return if buf.len() > MAX_HEAD_LEN {
            HeadParse::Malformed
        } else {
            HeadParse::Incomplete
        };
    };
    if head_len > MAX_HEAD_LEN {
        return HeadParse::Malformed;
    }

    // The head (request line + headers) is ASCII in any well-formed request.
    let Ok(text) = std::str::from_utf8(&buf[..head_len]) else {
        return HeadParse::Malformed;
    };

    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut fields = request_line.split(' ');
    let (Some(method), Some(target), Some(version)) =
        (fields.next(), fields.next(), fields.next())
    else {
        return HeadParse::Malformed;
    };
    if method.is_empty() || target.is_empty() || !version.starts_with("HTTP/") {
        return HeadParse::Malformed;
    }

    // An absolute-form target carries the authority and takes precedence over
    // the Host header; otherwise fall back to the Host header.
    let mut host = authority_from_target(target);
    if host.is_none() {
        for line in lines {
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.trim().eq_ignore_ascii_case("host")
            {
                host = Some(value.trim().to_owned());
                break;
            }
        }
    }

    HeadParse::Complete(RequestHead {
        method: method.to_owned(),
        target: target.to_owned(),
        host,
        head_len,
    })
}

/// Index just past the first `CRLFCRLF`, or `None` if absent.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|start| start + 4)
}

/// The authority of an absolute-form request target (`http://host/...`).
fn authority_from_target(target: &str) -> Option<String> {
    for scheme in ["http://", "https://"] {
        if let Some(rest) = target.strip_prefix(scheme) {
            let authority = rest.split('/').next().unwrap_or(rest);
            if !authority.is_empty() {
                return Some(authority.to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_simple_request() {
        let raw = b"GET /index.html HTTP/1.1\r\nHost: example.com\r\nAccept: */*\r\n\r\n";
        let HeadParse::Complete(head) = parse_request_head(raw) else {
            panic!("expected a complete head");
        };
        assert_eq!(head.method, "GET");
        assert_eq!(head.target, "/index.html");
        assert_eq!(head.host.as_deref(), Some("example.com"));
        assert_eq!(head.head_len, raw.len());
    }

    #[test]
    fn a_head_without_a_terminator_is_incomplete() {
        let raw = b"GET / HTTP/1.1\r\nHost: example.com\r\n";
        assert_eq!(parse_request_head(raw), HeadParse::Incomplete);
    }

    #[test]
    fn the_host_header_is_case_insensitive() {
        let raw = b"GET / HTTP/1.1\r\nhOsT:   api.example.com  \r\n\r\n";
        let HeadParse::Complete(head) = parse_request_head(raw) else {
            panic!("expected a complete head");
        };
        assert_eq!(head.host.as_deref(), Some("api.example.com"));
    }

    #[test]
    fn an_absolute_target_authority_wins_over_host() {
        let raw = b"GET http://proxy.example.com/x HTTP/1.1\r\nHost: ignored.example.com\r\n\r\n";
        let HeadParse::Complete(head) = parse_request_head(raw) else {
            panic!("expected a complete head");
        };
        assert_eq!(head.host.as_deref(), Some("proxy.example.com"));
    }

    #[test]
    fn a_missing_host_is_none() {
        let raw = b"GET / HTTP/1.0\r\n\r\n";
        let HeadParse::Complete(head) = parse_request_head(raw) else {
            panic!("expected a complete head");
        };
        assert!(head.host.is_none());
    }

    #[test]
    fn a_bad_request_line_is_malformed() {
        assert_eq!(parse_request_head(b"garbage\r\n\r\n"), HeadParse::Malformed);
    }

    #[test]
    fn an_oversized_head_without_a_terminator_is_malformed() {
        let raw = vec![b'a'; MAX_HEAD_LEN + 1];
        assert_eq!(parse_request_head(&raw), HeadParse::Malformed);
    }
}
