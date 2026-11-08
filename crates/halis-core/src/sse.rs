#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((end, skip)) = find_boundary(&self.buf) {
            let block: Vec<u8> = self.buf.drain(..end + skip).collect();
            if let Some(ev) = parse_block(&block) {
                out.push(ev);
            }
        }
        out
    }
}

fn find_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    if buf.len() < 2 {
        return None;
    }
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if buf[i] == b'\r' && buf[i + 1] == b'\r' {
            return Some((i, 2));
        }
        if i + 3 < buf.len()
            && buf[i] == b'\r'
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

fn parse_block(block: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(block);
    let mut event: Option<String> = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "data" => data_lines.push(value),
            "event" => event = Some(value.to_string()),
            _ => {}
        }
    }
    if data_lines.is_empty() {
        return None;
    }
    let data = data_lines.join("\n");
    if data.is_empty() {
        return None;
    }
    Some(SseEvent { event, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_events_across_chunks() {
        let mut dec = SseDecoder::default();
        let evs = dec.push(b"data: {\"a\":1}\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "{\"a\":1}");
        let evs = dec.push(b"data: {\"b\":2}\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "{\"b\":2}");
    }

    #[test]
    fn emits_when_boundary_arrives_later() {
        let mut dec = SseDecoder::default();
        assert!(dec.push(b"data: half").is_empty());
        let evs = dec.push(b"-line\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "half-line");
    }

    #[test]
    fn handles_crlf_and_split_lines() {
        let mut dec = SseDecoder::default();
        assert!(dec.push(b"event: message_start\r\n").is_empty());
        let evs = dec.push(b"data: {\"x\":1}\r\n\r\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("message_start"));
        assert_eq!(evs[0].data, "{\"x\":1}");
    }

    #[test]
    fn joins_multi_data_lines() {
        let mut dec = SseDecoder::default();
        let evs = dec.push(b"data: line1\ndata: line2\n\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "line1\nline2");
    }

    #[test]
    fn ignores_comments_and_heartbeats() {
        let mut dec = SseDecoder::default();
        assert!(dec.push(b": keepalive\n\n").is_empty());
        let evs = dec.push(b": comment\nevent: ping\ndata:\n\n");
        assert!(evs.is_empty());
    }

    #[test]
    fn event_without_data_is_skipped() {
        let mut dec = SseDecoder::default();
        assert!(dec.push(b"event: done\n\n").is_empty());
    }
}
