//! `netrc` files as curl reads them, for `netrc-file`.

/// A login and password for a host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
	pub login: String,
	pub password: String,
}

/// The entries of a `netrc` file, in order. An entry without a machine is the
/// `default` one.
#[derive(Clone, Default)]
pub struct Netrc(Vec<(Option<String>, Option<String>, Option<String>)>);

/// The words of a line; `#` starts a comment and double quotes group, with
/// `\n`, `\r`, `\t` and other escaped characters.
fn words(line: &str) -> Vec<String> {
	let mut out = Vec::new();
	let mut chars = line.chars().peekable();
	loop {
		while chars.next_if(|c| c.is_ascii_whitespace()).is_some() {}
		match chars.peek() {
			None | Some('#') => return out,
			Some('"') => {
				chars.next();
				let mut word = String::new();
				while let Some(c) = chars.next() {
					match c {
						'"' => break,
						'\\' => match chars.next() {
							Some('n') => word.push('\n'),
							Some('r') => word.push('\r'),
							Some('t') => word.push('\t'),
							Some(c) => word.push(c),
							None => {}
						},
						c => word.push(c),
					}
				}
				out.push(word);
			}
			Some(_) => {
				let mut word = String::new();
				while let Some(c) = chars.next_if(|c| !c.is_ascii_whitespace()) {
					word.push(c);
				}
				out.push(word);
			}
		}
	}
}

impl Netrc {
	pub fn parse(text: &str) -> Self {
		let mut entries: Vec<(Option<String>, Option<String>, Option<String>)> = Vec::new();
		let mut macro_definition = false;
		// The keyword whose value is the next word, which may be on the next
		// line.
		let mut keyword = "";
		for line in text.lines() {
			if macro_definition {
				// A macro ends with an empty line.
				macro_definition = !line.trim().is_empty();
				continue;
			}
			for word in words(line) {
				match keyword {
					"machine" => entries.push((Some(word), None, None)),
					"login" | "user" => {
						if let Some(entry) = entries.last_mut() {
							entry.1 = Some(word);
						}
					}
					"password" => {
						if let Some(entry) = entries.last_mut() {
							entry.2 = Some(word);
						}
					}
					"account" => {}
					_ => {
						match word.to_ascii_lowercase().as_str() {
							"machine" => keyword = "machine",
							"login" => keyword = "login",
							"password" => keyword = "password",
							"account" => keyword = "account",
							"default" => entries.push((None, None, None)),
							"macdef" => {
								macro_definition = true;
								break;
							}
							_ => {}
						}
						continue;
					}
				}
				keyword = "";
			}
		}
		Self(entries)
	}

	/// The credentials for `host`, of the user `login` if the URL names one:
	/// those of the first entry that matches, counting `default`.
	pub fn find(&self, host: &str, login: Option<&str>) -> Option<Credentials> {
		self.0
			.iter()
			.filter(|(machine, _, _)| {
				machine
					.as_ref()
					.is_none_or(|machine| machine.eq_ignore_ascii_case(host))
			})
			.filter(|(_, user, password)| {
				(user.is_some() || password.is_some())
					&& login.is_none_or(|login| user.as_deref().is_none_or(|user| user == login))
			})
			.map(|(_, user, password)| Credentials {
				login: login
					.map(str::to_owned)
					.or_else(|| user.clone())
					.unwrap_or_default(),
				password: password.clone().unwrap_or_default(),
			})
			.next()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn finds_machines_and_defaults() {
		let netrc = Netrc::parse(
			"# comment\nmachine a.example login u password p # more\nmacdef init\nmachine b\npassword no\n\nmachine B.example\n\tlogin\n\t\"two words\"\n\tpassword \"q\\\"\"\nmachine c account x login l password z\ndefault login anonymous password d\nmachine late login never password never\n",
		);
		let find = |host: &str, login| {
			netrc
				.find(host, login)
				.map(|c| format!("{}:{}", c.login, c.password))
		};
		assert_eq!(find("a.example", None).unwrap(), "u:p");
		assert_eq!(find("A.EXAMPLE", Some("u")).unwrap(), "u:p");
		assert_eq!(find("a.example", Some("v")), None);
		assert_eq!(find("b.example", None).unwrap(), "two words:q\"");
		assert_eq!(find("c", None).unwrap(), "l:z");
		assert_eq!(find("late", None).unwrap(), "anonymous:d");
		assert!(Netrc::parse("").find("a", None).is_none());
		assert!(
			Netrc::parse("machine x login a password b")
				.find("y", None)
				.is_none()
		);
	}
}
