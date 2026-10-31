//! A write-stall timeout for accepted client connections.
//!
//! When a client stops reading without closing the connection, its socket
//! buffer fills, writes return `Pending`, and hyper stops polling the
//! response body. Nothing then drops the body or the upstream slot it holds
//! (see `MAX_INFLIGHT_UPSTREAM`). This wrapper fails a write or flush that
//! has made no progress for the timeout, which makes hyper tear down the
//! connection and drop the body. The timer only runs while a write is
//! blocked and restarts on every completed write, so slow clients that keep
//! reading are never cut off, however long the download takes in total.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Instant, Sleep};

pub struct WriteTimeout<S> {
    inner: S,
    timeout: Option<Duration>,
    deadline: Pin<Box<Sleep>>,
    armed: bool,
}

impl<S> WriteTimeout<S> {
    /// `None` disables the timeout.
    pub fn new(inner: S, timeout: Option<Duration>) -> Self {
        Self {
            inner,
            timeout,
            deadline: Box::pin(tokio::time::sleep(Duration::ZERO)),
            armed: false,
        }
    }

    /// Passes a ready result through (disarming the timer), or, while the
    /// write stays blocked, errors once it has been blocked for the timeout.
    fn check<T>(&mut self, cx: &mut Context<'_>, poll: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        let Some(timeout) = self.timeout else {
            return poll;
        };
        if poll.is_ready() {
            self.armed = false;
            return poll;
        }
        if !self.armed {
            self.armed = true;
            self.deadline.as_mut().reset(Instant::now() + timeout);
        }
        match self.deadline.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "client stopped reading (CLIENT_WRITE_TIMEOUT_MS)",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for WriteTimeout<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for WriteTimeout<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        let poll = Pin::new(&mut this.inner).poll_write(cx, buf);
        this.check(cx, poll)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = &mut *self;
        let poll = Pin::new(&mut this.inner).poll_write_vectored(cx, bufs);
        this.check(cx, poll)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let poll = Pin::new(&mut this.inner).poll_flush(cx);
        this.check(cx, poll)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let poll = Pin::new(&mut this.inner).poll_shutdown(cx);
        this.check(cx, poll)
    }
}
