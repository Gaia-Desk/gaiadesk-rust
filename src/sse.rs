//! Server-Sent Events: an incremental `text/event-stream` parser (the WHATWG
//! rules: fields, comments, blank-line dispatch), fed bytes in any chunking —
//! an event may be split anywhere, a UTF-8 character or a `\r\n` included.

/// One server-sent event: its `event:` name (default `message`) and its
/// `data:` lines joined by `\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub event: String,
    pub data: String,
}

/// Text out of byte chunks that may split a character: what is complete now,
/// keeping an unfinished character for the next chunk.
#[derive(Debug, Default)]
pub(crate) struct Utf8Carry(Vec<u8>);

impl Utf8Carry {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.0.extend_from_slice(bytes);
        match std::str::from_utf8(&self.0) {
            Ok(s) => {
                let s = s.to_string();
                self.0.clear();
                s
            }
            Err(e) if e.error_len().is_none() => {
                let n = e.valid_up_to();
                let s = String::from_utf8_lossy(&self.0[..n]).into_owned();
                self.0.drain(..n);
                s
            }
            Err(_) => String::from_utf8_lossy(&std::mem::take(&mut self.0)).into_owned(),
        }
    }

    /// Whatever is left, lossily.
    pub fn finish(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.0)).into_owned()
    }
}

#[derive(Debug, Default)]
pub(crate) struct SseParser {
    text: Utf8Carry,
    buf: String,
    event: String,
    data: Vec<String>,
}

impl SseParser {
    /// Feed bytes; returns the events they completed.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        let t = self.text.push(bytes);
        self.feed_text(&t)
    }

    fn feed_text(&mut self, text: &str) -> Vec<SseEvent> {
        self.buf.push_str(text);
        let mut out = Vec::new();
        while let Some(i) = self.buf.find(['\r', '\n']) {
            let cr = self.buf.as_bytes()[i] == b'\r';
            // A trailing `\r` may be the first half of `\r\n`: wait for more.
            if cr && i == self.buf.len() - 1 {
                break;
            }
            let len = if cr && self.buf.as_bytes()[i + 1] == b'\n' { 2 } else { 1 };
            let line: String = self.buf[..i].to_string();
            self.buf.drain(..i + len);
            if let Some(ev) = self.line(&line) {
                out.push(ev);
            }
        }
        out
    }

    /// The end of the stream: an event the server did not finish with a blank line is still delivered.
    pub fn end(&mut self) -> Vec<SseEvent> {
        let rest = self.text.finish();
        let mut out = self.feed_text(&rest);
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            if let Some(ev) = self.line(line.strip_suffix('\r').unwrap_or(&line)) {
                out.push(ev);
            }
        }
        if let Some(ev) = self.line("") {
            out.push(ev);
        }
        out
    }

    fn line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            let event = std::mem::take(&mut self.event);
            if self.data.is_empty() {
                return None;
            }
            let data = std::mem::take(&mut self.data).join("\n");
            return Some(SseEvent { event: if event.is_empty() { "message".into() } else { event }, data });
        }
        if line.starts_with(':') {
            return None; // a comment: a keep-alive
        }
        let (field, value) = match line.find(':') {
            Some(i) => (&line[..i], &line[i + 1..]),
            None => (line, ""),
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = value.to_string(),
            "data" => self.data.push(value.to_string()),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(e: &str, d: &str) -> SseEvent {
        SseEvent { event: e.into(), data: d.into() }
    }

    #[test]
    fn events_split_anywhere_crlf_across_chunks_comments_and_multiline_data() {
        let text = ": keep-alive\r\n\r\nevent: stdout\r\ndata: {\"a\":1}\r\n\r\ndata: one\ndata: two\n\nevent:x\ndata\n\n";
        let want = vec![ev("stdout", "{\"a\":1}"), ev("message", "one\ntwo"), ev("x", "")];
        for size in 1..text.len() {
            let mut p = SseParser::default();
            let mut got = Vec::new();
            for chunk in text.as_bytes().chunks(size) {
                got.extend(p.feed(chunk));
            }
            got.extend(p.end());
            assert_eq!(got, want, "chunk size {size}");
        }
    }

    #[test]
    fn an_unterminated_last_event_and_a_split_character() {
        let mut p = SseParser::default();
        let b = "event: stdout\ndata: é\n".as_bytes();
        let mut got = p.feed(&b[..20]);
        got.extend(p.feed(&b[20..]));
        got.extend(p.end());
        assert_eq!(got, vec![ev("stdout", "é")]);
        let mut p = SseParser::default();
        assert!(p.feed(b"data: x\r").is_empty());
        assert_eq!(p.end(), vec![ev("message", "x")]);
    }

    #[test]
    fn utf8_carry_keeps_an_unfinished_character() {
        let mut c = Utf8Carry::default();
        let b = "aé".as_bytes();
        assert_eq!(c.push(&b[..2]), "a");
        assert_eq!(c.push(&b[2..]), "é");
        assert_eq!(c.push(&[0xff, b'x']), "\u{fffd}x");
        c.push(&b[1..2]);
        assert_eq!(c.finish(), "\u{fffd}");
    }
}
