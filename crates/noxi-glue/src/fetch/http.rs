//! Injectable HTTP boundary used by files, tarballs, forges and LFS.

use noxi_flakeref::error::{Error, Result};
use std::collections::BTreeMap;
use ureq::ResponseExt;

#[derive(Clone, Debug)]
pub struct Request {
	pub method: String,
	pub url: String,
	pub headers: Vec<(String, String)>,
	pub body: Vec<u8>,
}

impl Request {
	pub fn get(url: &str) -> Self {
		Self {
			method: "GET".into(),
			url: url.into(),
			headers: Vec::new(),
			body: Vec::new(),
		}
	}
}

#[derive(Clone, Debug, Default)]
pub struct Response {
	pub status: u16,
	pub headers: BTreeMap<String, String>,
	pub effective_url: String,
	pub body: Vec<u8>,
}

pub trait Client: std::fmt::Debug {
	fn request(&self, request: &Request) -> Result<Response>;
}

#[derive(Debug, Default)]
pub struct Native;

impl Client for Native {
	fn request(&self, request: &Request) -> Result<Response> {
		let config = ureq::Agent::config_builder()
			.http_status_as_error(false)
			.build();
		let agent: ureq::Agent = config.into();
		let response = if request.method == "POST" {
			let mut call = agent.post(&request.url).header("User-Agent", "Nix/2.35.2");
			for (name, value) in &request.headers {
				call = call.header(name, value);
			}
			call.send(request.body.as_slice())
		} else {
			let mut call = agent.get(&request.url).header("User-Agent", "Nix/2.35.2");
			for (name, value) in &request.headers {
				call = call.header(name, value);
			}
			call.call()
		}
		.map_err(|e| Error::new(e.to_string()))?;
		let status = response.status().as_u16();
		let headers = response
			.headers()
			.iter()
			.filter_map(|(name, value)| {
				value
					.to_str()
					.ok()
					.map(|value| (name.as_str().to_owned(), value.to_owned()))
			})
			.collect();
		let effective_url = response.get_uri().to_string();
		let body = response
			.into_body()
			.with_config()
			.limit(1024 * 1024 * 1024)
			.read_to_vec()
			.map_err(|e| Error::new(e.to_string()))?;
		Ok(Response {
			status,
			headers,
			effective_url,
			body,
		})
	}
}
