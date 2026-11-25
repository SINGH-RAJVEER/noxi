//! Plain downloads, CppNix's `FileTransfer::download` with
//! `decompress = false`.

use std::io::{self, Read};
use std::sync::Arc;
use std::time::{Duration, Instant};

use data_encoding::BASE64;
use ureq::tls::{Certificate, PemItem, RootCerts, TlsConfig};

use crate::binary_cache::{TransferSettings, curl_message};
use crate::error::{Error, Result};
use crate::netrc::{Credentials, Netrc};
use crate::quick_ack::STALLED;

/// curl's `MAX_REDIRS`.
const MAX_REDIRECTS: usize = 30;

/// How a file is downloaded.
#[derive(Clone, Default)]
pub struct Options {
	pub transfer: TransferSettings,
	pub netrc: Netrc,
	/// The certificates HTTPS servers are verified against, in PEM form.
	pub ca_certificates: Vec<u8>,
	/// `stalled-download-timeout` in seconds; zero means no limit.
	pub stalled_timeout: u64,
}

impl Options {
	/// What curl reports when nothing arrives for that long.
	fn stalled(&self) -> String {
		format!(
			"Operation too slow. Less than 1 bytes/sec transferred the last {} seconds",
			self.stalled_timeout
		)
	}
}

/// A response's body, with the reference's message when it breaks off.
struct Body {
	inner: Box<dyn Read + Send>,
	url: String,
	status: u16,
}

impl Read for Body {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		self.inner.read(buf).map_err(|e| {
			let reason = match e.kind() {
				io::ErrorKind::TimedOut => "Timeout was reached".to_owned(),
				io::ErrorKind::UnexpectedEof => "Transferred a partial file".to_owned(),
				_ => e.to_string(),
			};
			io::Error::other(format!(
				"unable to download '{}': HTTP error {} (curl error: {reason})",
				self.url, self.status
			))
		})
	}
}

/// The parts of an HTTP URL that decide about credentials.
struct Origin<'a> {
	scheme: &'a str,
	userinfo: Option<&'a str>,
	authority: &'a str,
	rest: &'a str,
}

impl<'a> Origin<'a> {
	fn parse(url: &'a str) -> Option<Self> {
		let (scheme, rest) = url.split_once("://")?;
		if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
			return None;
		}
		let rest = rest.split('#').next().unwrap_or(rest);
		let end = rest.find(['/', '?']).unwrap_or(rest.len());
		let (authority, rest) = rest.split_at(end);
		let (userinfo, authority) = match authority.rsplit_once('@') {
			Some((userinfo, authority)) => (Some(userinfo), authority),
			None => (None, authority),
		};
		Some(Self {
			scheme,
			userinfo,
			authority,
			rest,
		})
	}

	fn host(&self) -> &str {
		let host = match self.authority.rsplit_once(':') {
			Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
			_ => self.authority,
		};
		host.trim_start_matches('[').trim_end_matches(']')
	}

	/// The URL without its credentials.
	fn url(&self) -> String {
		let slash = if self.rest.starts_with('/') { "" } else { "/" };
		format!("{}://{}{slash}{}", self.scheme, self.authority, self.rest)
	}

	fn same_server(&self, other: &Origin<'_>) -> bool {
		self.scheme.eq_ignore_ascii_case(other.scheme)
			&& self.authority.eq_ignore_ascii_case(other.authority)
	}

	fn credentials(&self, netrc: &Netrc) -> Option<Credentials> {
		let decode =
			|s: &str| nix_compat::nixcpp::url::percent_decode(s).unwrap_or_else(|_| s.to_owned());
		match self.userinfo {
			Some(userinfo) => match userinfo.split_once(':') {
				Some((login, password)) => Some(Credentials {
					login: decode(login),
					password: decode(password),
				}),
				None => {
					let login = decode(userinfo);
					Some(
						netrc
							.find(self.host(), Some(&login))
							.unwrap_or(Credentials {
								login,
								password: String::new(),
							}),
					)
				}
			},
			None => netrc.find(self.host(), None),
		}
	}
}

/// The target of a redirect from `base`.
fn resolve(base: &Origin<'_>, location: &str) -> String {
	let has_scheme = location.find(':').is_some_and(|colon| {
		location[..colon]
			.bytes()
			.all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
			&& !location[..colon].is_empty()
	});
	if has_scheme {
		return location.to_owned();
	}
	let path = base.rest.split('?').next().unwrap_or("");
	if location.starts_with("//") {
		format!("{}:{location}", base.scheme)
	} else if location.starts_with('/') {
		format!("{}://{}{location}", base.scheme, base.authority)
	} else if location.starts_with('?') || location.is_empty() {
		format!("{}://{}{path}{location}", base.scheme, base.authority)
	} else {
		let directory = &path[..path.rfind('/').map_or(0, |slash| slash + 1)];
		let directory = if directory.is_empty() { "/" } else { directory };
		format!("{}://{}{directory}{location}", base.scheme, base.authority)
	}
}

/// Why one attempt failed.
struct Failure {
	transient: bool,
	message: String,
	status: u16,
	retry_after_ms: Option<u32>,
}

impl Failure {
	fn curl(url: &str, transient: bool, message: &str) -> Self {
		Self {
			transient,
			message: format!("unable to download '{url}': {message}"),
			status: 0,
			retry_after_ms: None,
		}
	}
}

fn attempt(
	agent: &ureq::Agent,
	url: &str,
	options: &Options,
) -> std::result::Result<Box<dyn Read + Send>, Failure> {
	let unsupported = || Failure::curl(url, false, "Unsupported protocol (1)");
	let mut current = url.to_owned();
	let mut status = 0;
	for _ in 0..=MAX_REDIRECTS {
		let origin = Origin::parse(&current).ok_or_else(unsupported)?;
		if origin.scheme.eq_ignore_ascii_case("https") && options.ca_certificates.is_empty() {
			return Err(Failure::curl(
				url,
				false,
				"Problem with the SSL CA cert (path? access rights?) (77)",
			));
		}
		let mut request = agent.get(origin.url());
		if let Some(credentials) = origin.credentials(&options.netrc) {
			let token =
				BASE64.encode(format!("{}:{}", credentials.login, credentials.password).as_bytes());
			request = request.header("Authorization", format!("Basic {token}"));
		}
		let started = Instant::now();
		let response = request.call().map_err(|e| {
			if matches!(&e, ureq::Error::Io(e) if e.to_string() == STALLED) {
				let message = format!("Timeout was reached (28) {}", options.stalled());
				return Failure::curl(url, true, &message);
			}
			Failure::curl(
				url,
				!matches!(e, ureq::Error::BadUri(_)),
				&curl_message(&e, &current, started.elapsed()),
			)
		})?;
		status = response.status().as_u16();
		let header = |name: &str| {
			response
				.headers()
				.get(name)
				.and_then(|value| value.to_str().ok())
		};
		match status {
			200..=299 => {
				return Ok(Box::new(Body {
					inner: Box::new(response.into_body().into_reader()),
					url: url.to_owned(),
					status,
				}));
			}
			301 | 302 | 303 | 307 | 308 if header("location").is_some() => {
				let mut next = resolve(&origin, header("location").unwrap_or_default());
				// Credentials of the URL stay with its server.
				if let Some(userinfo) = origin.userinfo
					&& let Some(target) = Origin::parse(&next)
					&& target.userinfo.is_none()
					&& target.same_server(&origin)
				{
					next = format!(
						"{}://{userinfo}@{}{}",
						target.scheme, target.authority, target.rest
					);
				}
				current = next;
				continue;
			}
			_ => {}
		}
		let retry_after_ms = header("retry-after")
			.and_then(|value| value.trim().parse::<u32>().ok())
			.map(|seconds| seconds.saturating_mul(1000));
		let mut body = Vec::new();
		let _ = response
			.into_body()
			.into_reader()
			.take(1 << 20)
			.read_to_end(&mut body);
		let body = String::from_utf8_lossy(&body).into_owned();
		let mut message = format!("unable to download '{url}': HTTP error {status}");
		// `FileTransferError`'s heuristic for showing the body.
		if !body.is_empty() && (body.len() < 1024 || body.contains("<html>")) {
			message += &format!("\n\nresponse body:\n\n{}", body.trim_end());
		}
		return Err(Failure {
			transient: !((400..500).contains(&status) && !matches!(status, 408 | 429)
				|| matches!(status, 501 | 505 | 511)),
			message,
			status,
			retry_after_ms,
		});
	}
	Err(Failure::curl(
		url,
		false,
		&format!("HTTP error {status} (curl error: Number of redirects hit maximum amount)"),
	))
}

/// Download `url` without decoding its content, retrying transient failures
/// and passing the warnings about them to `warn`.
pub fn download(
	url: &str,
	options: &Options,
	warn: &mut dyn FnMut(&str),
) -> Result<Box<dyn Read + Send>> {
	if !url.contains(':') {
		return Err(Error::Other(format!("URL '{url}' doesn't have a scheme")));
	}
	let transfer = &options.transfer;
	let certificates: Vec<Certificate<'static>> = ureq::tls::parse_pem(&options.ca_certificates)
		.filter_map(|item| match item {
			Ok(PemItem::Certificate(certificate)) => Some(certificate),
			_ => None,
		})
		.collect();
	let agent = crate::quick_ack::stalling_agent(
		ureq::Agent::config_builder()
			.http_status_as_error(false)
			.accept_encoding("")
			.max_redirects(0)
			.max_redirects_will_error(false)
			.timeout_connect(
				(transfer.connect_timeout != 0)
					.then(|| Duration::from_secs(transfer.connect_timeout)),
			)
			.tls_config(
				TlsConfig::builder()
					.root_certs(RootCerts::Specific(Arc::new(certificates)))
					.build(),
			)
			.user_agent(concat!("noxi/", env!("CARGO_PKG_VERSION")))
			.build(),
		(options.stalled_timeout != 0).then(|| Duration::from_secs(options.stalled_timeout)),
	);
	let mut attempts = 0;
	loop {
		let failure = match attempt(&agent, url, options) {
			Ok(body) => return Ok(body),
			Err(failure) => failure,
		};
		attempts += 1;
		if !failure.transient || attempts >= transfer.attempts {
			return Err(Error::Other(failure.message));
		}
		let base_ms = if matches!(failure.status, 429 | 503) {
			transfer.rate_limited_delay_ms
		} else {
			transfer.delay_ms
		};
		let delay = transfer.retry_delay(attempts, base_ms, failure.retry_after_ms);
		warn(&format!(
			"{}; retrying in {delay} ms (attempt {attempts}/{})",
			failure.message, transfer.attempts
		));
		std::thread::sleep(Duration::from_millis(delay as u64));
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn redirects_resolve_against_the_url() {
		let base = Origin::parse("http://u:p@host:1/a/b?q").unwrap();
		assert_eq!(base.host(), "host");
		assert_eq!(base.url(), "http://host:1/a/b?q");
		for (location, expected) in [
			("https://x/y", "https://x/y"),
			("file:///etc/passwd", "file:///etc/passwd"),
			("//x/y", "http://x/y"),
			("/y", "http://host:1/y"),
			("y", "http://host:1/a/y"),
			("?r", "http://host:1/a/b?r"),
		] {
			assert_eq!(resolve(&base, location), expected);
		}
		assert!(Origin::parse("file:///etc/passwd").is_none());
		assert_eq!(Origin::parse("http://[::1]:80/").unwrap().host(), "::1");
	}
}
