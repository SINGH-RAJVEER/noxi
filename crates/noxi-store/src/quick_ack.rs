//! A TCP connection for HTTP caches that acknowledges data at once.
//!
//! A server that writes a response's headers and body separately, without
//! `TCP_NODELAY`, holds the body until the headers are acknowledged, and
//! Linux delays that acknowledgement by up to 40 ms on a reused
//! connection. `TCP_QUICKACK` lapses after use, so it is set again before
//! every read. Connections through a proxy are left to ureq's connectors.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::time::Duration;

use ureq::unversioned::transport::{
	Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, Transport,
};
use ureq::{Error, Timeout};

/// The first connector of a chain: opens direct TCP connections.
#[derive(Debug, Default)]
pub struct QuickAckConnector;

impl Connector<()> for QuickAckConnector {
	type Out = QuickAckTransport;

	fn connect(
		&self,
		details: &ConnectionDetails,
		_chained: Option<()>,
	) -> Result<Option<Self::Out>, Error> {
		if details.config.proxy().is_some() {
			return Ok(None);
		}
		let mut last = None;
		for addr in details.addrs.iter() {
			let stream = match std_timeout(details.timeout) {
				Some(timeout) => TcpStream::connect_timeout(addr, timeout),
				None => TcpStream::connect(addr),
			};
			match stream {
				Ok(stream) => {
					if details.config.no_delay() {
						stream.set_nodelay(true)?;
					}
					return Ok(Some(QuickAckTransport {
						stream,
						buffers: LazyBuffers::new(
							details.config.input_buffer_size(),
							details.config.output_buffer_size(),
						),
						read_timeout: None,
						write_timeout: None,
					}));
				}
				Err(e) if e.kind() == io::ErrorKind::TimedOut => {
					last = Some(Error::Timeout(Timeout::Connect));
				}
				Err(e) => last = Some(e.into()),
			}
		}
		Err(last.unwrap_or_else(|| {
			io::Error::new(io::ErrorKind::ConnectionRefused, "Connection refused").into()
		}))
	}
}

pub struct QuickAckTransport {
	stream: TcpStream,
	buffers: LazyBuffers,
	read_timeout: Option<Duration>,
	write_timeout: Option<Duration>,
}

impl fmt::Debug for QuickAckTransport {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("QuickAckTransport")
			.field("addr", &self.stream.peer_addr().ok())
			.finish()
	}
}

/// The time left for the current step, if limited.
fn std_timeout(timeout: NextTimeout) -> Option<Duration> {
	timeout.not_zero().map(|t| *t)
}

/// Map a blocking-I/O timeout to ureq's error for the current step.
fn io_error(e: io::Error, timeout: NextTimeout) -> Error {
	match e.kind() {
		io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => Error::Timeout(timeout.reason),
		_ => e.into(),
	}
}

impl Transport for QuickAckTransport {
	fn buffers(&mut self) -> &mut dyn Buffers {
		&mut self.buffers
	}

	fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
		if std_timeout(timeout) != self.write_timeout {
			self.stream.set_write_timeout(std_timeout(timeout))?;
			self.write_timeout = std_timeout(timeout);
		}
		let output = &self.buffers.output()[..amount];
		self.stream
			.write_all(output)
			.map_err(|e| io_error(e, timeout))
	}

	fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
		if std_timeout(timeout) != self.read_timeout {
			self.stream.set_read_timeout(std_timeout(timeout))?;
			self.read_timeout = std_timeout(timeout);
		}
		let on: libc::c_int = 1;
		// SAFETY: setting an integer option on a socket we own. A failure
		// only leaves acknowledgements delayed.
		unsafe {
			libc::setsockopt(
				self.stream.as_raw_fd(),
				libc::IPPROTO_TCP,
				libc::TCP_QUICKACK,
				(&on as *const libc::c_int).cast(),
				size_of::<libc::c_int>() as libc::socklen_t,
			);
		}
		let input = self.buffers.input_append_buf();
		let amount = self.stream.read(input).map_err(|e| io_error(e, timeout))?;
		self.buffers.input_appended(amount);
		Ok(amount > 0)
	}

	fn is_open(&mut self) -> bool {
		// As ureq's TCP transport: open if a non-blocking read finds
		// nothing waiting.
		if self.stream.set_nonblocking(true).is_err() {
			return false;
		}
		let mut buf = [0];
		let open =
			matches!(self.stream.read(&mut buf), Err(e) if e.kind() == io::ErrorKind::WouldBlock);
		open && self.stream.set_nonblocking(false).is_ok()
	}
}

/// An agent with ureq's connectors after [`QuickAckConnector`], which
/// takes every direct connection.
pub fn agent(config: ureq::config::Config) -> ureq::Agent {
	use ureq::unversioned::resolver::DefaultResolver;
	use ureq::unversioned::transport::{ConnectProxyConnector, RustlsConnector, TcpConnector};
	let connector =
		().chain(QuickAckConnector)
			.chain(ConnectProxyConnector::default())
			.chain(TcpConnector::default())
			.chain(RustlsConnector::default());
	ureq::Agent::with_parts(config, connector, DefaultResolver::default())
}
