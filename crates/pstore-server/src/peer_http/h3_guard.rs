//! Bound encoded frames before h3 buffers them to decode QPACK.
use bytes::Bytes;
use h3::quic::{self, ConnectionErrorIncoming, StreamErrorIncoming, StreamId, WriteBuf};
use std::task::{Context, Poll};

const CONTROL_FRAME: u64 = 32 * 1024;
const DATA_FRAME: u64 = crate::peer_wire::MAX_FRAME as u64 + 4;

#[derive(Clone, Copy, Default)]
struct Integer {
    value: u64,
    read: u8,
    width: u8,
}
impl Integer {
    fn push(&mut self, byte: u8) -> Option<u64> {
        if self.read == 0 {
            self.width = 1 << (byte >> 6);
            self.value = u64::from(byte & 0x3f);
        } else {
            self.value = (self.value << 8) | u64::from(byte);
        }
        self.read += 1;
        (self.read == self.width).then_some(self.value)
    }
}

#[derive(Clone, Copy)]
enum State {
    Uni(Integer),
    Kind(Integer),
    Length(u64, Integer),
    Payload(u64),
    Opaque,
    Failed,
}

#[derive(Debug)]
pub(super) struct BoundExceeded;
pub(super) struct Frames {
    state: State,
}
impl Frames {
    pub(super) fn bidirectional() -> Self {
        Self {
            state: State::Kind(Integer::default()),
        }
    }
    pub(super) fn unidirectional() -> Self {
        Self {
            state: State::Uni(Integer::default()),
        }
    }
    pub(super) fn push(&mut self, mut bytes: &[u8]) -> Result<(), BoundExceeded> {
        if matches!(self.state, State::Failed) {
            return Err(BoundExceeded);
        }
        while let Some((&byte, rest)) = bytes.split_first() {
            self.state = match self.state {
                State::Failed => return Err(BoundExceeded),
                State::Opaque => return Ok(()),
                State::Payload(left) => {
                    let consumed = left.min(bytes.len() as u64) as usize;
                    bytes = bytes.get(consumed..).ok_or(BoundExceeded)?;
                    let remaining = left - consumed as u64;
                    if remaining == 0 {
                        State::Kind(Integer::default())
                    } else {
                        State::Payload(remaining)
                    }
                }
                State::Uni(mut value) => {
                    bytes = rest;
                    match value.push(byte) {
                        Some(0) => State::Kind(Integer::default()),
                        Some(_) => State::Opaque,
                        None => State::Uni(value),
                    }
                }
                State::Kind(mut value) => {
                    bytes = rest;
                    match value.push(byte) {
                        Some(kind) => State::Length(kind, Integer::default()),
                        None => State::Kind(value),
                    }
                }
                State::Length(kind, mut value) => {
                    bytes = rest;
                    match value.push(byte) {
                        Some(size) if size > if kind == 0 { DATA_FRAME } else { CONTROL_FRAME } => {
                            self.state = State::Failed;
                            return Err(BoundExceeded);
                        }
                        Some(0) => State::Kind(Integer::default()),
                        Some(size) => State::Payload(size),
                        None => State::Length(kind, value),
                    }
                }
            };
        }
        Ok(())
    }
}

pub(super) struct Checked<S> {
    inner: S,
    frames: Frames,
}
impl<S> Checked<S> {
    fn bidi(inner: S) -> Self {
        Self {
            inner,
            frames: Frames::bidirectional(),
        }
    }
    fn uni(inner: S) -> Self {
        Self {
            inner,
            frames: Frames::unidirectional(),
        }
    }
}

impl<S: quic::RecvStream<Buf = Bytes>> quic::RecvStream for Checked<S> {
    type Buf = Bytes;
    fn poll_data(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Bytes>, StreamErrorIncoming>> {
        match self.inner.poll_data(cx) {
            Poll::Ready(Ok(Some(bytes))) => {
                if self.frames.push(&bytes).is_err() {
                    let error_code = h3::error::Code::H3_EXCESSIVE_LOAD.value();
                    self.inner.stop_sending(error_code);
                    Poll::Ready(Err(StreamErrorIncoming::StreamTerminated { error_code }))
                } else {
                    Poll::Ready(Ok(Some(bytes)))
                }
            }
            other => other,
        }
    }
    fn stop_sending(&mut self, code: u64) {
        self.inner.stop_sending(code);
    }
    fn recv_id(&self) -> StreamId {
        self.inner.recv_id()
    }
}

impl<S: quic::SendStream<Bytes>> quic::SendStream<Bytes> for Checked<S> {
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        self.inner.poll_ready(cx)
    }
    fn send_data<T: Into<WriteBuf<Bytes>>>(&mut self, data: T) -> Result<(), StreamErrorIncoming> {
        self.inner.send_data(data)
    }
    fn poll_finish(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), StreamErrorIncoming>> {
        self.inner.poll_finish(cx)
    }
    fn reset(&mut self, code: u64) {
        self.inner.reset(code);
    }
    fn send_id(&self) -> StreamId {
        self.inner.send_id()
    }
}

impl<S> quic::BidiStream<Bytes> for Checked<S>
where
    S: quic::BidiStream<Bytes> + quic::RecvStream<Buf = Bytes>,
    S::RecvStream: quic::RecvStream<Buf = Bytes>,
{
    type SendStream = S::SendStream;
    type RecvStream = Checked<S::RecvStream>;
    fn split(self) -> (Self::SendStream, Self::RecvStream) {
        let (send, receive) = self.inner.split();
        (
            send,
            Checked {
                inner: receive,
                frames: self.frames,
            },
        )
    }
}

pub(super) struct Connection {
    inner: h3_quinn::Connection,
}
impl Connection {
    pub(super) fn new(connection: quinn::Connection) -> Self {
        Self {
            inner: h3_quinn::Connection::new(connection),
        }
    }
}
#[derive(Clone)]
pub(super) struct OpenStreams {
    inner: h3_quinn::OpenStreams,
}

impl quic::Connection<Bytes> for Connection {
    type RecvStream = Checked<h3_quinn::RecvStream>;
    type OpenStreams = OpenStreams;
    fn poll_accept_recv(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::RecvStream, ConnectionErrorIncoming>> {
        quic::Connection::<Bytes>::poll_accept_recv(&mut self.inner, cx)
            .map(|result| result.map(Checked::uni))
    }
    fn poll_accept_bidi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::BidiStream, ConnectionErrorIncoming>> {
        quic::Connection::<Bytes>::poll_accept_bidi(&mut self.inner, cx)
            .map(|result| result.map(Checked::bidi))
    }
    fn opener(&self) -> Self::OpenStreams {
        OpenStreams {
            inner: quic::Connection::<Bytes>::opener(&self.inner),
        }
    }
}

impl quic::OpenStreams<Bytes> for Connection {
    type BidiStream = Checked<h3_quinn::BidiStream<Bytes>>;
    type SendStream = h3_quinn::SendStream<Bytes>;
    fn poll_open_bidi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        quic::OpenStreams::<Bytes>::poll_open_bidi(&mut self.inner, cx)
            .map(|result| result.map(Checked::bidi))
    }
    fn poll_open_send(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        quic::OpenStreams::<Bytes>::poll_open_send(&mut self.inner, cx)
    }
    fn close(&mut self, code: h3::error::Code, reason: &[u8]) {
        quic::OpenStreams::<Bytes>::close(&mut self.inner, code, reason);
    }
}

impl quic::OpenStreams<Bytes> for OpenStreams {
    type BidiStream = Checked<h3_quinn::BidiStream<Bytes>>;
    type SendStream = h3_quinn::SendStream<Bytes>;
    fn poll_open_bidi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::BidiStream, StreamErrorIncoming>> {
        quic::OpenStreams::<Bytes>::poll_open_bidi(&mut self.inner, cx)
            .map(|result| result.map(Checked::bidi))
    }
    fn poll_open_send(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Self::SendStream, StreamErrorIncoming>> {
        quic::OpenStreams::<Bytes>::poll_open_send(&mut self.inner, cx)
    }
    fn close(&mut self, code: h3::error::Code, reason: &[u8]) {
        quic::OpenStreams::<Bytes>::close(&mut self.inner, code, reason);
    }
}

#[cfg(test)]
mod tests;
