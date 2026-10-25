//! Incremental decoders for the two stream framings (phase 2.1). Chunk boundaries fall anywhere: a line or event may
//! be split across chunks, and one chunk may hold several. Both decoders tolerate `\r\n`.

/// NDJSON: one JSON value per line.
#[derive(Debug, Default)]
pub struct NdjsonDecoder {
    partial: Vec<u8>,
}

impl NdjsonDecoder {
    /// Adds a chunk and appends every line it completes (without its terminator, empty lines skipped) to `out`.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<Vec<u8>>) {
        let mut rest = chunk;
        while let Some(newline) = rest.iter().position(|&b| b == b'\n') {
            self.partial.extend_from_slice(&rest[..newline]);
            rest = &rest[newline + 1..];
            let mut line = std::mem::take(&mut self.partial);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if !line.iter().all(u8::is_ascii_whitespace) {
                out.push(line);
            }
        }
        self.partial.extend_from_slice(rest);
    }

    /// The last line, if the stream ended without a final newline.
    pub fn finish(&mut self) -> Option<Vec<u8>> {
        let line = std::mem::take(&mut self.partial);
        (!line.iter().all(u8::is_ascii_whitespace)).then_some(line)
    }
}

/// Server-sent events: `data:` lines, joined with `\n`, form one event at a blank line. Other fields (`event:`,
/// `id:`, comments) are ignored; Ollama sends only `data:`.
#[derive(Debug, Default)]
pub struct SseDecoder {
    lines: NdjsonLines,
    data: Option<Vec<u8>>,
}

/// Line splitting for SSE, which, unlike NDJSON, needs the empty lines.
#[derive(Debug, Default)]
struct NdjsonLines {
    partial: Vec<u8>,
}

impl SseDecoder {
    /// Adds a chunk and appends the data of every event it completes to `out`.
    pub fn push(&mut self, chunk: &[u8], out: &mut Vec<Vec<u8>>) {
        let mut rest = chunk;
        while let Some(newline) = rest.iter().position(|&b| b == b'\n') {
            self.lines.partial.extend_from_slice(&rest[..newline]);
            rest = &rest[newline + 1..];
            let mut line = std::mem::take(&mut self.lines.partial);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.line(line, out);
        }
        self.lines.partial.extend_from_slice(rest);
    }

    fn line(&mut self, line: Vec<u8>, out: &mut Vec<Vec<u8>>) {
        if line.is_empty() {
            if let Some(data) = self.data.take() {
                out.push(data);
            }
            return;
        }
        if let Some(value) = line.strip_prefix(b"data:") {
            let value = value.strip_prefix(b" ").unwrap_or(value);
            match &mut self.data {
                Some(data) => {
                    data.push(b'\n');
                    data.extend_from_slice(value);
                }
                None => self.data = Some(value.to_vec()),
            }
        }
    }

    /// An event the stream ended in the middle of, if any.
    pub fn finish(&mut self) -> Option<Vec<u8>> {
        let partial = std::mem::take(&mut self.lines.partial);
        let mut out = Vec::new();
        if !partial.is_empty() {
            self.line(partial, &mut out);
        }
        self.line(Vec::new(), &mut out);
        out.pop()
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const NDJSON: &str = include_str!("../../tests/fixtures/ollama/chat-stream.ndjson");
    const SSE: &str = include_str!("../../tests/fixtures/ollama/openai-chat-stream-usage.sse");

    fn ndjson_in(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut decoder = NdjsonDecoder::default();
        let mut out = Vec::new();
        for chunk in chunks {
            decoder.push(chunk, &mut out);
        }
        out.extend(decoder.finish());
        out
    }

    fn sse_in(chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let mut decoder = SseDecoder::default();
        let mut out = Vec::new();
        for chunk in chunks {
            decoder.push(chunk, &mut out);
        }
        out.extend(decoder.finish());
        out
    }

    /// Splits `text` at the given positions (taken modulo its length, sorted).
    fn split<'a>(text: &'a [u8], cuts: &[usize]) -> Vec<&'a [u8]> {
        let mut cuts: Vec<usize> = cuts.iter().map(|c| c % (text.len() + 1)).collect();
        cuts.sort_unstable();
        let mut pieces = Vec::new();
        let mut start = 0;
        for cut in cuts {
            pieces.push(&text[start..cut]);
            start = cut;
        }
        pieces.push(&text[start..]);
        pieces
    }

    #[test]
    fn ndjson_lines() {
        let lines = ndjson_in(&[NDJSON.as_bytes()]);
        assert_eq!(lines.len(), 10);
        assert!(lines.iter().all(|l| l.starts_with(b"{") && l.ends_with(b"}")));
        assert_eq!(
            ndjson_in(&[b"{\"a\":1}\r\n\r\n{\"b\":", b"2}"]),
            vec![b"{\"a\":1}".to_vec(), b"{\"b\":2}".to_vec()]
        );
    }

    #[test]
    fn sse_events() {
        let events = sse_in(&[SSE.as_bytes()]);
        assert_eq!(events.last().unwrap(), b"[DONE]");
        assert!(events[..events.len() - 1].iter().all(|e| e.starts_with(b"{")));
        assert_eq!(sse_in(&[b"data: a\r\ndata: b\r\n\r\n: comment\n\ndata:c"]), vec![b"a\nb".to_vec(), b"c".to_vec()]);
    }

    proptest! {
        #[test]
        fn any_split_of_a_stream_decodes_the_same(cuts in proptest::collection::vec(0usize..4000, 0..40)) {
            let whole = ndjson_in(&[NDJSON.as_bytes()]);
            prop_assert_eq!(ndjson_in(&split(NDJSON.as_bytes(), &cuts)), whole);
            let whole = sse_in(&[SSE.as_bytes()]);
            prop_assert_eq!(sse_in(&split(SSE.as_bytes(), &cuts)), whole);
        }
    }
}
