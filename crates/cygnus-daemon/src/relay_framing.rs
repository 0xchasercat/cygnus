//! HTTP/1.1 response framing for the relay.
//!
//! The front opens a fresh upstream connection per request and must know when
//! one response has been fully written so it can end the exchange itself.
//! Asking apps to `connection: close` is not an option: server event loops
//! differ in how they flush large bodies while closing (macOS kqueue backends
//! truncate), and holding sockets open until timeouts wastes every client's
//! time. Framing the response the way any production proxy does removes both
//! problems: count `content-length` bytes, walk chunked framing to the
//! terminal chunk, pass 101 upgrades into tunnel mode, and fall back to
//! close-delimited bodies when a response declares neither.
//!
//! The parser fails open: anything malformed downgrades to close-delimited
//! copying, which can never truncate a response — it only forgoes the early,
//! precise end of exchange.

const MAX_RESPONSE_HEAD_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BodyKind {
    /// No body follows the head (HEAD responses, 204, 304).
    None,
    /// Exactly `remaining` bytes follow the head.
    Length,
    /// Chunked transfer coding follows the head.
    Chunked,
    /// The body ends when the upstream closes.
    ToEof,
}

#[derive(Clone, Debug)]
enum ChunkState {
    /// Accumulating a chunk-size line up to CRLF.
    Size { line: Vec<u8> },
    /// Consuming `remaining` bytes of chunk data.
    Data { remaining: u64 },
    /// Consuming the CR after chunk data.
    DataCr,
    /// Consuming the LF after chunk data.
    DataLf,
    /// Consuming trailers until a blank line.
    Trailers { tail: Vec<u8> },
}

#[derive(Debug)]
pub(crate) struct ResponseFraming {
    request_is_head: bool,
    head: Vec<u8>,
    head_done: bool,
    status: Option<u16>,
    body: BodyKind,
    remaining: u64,
    chunk: ChunkState,
    complete: bool,
    tunnel: bool,
}

impl ResponseFraming {
    pub(crate) fn new(request_is_head: bool) -> Self {
        Self {
            request_is_head,
            head: Vec::new(),
            head_done: false,
            status: None,
            body: BodyKind::ToEof,
            remaining: 0,
            chunk: ChunkState::Size { line: Vec::new() },
            complete: false,
            tunnel: false,
        }
    }

    /// The response status once the status line has been seen.
    pub(crate) fn status(&self) -> Option<u16> {
        self.status
    }

    /// True once exactly one full response has been observed.
    pub(crate) fn complete(&self) -> bool {
        self.complete
    }

    /// True when the exchange upgraded (101): both directions copy freely
    /// until either side closes.
    pub(crate) fn tunnel(&self) -> bool {
        self.tunnel
    }

    /// Observe response bytes as they stream past.
    pub(crate) fn observe(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while !rest.is_empty() && !self.complete && !self.tunnel {
            if !self.head_done {
                rest = self.observe_head(rest);
            } else {
                rest = match self.body {
                    BodyKind::None => {
                        // Nothing should follow; ignore whatever does.
                        &[]
                    }
                    BodyKind::Length => self.observe_length(rest),
                    BodyKind::Chunked => self.observe_chunked(rest),
                    BodyKind::ToEof => &[],
                };
            }
        }
    }

    fn observe_head<'a>(&mut self, bytes: &'a [u8]) -> &'a [u8] {
        let take = bytes
            .len()
            .min(MAX_RESPONSE_HEAD_BYTES.saturating_sub(self.head.len()));
        if take == 0 {
            // Head too large to reason about: fail open to close-delimited.
            self.head_done = true;
            self.body = BodyKind::ToEof;
            return &[];
        }
        self.head.extend_from_slice(&bytes[..take]);
        let Some(end) = find_head_end(&self.head) else {
            return &bytes[take..];
        };
        // Bytes past the head in this read belong to the body.
        let body_offset = take - (self.head.len() - end);
        self.head.truncate(end);
        self.head_done = true;
        self.parse_head();
        &bytes[body_offset..]
    }

    fn parse_head(&mut self) {
        let head = std::mem::take(&mut self.head);
        let status = parse_status(&head);
        self.status = status;
        match status {
            Some(101) => {
                self.tunnel = true;
                return;
            }
            Some(code) if (100..200).contains(&code) => {
                // Interim response: a final head follows. Start over.
                self.head_done = false;
                self.head = Vec::new();
                return;
            }
            _ => {}
        }
        if self.request_is_head || matches!(status, Some(204) | Some(304)) {
            self.body = BodyKind::None;
            self.complete = true;
            return;
        }
        if header_contains_token(&head, b"transfer-encoding", b"chunked") {
            self.body = BodyKind::Chunked;
            self.chunk = ChunkState::Size { line: Vec::new() };
            return;
        }
        match parse_content_length(&head) {
            Some(0) => {
                self.body = BodyKind::None;
                self.complete = true;
            }
            Some(length) => {
                self.body = BodyKind::Length;
                self.remaining = length;
            }
            None => {
                self.body = BodyKind::ToEof;
            }
        }
    }

    fn observe_length<'a>(&mut self, bytes: &'a [u8]) -> &'a [u8] {
        let take = (bytes.len() as u64).min(self.remaining) as usize;
        self.remaining -= take as u64;
        if self.remaining == 0 {
            self.complete = true;
        }
        &bytes[take..]
    }

    fn observe_chunked<'a>(&mut self, mut bytes: &'a [u8]) -> &'a [u8] {
        while !bytes.is_empty() && !self.complete {
            match &mut self.chunk {
                ChunkState::Size { line } => {
                    let Some(position) = bytes.iter().position(|byte| *byte == b'\n') else {
                        line.extend_from_slice(bytes);
                        if line.len() > 1_024 {
                            // Unreasonable size line: fail open.
                            self.body = BodyKind::ToEof;
                        }
                        return &[];
                    };
                    line.extend_from_slice(&bytes[..position]);
                    bytes = &bytes[position + 1..];
                    let size = parse_chunk_size(line);
                    match size {
                        Some(0) => {
                            self.chunk = ChunkState::Trailers { tail: vec![b'\n'] };
                        }
                        Some(size) => {
                            self.chunk = ChunkState::Data { remaining: size };
                        }
                        None => {
                            // Malformed framing: fail open to close-delimited.
                            self.body = BodyKind::ToEof;
                            return &[];
                        }
                    }
                }
                ChunkState::Data { remaining } => {
                    let take = (bytes.len() as u64).min(*remaining) as usize;
                    *remaining -= take as u64;
                    bytes = &bytes[take..];
                    if *remaining == 0 {
                        self.chunk = ChunkState::DataCr;
                    }
                }
                ChunkState::DataCr => {
                    if bytes[0] == b'\r' {
                        bytes = &bytes[1..];
                        self.chunk = ChunkState::DataLf;
                    } else if bytes[0] == b'\n' {
                        bytes = &bytes[1..];
                        self.chunk = ChunkState::Size { line: Vec::new() };
                    } else {
                        self.body = BodyKind::ToEof;
                        return &[];
                    }
                }
                ChunkState::DataLf => {
                    if bytes[0] == b'\n' {
                        bytes = &bytes[1..];
                        self.chunk = ChunkState::Size { line: Vec::new() };
                    } else {
                        self.body = BodyKind::ToEof;
                        return &[];
                    }
                }
                ChunkState::Trailers { tail } => {
                    // The terminal chunk ends after a blank line: either the
                    // immediate CRLF, or trailer lines followed by CRLFCRLF.
                    for (index, byte) in bytes.iter().enumerate() {
                        tail.push(*byte);
                        if *byte == b'\n' {
                            let normalized: Vec<u8> = tail
                                .iter()
                                .copied()
                                .filter(|value| *value != b'\r')
                                .collect();
                            if normalized.ends_with(b"\n\n") {
                                self.complete = true;
                                return &bytes[index + 1..];
                            }
                        }
                        if tail.len() > 16 * 1_024 {
                            self.body = BodyKind::ToEof;
                            return &[];
                        }
                    }
                    return &[];
                }
            }
        }
        bytes
    }
}

fn find_head_end(head: &[u8]) -> Option<usize> {
    head.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
        .or_else(|| {
            head.windows(2)
                .position(|window| window == b"\n\n")
                .map(|position| position + 2)
        })
}

fn parse_status(head: &[u8]) -> Option<u16> {
    let line_end = head.iter().position(|byte| *byte == b'\n')?;
    let line = &head[..line_end];
    let mut parts = line
        .split(|byte| *byte == b' ')
        .filter(|part| !part.is_empty());
    let version = parts.next()?;
    if !version.starts_with(b"HTTP/") {
        return None;
    }
    let code = parts.next()?;
    if code.len() != 3 {
        return None;
    }
    std::str::from_utf8(code).ok()?.parse().ok()
}

fn parse_content_length(head: &[u8]) -> Option<u64> {
    let mut value: Option<u64> = None;
    for line in head.split(|byte| *byte == b'\n') {
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            continue;
        };
        let name = trim_ascii(&line[..colon]);
        if !name.eq_ignore_ascii_case(b"content-length") {
            continue;
        }
        let parsed: u64 = std::str::from_utf8(trim_ascii(&line[colon + 1..]))
            .ok()?
            .parse()
            .ok()?;
        match value {
            Some(existing) if existing != parsed => return None,
            _ => value = Some(parsed),
        }
    }
    value
}

fn header_contains_token(head: &[u8], name: &[u8], token: &[u8]) -> bool {
    for line in head.split(|byte| *byte == b'\n') {
        let Some(colon) = line.iter().position(|byte| *byte == b':') else {
            continue;
        };
        if !trim_ascii(&line[..colon]).eq_ignore_ascii_case(name) {
            continue;
        }
        let value = trim_ascii(&line[colon + 1..]);
        for piece in value.split(|byte| *byte == b',') {
            if trim_ascii(piece).eq_ignore_ascii_case(token) {
                return true;
            }
        }
    }
    false
}

fn parse_chunk_size(line: &[u8]) -> Option<u64> {
    let line = trim_ascii(line);
    let size_part = line
        .split(|byte| *byte == b';')
        .next()
        .map(trim_ascii)
        .filter(|part| !part.is_empty())?;
    u64::from_str_radix(std::str::from_utf8(size_part).ok()?, 16).ok()
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |position| position + 1);
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_in_pieces(framing: &mut ResponseFraming, bytes: &[u8], piece: usize) {
        for chunk in bytes.chunks(piece) {
            framing.observe(chunk);
        }
    }

    #[test]
    fn content_length_completes_exactly_at_the_last_byte() {
        for piece in [1, 3, 7, 4096] {
            let mut framing = ResponseFraming::new(false);
            let response = b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello";
            feed_in_pieces(&mut framing, response, piece);
            assert!(framing.complete(), "piece size {piece}");
            assert_eq!(framing.status(), Some(200));
        }
    }

    #[test]
    fn content_length_zero_and_no_body_statuses_complete_at_head() {
        for response in [
            &b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n"[..],
            &b"HTTP/1.1 204 No Content\r\n\r\n"[..],
            &b"HTTP/1.1 304 Not Modified\r\ncontent-length: 88\r\n\r\n"[..],
        ] {
            let mut framing = ResponseFraming::new(false);
            framing.observe(response);
            assert!(framing.complete(), "{}", String::from_utf8_lossy(response));
        }
    }

    #[test]
    fn head_requests_have_no_body_regardless_of_length() {
        let mut framing = ResponseFraming::new(true);
        framing.observe(b"HTTP/1.1 200 OK\r\ncontent-length: 423883\r\n\r\n");
        assert!(framing.complete());
    }

    #[test]
    fn chunked_completes_at_terminal_chunk() {
        for piece in [1, 2, 5, 4096] {
            let mut framing = ResponseFraming::new(false);
            let response =
                b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
            feed_in_pieces(&mut framing, response, piece);
            assert!(framing.complete(), "piece size {piece}");
        }
    }

    #[test]
    fn chunked_with_trailers_completes_after_blank_line() {
        let mut framing = ResponseFraming::new(false);
        framing.observe(
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\nx-sum: 1\r\n\r\n",
        );
        assert!(framing.complete());
    }

    #[test]
    fn upgrade_switches_to_tunnel() {
        let mut framing = ResponseFraming::new(false);
        framing.observe(b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\n\r\n");
        assert!(framing.tunnel());
        assert!(!framing.complete());
        // Frames after the upgrade never complete the exchange.
        framing.observe(&[0x81, 0x05, b'h', b'e', b'l', b'l', b'o']);
        assert!(!framing.complete());
    }

    #[test]
    fn interim_hundred_continue_is_skipped() {
        let mut framing = ResponseFraming::new(false);
        framing.observe(
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok",
        );
        assert!(framing.complete());
        assert_eq!(framing.status(), Some(200));
    }

    #[test]
    fn missing_framing_headers_stay_open_until_eof() {
        let mut framing = ResponseFraming::new(false);
        framing.observe(b"HTTP/1.1 200 OK\r\n\r\nstreaming forever");
        assert!(!framing.complete());
        assert!(!framing.tunnel());
    }

    #[test]
    fn conflicting_content_lengths_fail_open() {
        let mut framing = ResponseFraming::new(false);
        framing.observe(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\ncontent-length: 9\r\n\r\nhello");
        assert!(!framing.complete());
    }
}
