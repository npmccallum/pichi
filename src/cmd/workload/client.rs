// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Bounded client framing using pichi's existing async runtime.

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use interprocess::local_socket::{
    GenericFilePath,
    tokio::{Stream, prelude::*},
};
use madrillo::{ClientMessage, PROTOCOL_VERSION, Request, Response, ServerMessage};
use std::{path::Path, pin::Pin, task, time::Duration};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, ReadHalf, WriteHalf};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

const MAX_FRAME: usize = 1024 * 1024;

/// Transport failure, distinct from malformed responses and host console I/O.
#[derive(Debug)]
pub(super) struct ConnectionError(std::io::Error);

impl std::fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for ConnectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// The monitor's stream, which on Windows must not flush when dropped.
#[derive(Debug)]
struct Wire(Stream);

impl AsyncRead for Wire {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for Wire {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        bytes: &[u8],
    ) -> task::Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, bytes)
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

#[cfg(windows)]
impl Drop for Wire {
    fn drop(&mut self) {
        let Stream::NamedPipe(pipe) = &self.0;
        pipe.inner().assume_flushed();
    }
}

/// Requests to the monitor. Dropping it closes the write side's half of the
/// connection only once the receiving half is gone too.
#[derive(Debug)]
pub(super) struct Sender(FramedWrite<WriteHalf<Wire>, LengthDelimitedCodec>);

/// The monitor's responses.
#[derive(Debug)]
pub(super) struct Receiver(FramedRead<ReadHalf<Wire>, LengthDelimitedCodec>);

#[derive(Debug)]
pub(super) struct Client {
    sender: Sender,
    receiver: Receiver,
}

impl Client {
    pub(super) async fn connect(path: &Path) -> Result<Self> {
        let stream = tokio::time::timeout(
            Duration::from_secs(5),
            Stream::connect(path.to_fs_name::<GenericFilePath>()?),
        )
        .await
        .context("monitor connect deadline")??;
        #[cfg(unix)]
        if stream.peer_creds()?.euid() != Some(nix::unistd::geteuid().as_raw()) {
            bail!("monitor endpoint belongs to another account");
        }
        #[cfg(windows)]
        {
            use win_custody::{ObjectSecurityDescriptor, OwnedSid};
            let Stream::NamedPipe(pipe) = &stream;
            let descriptor = ObjectSecurityDescriptor::query_kernel_object(pipe.inner())?;
            let owner = descriptor.owner()?;
            let user = OwnedSid::from_string(&win_custody::current_user_sid_string()?)?;
            let system = OwnedSid::from_string("S-1-5-18")?;
            let administrators = OwnedSid::from_string("S-1-5-32-544")?;
            if ![user, system, administrators]
                .iter()
                .any(|sid| owner.matches(sid))
            {
                bail!("monitor endpoint belongs to another account");
            }
        }
        Ok(Self::new(stream))
    }

    pub(super) fn new(stream: Stream) -> Self {
        let (read, write) = tokio::io::split(Wire(stream));
        let codec = || {
            LengthDelimitedCodec::builder()
                .max_frame_length(MAX_FRAME)
                .new_codec()
        };
        Self {
            sender: Sender(FramedWrite::new(write, codec())),
            receiver: Receiver(FramedRead::new(read, codec())),
        }
    }

    /// The two directions, to be used at the same time.
    pub(super) fn halves(&mut self) -> (&mut Sender, &mut Receiver) {
        (&mut self.sender, &mut self.receiver)
    }

    /// Send a request that the monitor answers at once; it is given up on
    /// after a deadline.
    pub(super) async fn send(&mut self, message: ClientMessage) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), self.sender.send(message))
            .await
            .context("monitor control write deadline")?
    }

    // Framed retains partially received bytes when a select branch is cancelled.
    pub(super) async fn next(&mut self) -> Result<ServerMessage> {
        self.receiver.next().await
    }
}

impl Sender {
    /// Send without a deadline: the monitor may take as long as the guest
    /// does to read what an attachment sends.
    pub(super) async fn send(&mut self, message: ClientMessage) -> Result<()> {
        let frame = serde_json::to_vec(&Request {
            version: PROTOCOL_VERSION,
            message,
        })?;
        if frame.len() > MAX_FRAME {
            bail!("monitor request exceeds frame limit");
        }
        self.0
            .send(bytes::Bytes::from(frame))
            .await
            .map_err(ConnectionError)?;
        Ok(())
    }
}

impl Receiver {
    pub(super) async fn next(&mut self) -> Result<ServerMessage> {
        let frame = self
            .0
            .next()
            .await
            .unwrap_or_else(|| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "monitor disconnected without completion",
                ))
            })
            .map_err(|error| -> anyhow::Error {
                if error.kind() == std::io::ErrorKind::InvalidData {
                    error.into()
                } else {
                    ConnectionError(error).into()
                }
            })?;
        let response: Response = serde_json::from_slice(&frame)?;
        if response.version != PROTOCOL_VERSION {
            bail!("unsupported monitor protocol {}", response.version);
        }
        Ok(response.message)
    }
}
