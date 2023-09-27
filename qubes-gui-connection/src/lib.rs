/*
 * The Qubes OS Project, https://www.qubes-os.org
 *
 * Copyright (C) 2021  Demi Marie Obenour  <demi@invisiblethingslab.com>
 *
 * This program is free software; you can redistribute it and/or
 * modify it under the terms of the GNU General Public License
 * as published by the Free Software Foundation; either version 2
 * of the License, or (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program; if not, write to the Free Software
 * Foundation, Inc., 51 Franklin Street, Fifth Floor, Boston, MA  02110-1301, USA.
 *
 */
//! A client for the Qubes OS GUI protocol.  This client is low-level.

#![forbid(missing_docs)]
#![forbid(unconditional_recursion)]
#![forbid(clippy::all)]

pub use qubes_gui;
use std::convert::TryInto;
use std::task::Poll;

use qubes_castable::{static_assert, Castable};
use qubes_gui::{Header, UntrustedHeader};
use std::collections::VecDeque;
use std::io::{self, Error, ErrorKind};
use std::mem::size_of;
use vchan::{Status, Vchan};

#[cfg(test)]
mod tests;

/// Protocol state
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
enum ReadState {
    /// Currently connecting
    Connecting,
    /// Negotiating protocol version
    Negotiating,
    /// Reading a message header
    ReadingHeader,
    /// Reading a message body
    ReadingBody { header: Header },
    /// Discarding data from an unknown message
    Discard(usize),
    /// Something went wrong.  Terminal state.
    Error,
}

// Trait for a vchan, for unit-testing
trait VchanMock
where
    Self: Sized,
{
    fn buffer_space(&self) -> usize;
    fn recv_into(&self, buf: &mut Vec<u8>, bytes: usize) -> Result<(), vchan::Error>;
    fn recv_struct<T: Castable + Default>(&self) -> Result<T, vchan::Error>;
    fn send(&self, buf: &[u8]) -> Result<(), vchan::Error>;
    fn wait(&self);
    fn data_ready(&self) -> usize;
    fn status(&self) -> Status;
    fn discard(&self, bytes: usize) -> Result<(), vchan::Error>;
}

impl VchanMock for Option<Vchan> {
    fn discard(&self, bytes: usize) -> Result<(), vchan::Error> {
        Vchan::discard(self.as_ref().unwrap(), bytes)
    }
    fn buffer_space(&self) -> usize {
        Vchan::buffer_space(self.as_ref().unwrap())
    }
    fn recv_into(&self, buf: &mut Vec<u8>, bytes: usize) -> Result<(), vchan::Error> {
        Vchan::recv_into(self.as_ref().unwrap(), buf, bytes)
    }
    fn recv_struct<T: Castable>(&self) -> Result<T, vchan::Error> {
        Vchan::recv_struct(self.as_ref().unwrap())
    }
    fn send(&self, buf: &[u8]) -> Result<(), vchan::Error> {
        Vchan::send(self.as_ref().unwrap(), buf)
    }
    fn wait(&self) {
        Vchan::wait(self.as_ref().unwrap())
    }
    fn data_ready(&self) -> usize {
        Vchan::data_ready(self.as_ref().unwrap())
    }
    fn status(&self) -> Status {
        self.as_ref()
            .map(Vchan::status)
            .unwrap_or(Status::Disconnected)
    }
}

/// The kind of a state machine
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    /// An agent instance
    Agent,
    /// A daemon instance
    Daemon,
}

#[derive(Debug)]
struct RawMessageStream<T: VchanMock> {
    /// Vchan
    vchan: T,
    /// Write buffer
    queue: VecDeque<u8>,
    /// State of the read state machine
    state: ReadState,
    /// Read buffer
    buffer: Vec<u8>,
    /// Was reconnect successful?
    did_reconnect: bool,
    /// Configuration from the daemon
    xconf: qubes_gui::XConfVersion,
    /// Peer domain ID
    domid: u16,
    /// Agent or daemon?
    kind: Kind,
}

/// A buffer
#[derive(Debug)]
pub struct Buffer<'a> {
    inner: &'a mut Vec<u8>,
    hdr: Header,
}

impl<'a> Buffer<'a> {
    /// Gets the header
    pub fn hdr(&self) -> Header {
        self.hdr
    }
    /// Gets a reference to the body
    pub fn body(&self) -> &[u8] {
        &self.inner[..]
    }
    /// Takes ownership of the body
    pub fn take(mut self) -> Vec<u8> {
        std::mem::replace(&mut self.inner, vec![])
    }
}

impl<T: VchanMock + 'static> RawMessageStream<T> {
    /// Attempts to write as much of `slice` as possible to the `vchan`.  Never
    /// blocks.  Returns the number of bytes written.
    ///
    /// # Errors
    ///
    /// Fails if writing to the vchan fails.
    fn write_slice(vchan: &mut T, slice: &[u8]) -> Result<usize, vchan::Error> {
        let space = vchan.buffer_space();
        if space == 0 {
            Ok(0)
        } else {
            let to_write = space.min(slice.len());
            vchan.send(&slice[..to_write])?;
            Ok(to_write)
        }
    }

    /// Write as much of the buffered data as possible without blocking.
    /// Returns the number of bytes successfully written.
    fn flush_pending_writes(&mut self) -> Result<usize, vchan::Error> {
        let mut written = 0;
        loop {
            let (front, back) = self.queue.as_slices();
            let to_write = if front.is_empty() {
                if back.is_empty() {
                    break Ok(written);
                }
                back
            } else {
                front
            };
            let written_this_time = Self::write_slice(&mut self.vchan, to_write)?;
            if written_this_time == 0 {
                break Ok(written);
            }
            written += written_this_time;
            for _ in 0..written_this_time {
                let _ = self.queue.pop_front();
            }
        }
    }

    /// Write as much of the buffered data to the vchan as possible.  Queue the
    /// rest in an internal buffer.
    ///
    /// # Errors
    ///
    /// Fails if there is an I/O error on the vchan.
    pub fn write(&mut self, buf: &[u8]) -> Result<(), vchan::Error> {
        #[cfg(not(test))]
        match self.state {
            ReadState::Error | ReadState::Connecting | ReadState::Negotiating => return Ok(()),
            _ => {}
        }
        self.flush_pending_writes()?;
        if !self.queue.is_empty() {
            self.queue.extend(buf);
            return Ok(());
        }
        let written = Self::write_slice(&mut self.vchan, buf)?;
        if written != buf.len() {
            assert!(written < buf.len());
            self.queue.extend(&buf[written..]);
        }
        Ok(())
    }

    /// Acknowledge an event on the vchan.
    pub fn wait(&mut self) {
        self.vchan.wait()
    }

    /// Check for a reconnection, consuming the pending reconnection state.
    pub fn reconnected(&mut self) -> bool {
        std::mem::replace(&mut self.did_reconnect, false)
    }

    fn read_message_internal(&mut self) -> io::Result<Option<Header>> {
        const SIZE_OF_XCONF: usize = size_of::<qubes_gui::XConfVersion>();
        self.flush_pending_writes()?;
        static_assert!(
            size_of::<u32>() <= size_of::<usize>(),
            "<32-bit systems not supported"
        );
        loop {
            let ready = self.vchan.data_ready();
            match &mut self.state {
                ReadState::Connecting => match self.vchan.status() {
                    Status::Waiting => return Ok(None),
                    Status::Connected => match self.kind {
                        Kind::Daemon => self.state = ReadState::Negotiating,
                        Kind::Agent => {
                            assert!(self.vchan.buffer_space() >= 4, "vchans have larger buffers");
                            match self.vchan.send(qubes_gui::PROTOCOL_VERSION.as_bytes()) {
                                Ok(()) => self.state = ReadState::Negotiating,
                                Err(e) => break Err(e.into()),
                            }
                        }
                    },
                    Status::Disconnected => {
                        break Err(Error::new(ErrorKind::Other, "vchan connection refused"));
                    }
                },
                ReadState::Error => {
                    break Err(Error::new(ErrorKind::Other, "Already in error state"))
                }
                ReadState::Negotiating => match self.kind {
                    Kind::Agent if ready >= SIZE_OF_XCONF => {
                        let new_xconf: qubes_gui::XConfVersion = self.vchan.recv_struct()?;
                        let (daemon_major, daemon_minor) =
                            (new_xconf.version >> 16, new_xconf.version & 0xFFFF);
                        if qubes_gui::PROTOCOL_VERSION_MAJOR == daemon_major
                            && qubes_gui::PROTOCOL_VERSION_MINOR >= daemon_minor
                            && daemon_minor >= 4
                        {
                            self.xconf = new_xconf;
                            self.state = ReadState::ReadingHeader;
                            self.did_reconnect = true;
                        } else {
                            break Err(Error::new(ErrorKind::InvalidData,
                                            format!(
                                                "Version negotiation failed: their version is {}.{} but ours is {}.{}",
                                                daemon_major, daemon_minor,
                                                qubes_gui::PROTOCOL_VERSION_MAJOR,
                                                qubes_gui::PROTOCOL_VERSION_MINOR,
                                                )));
                        }
                    }
                    Kind::Daemon if ready >= 4 => {
                        let version: u32 = self.vchan.recv_struct()?;
                        let (major, minor) = (version >> 16, version & 0xFFFF);
                        if major == qubes_gui::PROTOCOL_VERSION_MAJOR {
                            let version = version.min(qubes_gui::PROTOCOL_VERSION_MINOR);
                            self.xconf.version = version;
                            self.vchan.send(if version >= 4 {
                                self.xconf.as_bytes()
                            } else {
                                self.xconf.xconf.as_bytes()
                            })?;
                            self.state = ReadState::ReadingHeader
                        } else {
                            break Err(Error::new(
                                    ErrorKind::InvalidData,
                                    format!(
                                        "Unsupported version from agent: daemon supports {}.{} but agent sent {}.{}",
                                        qubes_gui::PROTOCOL_VERSION_MAJOR,
                                        qubes_gui::PROTOCOL_VERSION_MINOR,
                                        major,
                                        minor,
                                    )));
                        }
                    }
                    Kind::Agent | Kind::Daemon => break Ok(None),
                },
                ReadState::ReadingHeader if ready < size_of::<Header>() => break Ok(None),
                ReadState::ReadingHeader => {
                    // Reset buffer to 0 bytes
                    self.buffer.clear();
                    let header: UntrustedHeader = self.vchan.recv_struct()?;
                    match header.validate_length() {
                        Err(e) => {
                            break Err(Error::new(ErrorKind::InvalidData, format!("{}", e)));
                        }
                        Ok(Some(header)) if header.len() == 0 => {
                            self.state = ReadState::ReadingHeader;
                            break Ok(Some(header));
                        }
                        Ok(Some(header)) => self.state = ReadState::ReadingBody { header },
                        Ok(None) if header.untrusted_len == 0 => {
                            self.state = ReadState::ReadingHeader
                        }
                        Ok(None) => self.state = ReadState::Discard(header.untrusted_len as _),
                    }
                }
                ReadState::Discard(untrusted_len) => {
                    match self.vchan.discard(ready.min(*untrusted_len)) {
                        Err(e) => break Err(e.into()),
                        Ok(()) if ready >= *untrusted_len => self.state = ReadState::ReadingHeader,
                        Ok(()) => *untrusted_len -= ready,
                    }
                }
                &mut ReadState::ReadingBody { header } => {
                    let to_read = header.len() - self.buffer.len();
                    self.vchan.recv_into(&mut self.buffer, to_read.min(ready))?;
                    break if ready >= to_read {
                        self.state = ReadState::ReadingHeader;
                        Ok(Some(header))
                    } else {
                        Ok(None)
                    };
                }
            }
        }
    }

    /// If a complete message has been buffered, returns `Ok(Some(msg))`.  If
    /// more data needs to arrive, returns `Ok(None)`.  If an error occurs,
    /// `Err` is returned, and the stream is placed in an error state.  If the
    /// stream is in an error state, all further functions will fail.
    pub fn read_message<'a>(&'a mut self) -> io::Result<Option<Buffer<'a>>> {
        match self.read_message_internal() {
            Ok(Some(header)) => Ok(Some(Buffer {
                hdr: header,
                inner: &mut self.buffer,
            })),
            Ok(None) => Ok(None),
            Err(e) => {
                self.state = ReadState::Error;
                Err(e)
            }
        }
    }

    pub fn needs_reconnect(&self) -> bool {
        self.vchan.status() == Status::Disconnected
    }
}

impl RawMessageStream<Option<Vchan>> {
    pub fn agent(domain: u16) -> io::Result<Self> {
        let vchan = Vchan::server(domain, qubes_gui::LISTENING_PORT.into(), 4096, 4096)?;
        Ok(Self {
            vchan: Some(vchan),
            queue: Default::default(),
            state: ReadState::Connecting,
            buffer: vec![],
            did_reconnect: false,
            domid: domain,
            kind: Kind::Agent,
            xconf: Default::default(),
        })
    }

    pub fn daemon(domain: u16, xconf: qubes_gui::XConf) -> io::Result<Self> {
        Ok(Self {
            vchan: Some(Vchan::client(domain, qubes_gui::LISTENING_PORT.into())?),
            queue: Default::default(),
            state: ReadState::ReadingHeader,
            buffer: vec![],
            did_reconnect: false,
            domid: domain,
            kind: Kind::Daemon,
            xconf: qubes_gui::XConfVersion {
                version: qubes_gui::PROTOCOL_VERSION,
                xconf,
            },
        })
    }

    pub fn reconnect(&mut self) -> Result<(), vchan::Error> {
        self.vchan = None;
        self.vchan = Some(Vchan::server(
            self.domid,
            qubes_gui::LISTENING_PORT.into(),
            4096,
            4096,
        )?);
        self.queue.clear();
        self.buffer.clear();
        self.state = ReadState::Connecting;
        Ok(())
    }

    pub fn as_raw_fd(&self) -> std::os::raw::c_int {
        self.vchan.as_ref().unwrap().fd()
    }
}
/// The entry-point to the library.
#[derive(Debug)]
pub struct Connection {
    raw: RawMessageStream<Option<vchan::Vchan>>,
}

impl Connection {
    /// Send a GUI message.  This never blocks; outgoing messages are queued
    /// until there is space in the vchan.
    pub fn send<T: qubes_gui::Message>(
        &mut self,
        message: &T,
        window: qubes_gui::WindowID,
    ) -> io::Result<()> {
        self.send_raw(message.as_bytes(), window, T::KIND as _)
    }

    /// Raw version of [`Connection::send`].  Using [`Connection::send`] is preferred
    /// where possible, as it automatically selects the correct message type.
    pub fn send_raw(
        &mut self,
        message: &[u8],
        window: qubes_gui::WindowID,
        ty: u32,
    ) -> io::Result<()> {
        let untrusted_len = message
            .len()
            .try_into()
            .expect("Message length must fit in a u32");
        let header = qubes_gui::UntrustedHeader {
            ty,
            window,
            untrusted_len,
        };
        header
            .validate_length()
            .unwrap()
            .expect("Sending unknown message!");
        // FIXME this is slow
        self.raw.write(header.as_bytes())?;
        self.raw.write(message)?;
        Ok(())
    }

    /// Even rawer version of [`Connection::send`].  Using [`Connection::send`] is
    /// preferred where possible, as it automatically selects the correct
    /// message type.  Otherwise, prefer [`Connection::send_raw`], which at least
    /// ensures correct framing.
    pub fn send_raw_bytes(&mut self, msg: &[u8]) -> io::Result<()> {
        self.raw.write(msg).map_err(From::from)
    }

    /// Acknowledge an event (as reported by poll(2), epoll(2), or similar).
    /// Must be called before performing any I/O.
    pub fn wait(&mut self) {
        self.raw.wait()
    }

    /// If a complete message has been buffered, returns `Ok(Some(msg))`.  If
    /// more data needs to arrive, returns `Ok(None)`.  If an error occurs,
    /// `Err` is returned, and the stream is placed in an error state.  If the
    /// stream is in an error state, all further functions will fail.
    pub fn read_message(&mut self) -> Poll<io::Result<Buffer<'_>>> {
        match self.raw.read_message() {
            Ok(None) => Poll::Pending,
            Ok(Some(v)) => Poll::Ready(Ok(v)),
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    /// Creates a daemon instance
    pub fn daemon(domain: u16, xconf: qubes_gui::XConf) -> io::Result<Self> {
        Ok(Self {
            raw: RawMessageStream::daemon(domain, xconf)?,
        })
    }

    /// Creates an agent instance
    pub fn agent(domain: u16) -> io::Result<Self> {
        Ok(Self {
            raw: RawMessageStream::agent(domain)?,
        })
    }

    /// Try to reconnect.  If this fails, the agent is no longer usable; future
    /// operations may panic.
    pub fn reconnect(&mut self) -> io::Result<()> {
        self.raw.reconnect().map_err(From::from)
    }

    /// Gets and clears the “did_reconnect” flag
    pub fn reconnected(&mut self) -> bool {
        self.raw.reconnected()
    }

    /// Returns true if a reconnection is needed.
    pub fn needs_reconnect(&self) -> bool {
        self.raw.needs_reconnect()
    }

    /// Get version information
    pub fn xconf(&self) -> qubes_gui::XConfVersion {
        self.raw.xconf
    }
}

impl std::os::unix::io::AsRawFd for Connection {
    fn as_raw_fd(&self) -> std::os::raw::c_int {
        self.raw.as_raw_fd()
    }
}
