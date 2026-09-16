//! Transport contract for daemon sockets and streams.
//!
//! The daemon redesign defines its transport as a trait from day one
//! (MISSION.md, Windows-readiness): AF_UNIX sockets today, named pipes
//! (`\\.\pipe\...`) on Windows later. Callers bind/connect through these
//! traits and never name a concrete socket type, so a future platform swap
//! (tokio named-pipe listener) is an implementation change only.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use anyhow::Result;

/// A full-duplex stream between a client and a daemon endpoint.
///
/// `split` consumes the boxed stream into its owned halves; concrete socket
/// types implement this, and callers hold only the erased halves.
pub trait TransportStream: Send + Sync {
    fn split(self: Box<Self>) -> (Box<dyn AsyncReadHalf>, Box<dyn AsyncWriteHalf>);
}

/// Owned read half of a [`TransportStream`]; blanket-implemented.
pub trait AsyncReadHalf: tokio::io::AsyncRead + Unpin + Send {}
impl<T> AsyncReadHalf for T where T: tokio::io::AsyncRead + Unpin + Send {}

/// Owned write half of a [`TransportStream`]; blanket-implemented.
pub trait AsyncWriteHalf: tokio::io::AsyncWrite + Unpin + Send {}
impl<T> AsyncWriteHalf for T where T: tokio::io::AsyncWrite + Unpin + Send {}

#[cfg(unix)]
impl TransportStream for tokio::net::UnixStream {
    fn split(self: Box<Self>) -> (Box<dyn AsyncReadHalf>, Box<dyn AsyncWriteHalf>) {
        let (reader, writer) = tokio::net::UnixStream::into_split(*self);
        (Box::new(reader), Box::new(writer))
    }
}

/// Future returned by [`TransportListener::accept`].
pub type AcceptFuture<'a> =
    Pin<Box<dyn Future<Output = std::io::Result<Box<dyn TransportStream>>> + Send + 'a>>;

/// A bound transport endpoint that hands out connected streams.
pub trait TransportListener: Send + Sync {
    /// Future-boxed so the trait stays dyn-compatible (RPITIT methods are not);
    /// the future borrows the listener for the duration of the accept.
    fn accept(&self) -> AcceptFuture<'_>;
}

#[cfg(unix)]
impl TransportListener for tokio::net::UnixListener {
    fn accept(&self) -> AcceptFuture<'_> {
        Box::pin(async move {
            let (stream, _address) = self.accept().await?;
            Ok(Box::new(stream) as Box<dyn TransportStream>)
        })
    }
}

/// Bind a listening endpoint at `path` (a socket file on Unix).
#[cfg(unix)]
pub async fn bind_transport(path: &Path) -> Result<Box<dyn TransportListener>> {
    let listener = tokio::net::UnixListener::bind(path)?;
    Ok(Box::new(listener))
}

/// Connect to the endpoint at `path` asynchronously.
#[cfg(unix)]
pub async fn connect_transport(path: &Path) -> Result<Box<dyn TransportStream>> {
    let stream = tokio::net::UnixStream::connect(path).await?;
    Ok(Box::new(stream))
}

#[cfg(not(unix))]
pub async fn bind_transport(_path: &Path) -> Result<Box<dyn TransportListener>> {
    anyhow::bail!("Windows transport (named pipes) is not yet implemented")
}

#[cfg(not(unix))]
pub async fn connect_transport(_path: &Path) -> Result<Box<dyn TransportStream>> {
    anyhow::bail!("Windows transport (named pipes) is not yet implemented")
}

/// A blocking full-duplex stream, for the CLI's one-shot command client.
pub trait BlockingTransportStream:
    std::fmt::Debug + std::io::Read + std::io::Write + Send + Sync
{
    /// Duplicate the underlying handle so reads and writes can proceed on
    /// separate owned halves.
    fn try_clone_box(&self) -> std::io::Result<Box<dyn BlockingTransportStream>>;
    /// Deadline a pending read (poll granularity for deadline-driven waits).
    fn set_read_timeout(&self, timeout: std::time::Duration) -> std::io::Result<()>;
}

#[cfg(unix)]
impl BlockingTransportStream for std::os::unix::net::UnixStream {
    fn try_clone_box(&self) -> std::io::Result<Box<dyn BlockingTransportStream>> {
        Ok(Box::new(self.try_clone()?))
    }

    fn set_read_timeout(&self, timeout: std::time::Duration) -> std::io::Result<()> {
        std::os::unix::net::UnixStream::set_read_timeout(self, Some(timeout))
    }
}

/// Connect to the endpoint at `path`, blocking until connected.
#[cfg(unix)]
pub fn connect_blocking(path: &Path) -> std::io::Result<Box<dyn BlockingTransportStream>> {
    let stream = std::os::unix::net::UnixStream::connect(path)?;
    Ok(Box::new(stream))
}

#[cfg(not(unix))]
pub fn connect_blocking(_path: &Path) -> std::io::Result<Box<dyn BlockingTransportStream>> {
    Err(std::io::Error::other(
        "Windows transport (named pipes) is not yet implemented",
    ))
}
