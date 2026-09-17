//! Adapts the core transport's byte channels to the `AsyncRead`/`AsyncWrite`
//! stream `russh-sftp` expects.
//!
//! Both halves are `async_channel` endpoints, so polling this stream never
//! touches a tokio reactor; the driver task that owns it runs on the
//! connection's runtime, which is where `russh-sftp`'s own tasks are spawned.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_channel::{Receiver, RecvError, SendError, Sender};
use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

type ReadFuture = Pin<Box<dyn Future<Output = Result<Bytes, RecvError>> + Send + 'static>>;
type WriteFuture = Pin<Box<dyn Future<Output = Result<(), SendError<Bytes>>> + Send + 'static>>;

/// A duplex byte stream over the channels [`sshdeck_core::sftp::SftpChannel`]
/// hands out.
pub(crate) struct RemoteStream {
    incoming: Receiver<Bytes>,
    outgoing: Sender<Bytes>,
    read: Vec<u8>,
    read_pos: usize,
    pending_read: Option<ReadFuture>,
    pending_write: Option<WriteFuture>,
    pending_write_len: usize,
}

impl RemoteStream {
    pub(crate) fn new(incoming: Receiver<Bytes>, outgoing: Sender<Bytes>) -> Self {
        Self {
            incoming,
            outgoing,
            read: Vec::new(),
            read_pos: 0,
            pending_read: None,
            pending_write: None,
            pending_write_len: 0,
        }
    }
}

impl AsyncRead for RemoteStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if self.read_pos < self.read.len() {
                let start = self.read_pos;
                let n = (self.read.len() - start).min(buf.remaining());
                buf.put_slice(&self.read[start..start + n]);
                self.read_pos += n;
                if self.read_pos == self.read.len() {
                    self.read.clear();
                    self.read_pos = 0;
                }
                return Poll::Ready(Ok(()));
            }

            if buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }

            if self.pending_read.is_none() {
                let incoming = self.incoming.clone();
                let future: ReadFuture = Box::pin(async move { incoming.recv().await });
                self.pending_read = Some(future);
            }

            let result = match self.pending_read.as_mut() {
                Some(future) => match future.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(result) => result,
                },
                None => return Poll::Ready(Ok(())),
            };
            self.pending_read = None;

            match result {
                Ok(bytes) => {
                    self.read = bytes.to_vec();
                    self.read_pos = 0;
                }
                // Closed and drained: end of stream.
                Err(_) => return Poll::Ready(Ok(())),
            }
        }
    }
}

impl AsyncWrite for RemoteStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        if self.pending_write.is_none() {
            let outgoing = self.outgoing.clone();
            let data = Bytes::copy_from_slice(buf);
            self.pending_write_len = buf.len();
            let future: WriteFuture = Box::pin(async move { outgoing.send(data).await });
            self.pending_write = Some(future);
        }

        let result = match self.pending_write.as_mut() {
            Some(future) => match future.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => result,
            },
            None => return Poll::Ready(Ok(0)),
        };
        self.pending_write = None;

        let len = self.pending_write_len;
        match result {
            Ok(()) => Poll::Ready(Ok(len)),
            Err(_) => Poll::Ready(Err(closed())),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        if self.pending_write.is_some() {
            let result = match self.pending_write.as_mut() {
                Some(future) => match future.as_mut().poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(result) => result,
                },
                None => Ok(()),
            };
            self.pending_write = None;
            if result.is_err() {
                return Poll::Ready(Err(closed()));
            }
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        match self.as_mut().poll_flush(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            Poll::Ready(Ok(())) => {}
        }
        self.outgoing.close();
        Poll::Ready(Ok(()))
    }
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "SFTP transport closed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::block_on;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn writes_pass_through_to_the_outgoing_channel() {
        let (incoming_tx, incoming_rx) = async_channel::bounded(4);
        let (outgoing_tx, outgoing_rx) = async_channel::bounded(4);
        let mut stream = RemoteStream::new(incoming_rx, outgoing_tx);

        block_on(stream.write_all(b"hello")).expect("write");
        assert_eq!(
            outgoing_rx.recv_blocking().expect("chunk"),
            Bytes::from_static(b"hello")
        );

        drop(incoming_tx);
    }

    #[test]
    fn reads_pass_through_and_end_at_eof() {
        let (incoming_tx, incoming_rx) = async_channel::bounded(4);
        let (outgoing_tx, _outgoing_rx) = async_channel::bounded(4);
        let mut stream = RemoteStream::new(incoming_rx, outgoing_tx);

        incoming_tx
            .send_blocking(Bytes::from_static(b"wor"))
            .expect("send");
        incoming_tx
            .send_blocking(Bytes::from_static(b"ld"))
            .expect("send");
        drop(incoming_tx);

        let mut buffer = Vec::new();
        block_on(stream.read_to_end(&mut buffer)).expect("read");
        assert_eq!(buffer, b"world".to_vec());
    }
}
