use std::{
    cmp, fmt,
    future::Future,
    io::{self, ErrorKind},
    mem,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::{Buf, Bytes, BytesMut};
use futures::{
    future,
    io::{AsyncRead, AsyncWrite},
    ready,
    stream::Stream,
    FutureExt,
};
use http::HeaderMap;
use http_body::Body as HttpBody;
use quinn::SendStream;
use quinn_proto::StreamId;

use crate::{
    connection::ConnectionRef,
    frame::{FrameStream, WriteFrame},
    headers::{DecodeHeaders, SendHeaders},
    proto::{
        frame::{DataFrame, HeadersFrame, HttpFrame},
        headers::Header,
        ErrorCode,
    },
    streams::Reset,
    Error,
};

/// Read the body of a request or response
///
/// This lets you stream a received body through [`AsyncRead`]. You can also use [`data()`] for
/// a less composable, but more efficient way to receive the body.
///
/// It it emitted by [`client::RecvResponse`] and [`server::RecvRequest`] futures.
///
/// This object manages the request nominal termination when originated from [`client::RecvResponse`].
/// You must be careful not to drop it until your client app is done with this request.
///
/// [`AsyncRead`]: https://docs.rs/futures/*/futures/io/trait.AsyncRead.html
/// [`data()`]: #method.data
/// [`client::RecvResponse`]: client/struct.RecvResponse.html
/// [`server::RecvRequest`]: server/struct.RecvRequest.html
pub struct BodyReader {
    recv: Option<FrameStream>,
    trailers: Option<HeadersFrame>,
    conn: ConnectionRef,
    stream_id: StreamId,
    buf: Option<Bytes>,
    finish_request: bool,
}

impl BodyReader {
    pub(crate) fn new(
        recv: FrameStream,
        conn: ConnectionRef,
        stream_id: StreamId,
        finish_request: bool,
    ) -> Self {
        BodyReader {
            conn,
            stream_id,
            finish_request,
            buf: None,
            trailers: None,
            recv: Some(recv),
        }
    }

    /// Receive a chunk of data
    ///
    /// This method is the fastest way of receiving a body's data, as it returns references to the
    /// underlying QUIC reordered data directly. [`AsyncRead`] has an internal buffer and works
    /// by copying it into the user's buffers, which can represent unwanted overhead for some
    /// applications.
    ///
    /// ```
    /// # use anyhow::Result;
    /// # use bytes::Bytes;
    /// # fn do_stuff(bytes: &Bytes) {}
    /// use quinn_h3::BodyReader;
    ///
    /// async fn consume_body(body_reader: &mut BodyReader) -> Result<()> {
    ///     while let Some(result) = body_reader.data().await {
    ///        do_stuff(&result?);
    ///     }
    ///     Ok(())
    /// }
    /// ```
    ///
    /// [`AsyncRead`]: https://docs.rs/futures/*/futures/io/trait.AsyncRead.html
    pub async fn data(&mut self) -> Option<Result<Bytes, Error>> {
        futures_util::future::poll_fn(move |cx| self.poll_read(cx)).await
    }

    /// Try to receive the trailers
    ///
    /// If a trailer block has been received after the body, this method will decode it and
    /// return `Some()`. This value is populated by reading methods: [`data()`] and
    /// `AsyncRead::poll_read()`. So this returns `None` when the body has not been completely
    /// consumed with either of them.
    ///
    /// ```
    /// # use anyhow::Result;
    /// # use bytes::Bytes;
    /// # fn do_stuff(bytes: &Bytes) {}
    /// use futures::AsyncReadExt;
    /// use quinn_h3::BodyReader;
    ///
    /// async fn get_trailers(body_reader: &mut BodyReader) -> Result<()> {
    ///     // Consume the body to the end
    ///     let mut body = String::new();
    ///     body_reader.read_to_string(&mut body).await?;
    ///
    ///     // Get the trailers if any
    ///     if let Some(trailers) = body_reader.trailers().await {
    ///         println!("trailers: {:?}", trailers?);
    ///     }
    ///     Ok(())
    /// }
    /// ```
    ///
    /// [`data()`]: #method.data
    pub async fn trailers(&mut self) -> Option<Result<Header, Error>> {
        let trailers = self.trailers.take();
        let Self {
            conn, stream_id, ..
        } = &self;
        match trailers {
            None => None,
            Some(t) => Some(DecodeHeaders::new(t, conn.clone(), *stream_id).await),
        }
    }

    /// Cancel the request or response associated with this body
    ///
    /// The peer will receive a request error with `REQUEST_CANCELLED` code.
    pub fn cancel(mut self) {
        if let Some(mut recv) = self.recv.take() {
            recv.reset(ErrorCode::REQUEST_CANCELLED);
        }
    }

    #[doc(hidden)]
    pub fn poll_read(&mut self, cx: &mut Context) -> Poll<Option<Result<Bytes, Error>>> {
        if let Some(data) = self.buf.take() {
            return Poll::Ready(Some(Ok(data))); // return buffered data in case user called AsyncRead before
        }

        loop {
            return match Pin::new(self.recv.as_mut().unwrap()).poll_next(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Some(Ok(HttpFrame::Reserved))) => continue,
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Ready(Some(Ok(HttpFrame::Data(d)))) => Poll::Ready(Some(Ok(d.payload))),
                Poll::Ready(Some(Ok(HttpFrame::Headers(d)))) => {
                    self.trailers = Some(d);
                    Poll::Ready(None)
                }
                Poll::Ready(Some(Err(e))) => {
                    self.recv.take().unwrap().reset(e.code());
                    Poll::Ready(Some(Err(e.into())))
                }
                Poll::Ready(Some(Ok(f))) => {
                    self.recv.take().unwrap().reset(ErrorCode::FRAME_UNEXPECTED);
                    Poll::Ready(Some(Err(Error::Peer(format!(
                        "Invalid frame type in body: {:?}",
                        f
                    )))))
                }
            };
        }
    }

    fn buf_read(&mut self, buf: &mut [u8]) -> usize {
        match self.buf {
            None => 0,
            Some(ref mut b) => {
                let size = cmp::min(buf.len(), b.len());
                buf[..size].copy_from_slice(&b.split_to(size));
                if b.is_empty() {
                    self.buf = None;
                }
                size
            }
        }
    }

    fn buf_put(&mut self, buf: Bytes) {
        assert!(self.buf.is_none());
        self.buf = Some(buf)
    }
}

impl AsyncRead for BodyReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        let size = self.buf_read(buf);
        if size == buf.len() {
            return Poll::Ready(Ok(size));
        }

        loop {
            return match Pin::new(self.recv.as_mut().unwrap()).poll_next(cx) {
                Poll::Ready(Some(Ok(HttpFrame::Reserved))) => continue,
                Poll::Ready(None) => Poll::Ready(Ok(size)),
                Poll::Pending => {
                    if size > 0 {
                        Poll::Ready(Ok(size))
                    } else {
                        Poll::Pending
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    self.recv.take().unwrap().reset(e.code());
                    Poll::Ready(Err(io::Error::new(
                        ErrorKind::Other,
                        format!("read error: {:?}", e),
                    )))
                }
                Poll::Ready(Some(Ok(HttpFrame::Data(mut d)))) => {
                    if d.payload.len() >= buf.len() - size {
                        let tail = d.payload.split_off(buf.len() - size);
                        self.buf_put(tail);
                    }
                    buf[size..size + d.payload.len()].copy_from_slice(&d.payload);
                    Poll::Ready(Ok(size + d.payload.len()))
                }
                Poll::Ready(Some(Ok(HttpFrame::Headers(d)))) => {
                    self.trailers = Some(d);
                    Poll::Ready(Ok(size))
                }
                Poll::Ready(Some(Ok(_))) => {
                    self.recv.take().unwrap().reset(ErrorCode::FRAME_UNEXPECTED);
                    Poll::Ready(Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "received an invalid frame type",
                    )))
                }
            };
        }
    }
}

impl tokio::io::AsyncRead for BodyReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        AsyncRead::poll_read(self, cx, buf)
    }
}

impl Drop for BodyReader {
    fn drop(&mut self) {
        if self.finish_request {
            self.conn
                .h3
                .lock()
                .unwrap()
                .inner
                .request_finished(self.stream_id);
        }
    }
}

/// Send the body of a request or response
///
/// Use this to send a body's data through an [`AsyncWrite`] implementation. You can optionally
/// send [`trailers`] once everything is transmitted.
///
/// You need to wait for transfer completion by calling [`close()`], otherwise the transmission integrity
/// is not garanteed.
///
/// This is emitted by [`client::Connection::send_request()`] and [`server::Sender::send_response()`].
///
/// If originating from [`server::Sender::send_response()`], this manages the nominal request
/// termination on drop. Be careful to keep it within scope until your server app is done with this
/// response.
///
/// [`AsyncWrite`]: https://docs.rs/futures/*/futures/io/trait.AsyncWrite.html
/// [`trailers`]: #method.trailers
/// [`close()`]: #method.close
/// [`client::Connection::send_request()`]: ../client/struct.Connection.html#method.send_request
/// [`server::Sender::send_response()`]: ../server/struct.Sender.html#method.send_response
pub struct BodyWriter {
    state: BodyWriterState,
    conn: ConnectionRef,
    stream_id: StreamId,
    finish_request: bool,
}

impl BodyWriter {
    pub(crate) fn new(
        send: SendStream,
        conn: ConnectionRef,
        stream_id: StreamId,
        finish_request: bool,
    ) -> Self {
        Self {
            conn,
            stream_id,
            state: BodyWriterState::Idle(send),
            finish_request,
        }
    }

    /// Send the trailers and terminate request
    ///
    /// ```
    /// # use anyhow::Result;
    /// use futures::AsyncWriteExt;
    /// use http::header::HeaderMap;
    /// use quinn_h3::BodyWriter;
    ///
    /// async fn send_body_and_trailers(mut body_writer: BodyWriter) -> Result<()> {
    ///     body_writer.write_all(b"wait for it").await?;
    ///
    ///     let mut trailers = HeaderMap::new();
    ///     trailers.insert("trailing", "here".parse()?);
    ///     body_writer.trailers(trailers).await?;
    ///    
    ///     Ok(())
    /// }
    /// ```
    pub async fn trailers(mut self, trailers: HeaderMap) -> Result<(), Error> {
        match mem::replace(&mut self.state, BodyWriterState::Finished) {
            BodyWriterState::Idle(send) => {
                let mut stream =
                    SendHeaders::new(Header::trailer(trailers), &self.conn, send, self.stream_id)?
                        .await?;
                stream.finish().await.map_err(Into::into)
            }
            _ => panic!("cannot send trailers while not in idle state"),
        }
    }

    /// Close the sending half of a request after flushing data
    ///
    /// You need to call this method if you want to be sure the body has been completely transmitted.
    pub async fn close(mut self) -> Result<(), Error> {
        futures_util::future::poll_fn(|cx| self.poll_close_inner(cx))
            .await
            .map_err(Into::into)
    }

    /// Cancel the request
    ///
    /// The peer will receive a request error with `REQUEST_CANCELLED` code.
    pub fn cancel(mut self) {
        let state = mem::replace(&mut self.state, BodyWriterState::Finished);
        match state {
            BodyWriterState::Idle(mut send) => {
                send.reset(ErrorCode::REQUEST_CANCELLED.into());
            }
            BodyWriterState::Writing(mut write) => {
                write.reset(ErrorCode::REQUEST_CANCELLED);
            }
            _ => (),
        }
    }

    fn poll_close_inner(&mut self, cx: &mut Context) -> Poll<Result<(), quinn::WriteError>> {
        loop {
            match self.state {
                BodyWriterState::Finished => return Poll::Ready(Ok(())),
                BodyWriterState::Idle(ref mut send) => {
                    ready!(send.finish().poll_unpin(cx))?;
                    self.state = BodyWriterState::Finished;
                    return Poll::Ready(Ok(()));
                }
                BodyWriterState::Writing(ref mut write) => {
                    let send = ready!(Pin::new(write).poll(cx))?;
                    self.state = BodyWriterState::Idle(send);
                }
            }
        }
    }
}

enum BodyWriterState {
    Idle(SendStream),
    Writing(WriteFrame<DataFrame<Bytes>>),
    Finished,
}

impl AsyncWrite for BodyWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        loop {
            match self.state {
                BodyWriterState::Finished => panic!(),
                BodyWriterState::Idle(_) => {
                    let frame = DataFrame {
                        payload: BytesMut::from(buf).freeze(),
                    };
                    self.state = match mem::replace(&mut self.state, BodyWriterState::Finished) {
                        BodyWriterState::Idle(send) => {
                            BodyWriterState::Writing(WriteFrame::new(send, frame))
                        }
                        _ => unreachable!(),
                    }
                }
                BodyWriterState::Writing(ref mut write) => {
                    let send = ready!(Pin::new(write).poll(cx))?;
                    self.state = BodyWriterState::Idle(send);
                    return Poll::Ready(Ok(buf.len()));
                }
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        match self.state {
            BodyWriterState::Finished => Poll::Ready(Ok(())),
            BodyWriterState::Idle(ref mut send) => {
                ready!(Pin::new(send).poll_flush(cx))?;
                self.state = BodyWriterState::Finished;
                Poll::Ready(Ok(()))
            }
            BodyWriterState::Writing(ref mut write) => {
                let send = ready!(Pin::new(write).poll(cx))?;
                self.state = BodyWriterState::Idle(send);
                Poll::Pending
            }
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        self.poll_close_inner(cx).map_err(Into::into)
    }
}

impl tokio::io::AsyncWrite for BodyWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        AsyncWrite::poll_write(self, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        AsyncWrite::poll_flush(self, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), io::Error>> {
        AsyncWrite::poll_close(self, cx)
    }
}

impl Drop for BodyWriter {
    fn drop(&mut self) {
        if self.finish_request {
            self.conn
                .h3
                .lock()
                .unwrap()
                .inner
                .request_finished(self.stream_id);
        }
    }
}

/// Helper struct to build HttpBody from simple types
///
/// This is inteded to be used to build [`Request`] and [`Response`] with a body
/// which conversion to [`Bytes`] is trivial.
///
/// You'll need to use [`IntoBody`] trait to build from a type:
///
/// ```
/// use http::{Response, StatusCode};
/// use quinn_h3::{IntoBody, SimpleBody};
///
/// // Type specification isn't needed, it's here for demonstration.
/// let body: SimpleBody<Bytes> = b"Greetings over HTTP/3".into_body();
/// let response = Response::builder()
///     .status(StatusCode::OK)
///     .header("response", "header")
///     .body(body)?;
/// ```
///
/// [`Request`]: https://docs.rs/http/*/http/request/index.html
/// [`Response`]: https://docs.rs/http/*/http/response/index.html
/// [`Bytes`]: https://docs.rs/bytes/*/bytes/bytes/index.html
/// [`IntoBody`]: trait.IntoBody.html
pub struct SimpleBody<T> {
    inner: Option<T>,
}

impl<T> SimpleBody<T> {
    /// Create an empty body
    pub fn empty() -> SimpleBody<T> {
        Self { inner: None }
    }
}

/// Create an `HttpBody` from a simple type
pub trait IntoBody<T> {
    /// Convert one type to a `SimpleBody`, `HttpBody` implementation
    fn into_body(self) -> SimpleBody<T>;
}

impl IntoBody<Bytes> for Bytes {
    fn into_body(self) -> SimpleBody<Bytes> {
        SimpleBody { inner: Some(self) }
    }
}

impl IntoBody<Bytes> for &str {
    fn into_body(self) -> SimpleBody<Bytes> {
        SimpleBody {
            inner: Some(Bytes::copy_from_slice(self.as_ref())),
        }
    }
}

impl IntoBody<Bytes> for &[u8] {
    fn into_body(self) -> SimpleBody<Bytes> {
        SimpleBody {
            inner: Some(Bytes::copy_from_slice(self)),
        }
    }
}

impl HttpBody for SimpleBody<Bytes> {
    type Data = Bytes;
    type Error = ();
    fn poll_data(
        mut self: Pin<&mut Self>,
        _: &mut Context,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        match self.inner.take() {
            Some(b) => Poll::Ready(Some(Ok(b))),
            None => Poll::Ready(None),
        }
    }
    fn poll_trailers(
        self: Pin<&mut Self>,
        _: &mut Context,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        Poll::Ready(Ok(None))
    }
}

pub struct RecvBody {
    conn: ConnectionRef,
    stream_id: StreamId,
    recv: FrameStream,
    trailers: Option<HeadersFrame>,
}

impl RecvBody {
    pub(crate) fn new(conn: ConnectionRef, stream_id: StreamId, recv: FrameStream) -> Self {
        Self {
            conn,
            stream_id,
            recv,
            trailers: None,
        }
    }

    pub async fn read_to_end(&mut self) -> Result<Bytes, Error> {
        let mut body = BytesMut::with_capacity(10_240);

        let mut me = self;
        let res: Result<(), Error> = future::poll_fn(|cx| {
            while let Some(d) = ready!(Pin::new(&mut me).poll_data(cx)) {
                body.extend(d?.bytes());
            }
            Poll::Ready(Ok(()))
        })
        .await;
        res?;

        Ok(body.freeze())
    }

    pub async fn trailers(&mut self) -> Result<Option<HeaderMap>, Error> {
        let mut me = self;
        Ok(future::poll_fn(|cx| Pin::new(&mut me).poll_trailers(cx)).await?)
    }
}

impl HttpBody for RecvBody {
    type Data = bytes::Bytes;
    type Error = Error;

    fn poll_data(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        loop {
            return match ready!(Pin::new(&mut self.recv).poll_next(cx)) {
                None => Poll::Ready(None),
                Some(Ok(HttpFrame::Reserved)) => continue,
                Some(Ok(HttpFrame::Data(d))) => Poll::Ready(Some(Ok(d.payload))),
                Some(Ok(HttpFrame::Headers(t))) => {
                    self.trailers = Some(t);
                    Poll::Ready(None)
                }
                Some(Err(e)) => {
                    self.recv.reset(e.code());
                    Poll::Ready(Some(Err(e.into())))
                }
                Some(Ok(f)) => {
                    self.recv.reset(ErrorCode::FRAME_UNEXPECTED);
                    Poll::Ready(Some(Err(Error::Peer(format!(
                        "Invalid frame type in body: {:?}",
                        f
                    )))))
                }
            };
        }
    }

    fn poll_trailers(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        if self.trailers.is_none() {
            return Poll::Ready(Ok(None));
        }

        let header = {
            let mut conn = self.conn.h3.lock().unwrap();
            ready!(conn.poll_decode(cx, self.stream_id, self.trailers.as_ref().unwrap()))?
        };
        self.trailers = None;

        Poll::Ready(Ok(Some(header.into_fields())))
    }
}

impl fmt::Debug for RecvBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecvBody")
            .field("stream", &self.stream_id)
            .finish()
    }
}
