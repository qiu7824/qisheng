use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const MAX_READ_BYTES: usize = 16 * 1024;

/// Observes bytes delivered to a server-side WebSocket decoder.
///
/// This state does not grant byte credits or count sockets. It prevents an idle
/// timeout from returning a credit while an incomplete frame or fragmented
/// message remains in the decoder. Malformed input conservatively retains the
/// credit until the decoder rejects the connection.
pub(super) struct ReadTrace {
    state: Mutex<TraceState>,
    retain_credit: AtomicBool,
}

impl ReadTrace {
    fn new() -> Self {
        Self {
            state: Mutex::new(TraceState::new()),
            retain_credit: AtomicBool::new(false),
        }
    }

    pub(super) fn must_retain_credit(&self) -> bool {
        self.retain_credit.load(Ordering::Acquire)
    }

    fn record_read(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        match self.state.lock() {
            Ok(mut state) => {
                state.observe(bytes);
                self.retain_credit
                    .store(state.must_retain_credit(), Ordering::Release);
            }
            Err(_) => self.retain_credit.store(true, Ordering::Release),
        }
    }
}

/// Replays a prefetched HTTP upgrade request and bounds every decoder read.
pub(super) struct TracedStream<S> {
    inner: S,
    prefix: Vec<u8>,
    prefix_offset: usize,
    trace: Arc<ReadTrace>,
}

impl<S> TracedStream<S> {
    pub(super) fn new(inner: S, prefix: Vec<u8>) -> Self {
        Self {
            inner,
            prefix,
            prefix_offset: 0,
            trace: Arc::new(ReadTrace::new()),
        }
    }

    pub(super) fn trace(&self) -> Arc<ReadTrace> {
        Arc::clone(&self.trace)
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for TracedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let capacity = buffer.remaining().min(MAX_READ_BYTES);
        if capacity == 0 {
            return Poll::Ready(Ok(()));
        }
        if this.prefix_offset < this.prefix.len() {
            let end = (this.prefix_offset + capacity).min(this.prefix.len());
            let bytes = &this.prefix[this.prefix_offset..end];
            buffer.put_slice(bytes);
            this.trace.record_read(bytes);
            this.prefix_offset = end;
            if this.prefix_offset == this.prefix.len() {
                this.prefix = Vec::new();
                this.prefix_offset = 0;
            }
            return Poll::Ready(Ok(()));
        }

        let mut limited = ReadBuf::new(buffer.initialize_unfilled_to(capacity));
        match Pin::new(&mut this.inner).poll_read(cx, &mut limited) {
            Poll::Ready(Ok(())) => {
                let count = limited.filled().len();
                this.trace.record_read(limited.filled());
                buffer.advance(count);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for TracedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, bytes)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, buffers)
    }
}

struct TraceState {
    phase: Phase,
    fragmented_opcode: Option<u8>,
}

enum Phase {
    Http { matched: u8, started: bool },
    Header(FrameHeader),
    Payload { frame: Frame, remaining: u64 },
    Malformed,
}

#[derive(Clone, Copy)]
struct Frame {
    fin: bool,
    opcode: u8,
    payload_length: u64,
}

struct FrameHeader {
    bytes: [u8; 14],
    length: usize,
    expected: usize,
}

impl FrameHeader {
    fn new() -> Self {
        Self {
            bytes: [0; 14],
            length: 0,
            expected: 2,
        }
    }

    fn configure(&mut self) -> Result<(), ()> {
        let first = self.bytes[0];
        let second = self.bytes[1];
        let opcode = first & 0x0f;
        let short_length = second & 0x7f;
        if first & 0x70 != 0 || second & 0x80 == 0 {
            return Err(());
        }
        if !matches!(opcode, 0 | 1 | 2 | 8 | 9 | 10) {
            return Err(());
        }
        if opcode >= 8 && (first & 0x80 == 0 || short_length > 125) {
            return Err(());
        }
        self.expected =
            2 + match short_length {
                126 => 2,
                127 => 8,
                _ => 0,
            } + 4;
        Ok(())
    }

    fn finish(&self, fragmented: bool) -> Result<Frame, ()> {
        let marker = self.bytes[1] & 0x7f;
        let payload_length = match marker {
            126 => u64::from(u16::from_be_bytes([self.bytes[2], self.bytes[3]])),
            127 => {
                let mut encoded = [0; 8];
                encoded.copy_from_slice(&self.bytes[2..10]);
                u64::from_be_bytes(encoded)
            }
            length => u64::from(length),
        };
        if (marker == 126 && payload_length < 126)
            || (marker == 127 && (payload_length < 65_536 || payload_length >> 63 != 0))
        {
            return Err(());
        }
        let opcode = self.bytes[0] & 0x0f;
        if (opcode == 0 && !fragmented) || (matches!(opcode, 1 | 2) && fragmented) {
            return Err(());
        }
        Ok(Frame {
            fin: self.bytes[0] & 0x80 != 0,
            opcode,
            payload_length,
        })
    }
}

impl TraceState {
    fn new() -> Self {
        Self {
            phase: Phase::Http {
                matched: 0,
                started: false,
            },
            fragmented_opcode: None,
        }
    }

    fn must_retain_credit(&self) -> bool {
        self.fragmented_opcode.is_some()
            || match &self.phase {
                Phase::Http { started, .. } => *started,
                Phase::Header(header) => header.length != 0,
                Phase::Payload { .. } | Phase::Malformed => true,
            }
    }

    fn complete_frame(&mut self, frame: Frame) {
        match frame.opcode {
            1 | 2 if !frame.fin => self.fragmented_opcode = Some(frame.opcode),
            0 if frame.fin => self.fragmented_opcode = None,
            _ => {}
        }
        self.phase = Phase::Header(FrameHeader::new());
    }

    fn observe(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            match &mut self.phase {
                Phase::Http { matched, started } => {
                    *started = true;
                    *matched = match (*matched, bytes[0]) {
                        (0 | 1 | 3, b'\r') => 1,
                        (1, b'\n') => 2,
                        (2, b'\r') => 3,
                        (3, b'\n') => 4,
                        _ => 0,
                    };
                    bytes = &bytes[1..];
                    if *matched == 4 {
                        self.phase = Phase::Header(FrameHeader::new());
                    }
                }
                Phase::Header(header) => {
                    let count = (header.expected - header.length).min(bytes.len());
                    header.bytes[header.length..header.length + count]
                        .copy_from_slice(&bytes[..count]);
                    header.length += count;
                    bytes = &bytes[count..];
                    if header.length < header.expected {
                        continue;
                    }
                    if header.length == 2 {
                        if header.configure().is_err() {
                            self.phase = Phase::Malformed;
                        }
                        continue;
                    }
                    let frame = match header.finish(self.fragmented_opcode.is_some()) {
                        Ok(frame) => frame,
                        Err(()) => {
                            self.phase = Phase::Malformed;
                            continue;
                        }
                    };
                    if frame.payload_length == 0 {
                        self.complete_frame(frame);
                    } else {
                        self.phase = Phase::Payload {
                            frame,
                            remaining: frame.payload_length,
                        };
                    }
                }
                Phase::Payload { frame, remaining } => {
                    let count = (*remaining).min(bytes.len() as u64) as usize;
                    *remaining -= count as u64;
                    bytes = &bytes[count..];
                    if *remaining == 0 {
                        let completed = *frame;
                        self.complete_frame(completed);
                    }
                }
                Phase::Malformed => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Waker;

    const HTTP: &[u8] = b"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\r\n";

    fn ready_trace() -> ReadTrace {
        let trace = ReadTrace::new();
        trace.record_read(HTTP);
        assert!(!trace.must_retain_credit());
        trace
    }

    fn frame(opcode: u8, fin: bool, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![opcode | if fin { 0x80 } else { 0 }];
        match payload.len() {
            0..=125 => bytes.push(0x80 | payload.len() as u8),
            126..=65_535 => {
                bytes.push(0x80 | 126);
                bytes.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            }
            _ => {
                bytes.push(0x80 | 127);
                bytes.extend_from_slice(&(payload.len() as u64).to_be_bytes());
            }
        }
        let mask = [0x11, 0x22, 0x33, 0x44];
        bytes.extend_from_slice(&mask);
        bytes.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        bytes
    }

    #[test]
    fn http_terminator_survives_one_byte_reads_and_overlap() {
        let trace = ReadTrace::new();
        assert!(!trace.must_retain_credit());
        let request = b"GET / HTTP/1.1\r\r\nHeader: value\r\n\r\n";
        for (index, byte) in request.iter().enumerate() {
            trace.record_read(std::slice::from_ref(byte));
            assert_eq!(trace.must_retain_credit(), index + 1 != request.len());
        }
        trace.record_read(&frame(9, true, b"ping"));
        assert!(!trace.must_retain_credit());
    }

    #[test]
    fn mask_and_short_extended_lengths_survive_one_byte_headers() {
        for length in [1, 125, 126, 65_535, 65_536] {
            let trace = ready_trace();
            let payload = vec![b'a'; length];
            let bytes = frame(1, true, &payload);
            let header_length = bytes.len() - length;
            for byte in &bytes[..header_length] {
                trace.record_read(std::slice::from_ref(byte));
                assert!(trace.must_retain_credit(), "length {length}");
            }
            trace.record_read(&bytes[header_length..bytes.len() - 1]);
            assert!(trace.must_retain_credit(), "length {length}");
            trace.record_read(&bytes[bytes.len() - 1..]);
            assert!(!trace.must_retain_credit(), "length {length}");
        }
    }

    #[test]
    fn fragmented_text_retains_credit_across_interleaved_control_frames() {
        let trace = ready_trace();
        trace.record_read(&frame(1, false, b"first"));
        assert!(trace.must_retain_credit());
        trace.record_read(&frame(9, true, b"ping"));
        assert!(trace.must_retain_credit());
        trace.record_read(&frame(0, false, b"middle"));
        assert!(trace.must_retain_credit());
        trace.record_read(&frame(10, true, b"pong"));
        assert!(trace.must_retain_credit());
        let final_frame = frame(0, true, b"last");
        for byte in &final_frame[..final_frame.len() - 1] {
            trace.record_read(std::slice::from_ref(byte));
            assert!(trace.must_retain_credit());
        }
        trace.record_read(&final_frame[final_frame.len() - 1..]);
        assert!(!trace.must_retain_credit());
    }

    #[test]
    fn empty_fragments_and_completed_ping_have_distinct_retention() {
        let trace = ready_trace();
        trace.record_read(&frame(9, true, b""));
        assert!(!trace.must_retain_credit());
        trace.record_read(&frame(2, false, b""));
        assert!(trace.must_retain_credit());
        trace.record_read(&frame(9, true, b""));
        assert!(trace.must_retain_credit());
        trace.record_read(&frame(0, true, b""));
        assert!(!trace.must_retain_credit());
    }

    #[test]
    fn partial_ping_retains_credit_until_its_payload_finishes() {
        let trace = ready_trace();
        let bytes = frame(9, true, b"ping");
        trace.record_read(&bytes[..bytes.len() - 1]);
        assert!(trace.must_retain_credit());
        trace.record_read(&bytes[bytes.len() - 1..]);
        assert!(!trace.must_retain_credit());
    }

    #[test]
    fn malformed_headers_keep_retention_sticky() {
        let mut nonminimal = vec![0x81, 0xfe, 0, 1];
        nonminimal.extend_from_slice(&[0; 4]);
        let mut high_bit = vec![0x81, 0xff, 0x80, 0, 0, 0, 0, 0, 0, 0];
        high_bit.extend_from_slice(&[0; 4]);
        for invalid in [
            vec![0x81, 1],
            vec![0xc1, 0x80],
            vec![0x83, 0x80],
            vec![0x09, 0x80],
            vec![0x89, 0xfe],
            frame(0, true, b"orphan"),
            nonminimal,
            high_bit,
        ] {
            let trace = ready_trace();
            trace.record_read(&invalid);
            assert!(trace.must_retain_credit());
            trace.record_read(&frame(9, true, b""));
            assert!(trace.must_retain_credit());
        }
        let trace = ready_trace();
        trace.record_read(&frame(1, false, b"open"));
        trace.record_read(&frame(2, true, b"nested"));
        trace.record_read(&frame(0, true, b"finish"));
        assert!(trace.must_retain_credit());
    }

    struct MockStream {
        bytes: Vec<u8>,
        offset: usize,
        read_capacities: Vec<usize>,
        writes: Vec<u8>,
        flushes: usize,
        shutdowns: usize,
    }

    impl MockStream {
        fn new(bytes: Vec<u8>) -> Self {
            Self {
                bytes,
                offset: 0,
                read_capacities: Vec::new(),
                writes: Vec::new(),
                flushes: 0,
                shutdowns: 0,
            }
        }
    }

    impl AsyncRead for MockStream {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            this.read_capacities.push(buffer.remaining());
            let count = buffer.remaining().min(this.bytes.len() - this.offset);
            buffer.put_slice(&this.bytes[this.offset..this.offset + count]);
            this.offset += count;
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for MockStream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.get_mut().writes.extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.get_mut().flushes += 1;
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.get_mut().shutdowns += 1;
            Poll::Ready(Ok(()))
        }
    }

    fn read_once(stream: &mut TracedStream<MockStream>, destination: &mut [u8]) -> usize {
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut buffer = ReadBuf::new(destination);
        assert!(matches!(
            Pin::new(stream).poll_read(&mut context, &mut buffer),
            Poll::Ready(Ok(()))
        ));
        buffer.filled().len()
    }

    #[test]
    fn prefetched_http_and_early_data_are_traced_when_delivered() {
        let opening = frame(1, false, b"first");
        let mut prefix = HTTP.to_vec();
        prefix.extend_from_slice(&opening[..7]);
        let mut following = opening[7..].to_vec();
        following.extend_from_slice(&frame(9, true, b"ping"));
        following.extend_from_slice(&frame(0, true, b"last"));
        let mut stream = TracedStream::new(MockStream::new(following), prefix.clone());
        let trace = stream.trace();
        let mut buffer = [0; 256];
        assert!(!trace.must_retain_credit());
        assert_eq!(read_once(&mut stream, &mut buffer), prefix.len());
        assert_eq!(&buffer[..prefix.len()], &prefix);
        assert!(stream.inner.read_capacities.is_empty());
        assert!(trace.must_retain_credit());
        assert!(read_once(&mut stream, &mut buffer) > 0);
        assert!(!trace.must_retain_credit());
    }

    #[test]
    fn prefix_and_underlying_reads_are_each_bounded_to_sixteen_kib() {
        let mut prefix = HTTP.to_vec();
        prefix.extend_from_slice(&frame(2, true, &vec![0; 50_000]));
        let following = frame(2, true, &vec![0; 70_000]);
        let expected_total = prefix.len() + following.len();
        let prefix_length = prefix.len();
        let mut stream = TracedStream::new(MockStream::new(following), prefix);
        let mut buffer = [0; MAX_READ_BYTES * 3];
        let mut total = 0;
        loop {
            let count = read_once(&mut stream, &mut buffer);
            assert!(count <= MAX_READ_BYTES);
            if count == 0 {
                break;
            }
            total += count;
            if total <= prefix_length {
                assert!(stream.inner.read_capacities.is_empty());
            }
        }
        assert_eq!(total, expected_total);
        assert!(!stream.trace().must_retain_credit());
        assert!(
            stream
                .inner
                .read_capacities
                .iter()
                .all(|capacity| *capacity == MAX_READ_BYTES)
        );
    }

    #[test]
    fn writes_flushes_and_shutdown_are_forwarded() {
        let mut stream = TracedStream::new(MockStream::new(Vec::new()), Vec::new());
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(matches!(
            Pin::new(&mut stream).poll_write(&mut context, b"reply"),
            Poll::Ready(Ok(5))
        ));
        assert!(matches!(
            Pin::new(&mut stream).poll_flush(&mut context),
            Poll::Ready(Ok(()))
        ));
        assert!(matches!(
            Pin::new(&mut stream).poll_shutdown(&mut context),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(stream.inner.writes, b"reply");
        assert_eq!(stream.inner.flushes, 1);
        assert_eq!(stream.inner.shutdowns, 1);
    }
}
