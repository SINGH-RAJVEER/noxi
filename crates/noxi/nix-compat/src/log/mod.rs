//! Contains types Nix uses for its logging, visible in the "internal-json" log
//! messages as well as in nix-daemon communication.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
#[cfg(feature = "serde")]
use tracing::warn;

/// Every "internal-json" log line emitted by Nix has this prefix.
pub const AT_NIX_PREFIX: &str = "@nix ";

/// The different verbosity levels Nix distinguishes.
#[derive(
	Clone, Debug, Eq, PartialEq, num_enum::TryFromPrimitive, num_enum::IntoPrimitive, Default,
)]
#[cfg_attr(
	feature = "serde",
	derive(Serialize, Deserialize),
	serde(try_from = "u64", into = "u64")
)]
#[repr(u64)]
pub enum VerbosityLevel {
	#[default]
	Error = 0,
	Warn = 1,
	Notice = 2,
	Info = 3,
	Talkative = 4,
	Chatty = 5,
	Debug = 6,
	Vomit = 7,
}

impl std::fmt::Display for VerbosityLevel {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"{}",
			match self {
				VerbosityLevel::Error => "error",
				VerbosityLevel::Warn => "warn",
				VerbosityLevel::Notice => "notice",
				VerbosityLevel::Info => "info",
				VerbosityLevel::Talkative => "talkative",
				VerbosityLevel::Chatty => "chatty",
				VerbosityLevel::Debug => "debug",
				VerbosityLevel::Vomit => "vomit",
			}
		)
	}
}

/// The different types of log messages Nix' `internal-json` format can
/// represent.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde",
    derive(Serialize, Deserialize),
    serde(tag = "action", rename_all = "camelCase" /*, deny_unknown_fields */))]
// TODO: deny_unknown_fields doesn't seem to work in the testcases below
pub enum LogMessage<'a> {
	Start {
		#[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
		fields: Option<Vec<Field<'a>>>,
		id: u64,
		level: VerbosityLevel,
		parent: u64,
		text: std::borrow::Cow<'a, str>,
		r#type: ActivityType,
	},

	Stop {
		id: u64,
	},

	Result {
		fields: Vec<Field<'a>>,
		id: u64,
		r#type: ResultType,
	},

	// FUTUREWORK: there sometimes seems to be column/file/line fields set to null, and a raw_msg field,
	// see msg_with_raw_msg testcase. These should be represented.
	Msg {
		level: VerbosityLevel,
		msg: std::borrow::Cow<'a, str>,
	},

	// Log lines like these are sent by nixpkgs stdenv, present in `nix log` outputs of individual builds.
	// They are also interpreted by Nix to re-emit [Self::Result]-style messages.
	SetPhase {
		phase: &'a str,
	},
}

#[cfg(feature = "serde")]
fn serialize_bytes_as_string<S>(b: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
	S: serde::Serializer,
{
	match std::str::from_utf8(b) {
		Ok(s) => serializer.serialize_str(s),
		Err(_) => {
			warn!("encountered invalid utf-8 in JSON");
			serializer.serialize_bytes(b)
		}
	}
}

/// Fields in a log message can be either ints or strings.
/// Sometimes, Nix also uses invalid UTF-8 in here, so we use BStr.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(untagged))]
pub enum Field<'a> {
	Int(u64),
	String(
		#[cfg_attr(
			feature = "serde",
			serde(serialize_with = "serialize_bytes_as_string", borrow)
		)]
		std::borrow::Cow<'a, [u8]>,
	),
}

#[derive(Clone, Debug, Eq, PartialEq, num_enum::TryFromPrimitive, num_enum::IntoPrimitive)]
#[cfg_attr(
	feature = "serde",
	derive(Serialize, Deserialize),
	serde(try_from = "u8", into = "u8")
)]
#[repr(u8)]
pub enum ActivityType {
	Unknown = 0,
	CopyPath = 100,
	FileTransfer = 101,
	Realise = 102,
	CopyPaths = 103,
	Builds = 104,
	Build = 105,
	OptimiseStore = 106,
	VerifyPaths = 107,
	Substitute = 108,
	QueryPathInfo = 109,
	PostBuildHook = 110,
	BuildWaiting = 111,
	FetchTree = 112,
}

impl std::fmt::Display for ActivityType {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"{}",
			match self {
				ActivityType::Unknown => "unknown",
				ActivityType::CopyPath => "copy-path",
				ActivityType::FileTransfer => "file-transfer",
				ActivityType::Realise => "realise",
				ActivityType::CopyPaths => "copy-paths",
				ActivityType::Builds => "builds",
				ActivityType::Build => "build",
				ActivityType::OptimiseStore => "optimise-store",
				ActivityType::VerifyPaths => "verify-paths",
				ActivityType::Substitute => "substitute",
				ActivityType::QueryPathInfo => "query-path-info",
				ActivityType::PostBuildHook => "post-build-hook",
				ActivityType::BuildWaiting => "build-waiting",
				ActivityType::FetchTree => "fetch-tree",
			}
		)
	}
}

#[derive(Clone, Debug, Eq, PartialEq, num_enum::TryFromPrimitive, num_enum::IntoPrimitive)]
#[cfg_attr(
	feature = "serde",
	derive(Serialize, Deserialize),
	serde(try_from = "u8", into = "u8")
)]
#[repr(u8)]
pub enum ResultType {
	FileLinked = 100,
	BuildLogLine = 101,
	UntrustedPath = 102,
	CorruptedPath = 103,
	SetPhase = 104,
	Progress = 105,
	SetExpected = 106,
	PostBuildLogLine = 107,
	FetchStatus = 108,
}

impl<'a> LogMessage<'a> {
	/// Parses a given log message string into a [LogMessage].
	#[cfg(feature = "serde")]
	pub fn from_json_str(s: &'a str) -> Result<Self, Error> {
		let s = s.strip_prefix(AT_NIX_PREFIX).ok_or(Error::MissingPrefix)?;

		Ok(serde_json::from_str(s)?)
	}
}

#[cfg(feature = "serde")]
impl std::fmt::Display for LogMessage<'_> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"{AT_NIX_PREFIX}{}",
			serde_json::to_string(self).expect("Failed to serialize LogMessage")
		)
	}
}

#[cfg(feature = "serde")]
#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("Missing @nix prefix")]
	MissingPrefix,

	#[error("Failed to deserialize: {0}")]
	FailedDeserialize(#[from] serde_json::Error),
}

#[cfg(test)]
// prevent assert_matches! from complaining about expected_message being unused,
// while it *is* compared.
#[allow(unused_variables)]
mod test {
	#[cfg(feature = "serde")]
	use std::borrow::Cow;

	use super::VerbosityLevel;
	#[cfg(feature = "serde")]
	use super::{ActivityType, Field, LogMessage, ResultType};
	#[cfg(feature = "serde")]
	use rstest::rstest;

	#[test]
	fn verbosity_level() {
		assert_eq!(
			VerbosityLevel::try_from(0).expect("must succeed"),
			VerbosityLevel::Error
		);
		assert_eq!(VerbosityLevel::default(), VerbosityLevel::Error);

		// Nix can be caused to send a verbosity level larger than itself knows about,
		// (by passing too many -v arguments) but we reject this.
		VerbosityLevel::try_from(42).expect_err("must fail parsing");
	}

	#[cfg(feature = "serde")]
	#[rstest]
	#[case::start(
        r#"@nix {"action":"start","id":1264799149195466,"level":5,"parent":0,"text":"copying '/nix/store/rfqxfljma55x8ybmyg07crnarvqx62sr-nixpkgs-src/pkgs/development/compilers/llvm/18/llvm/lit-shell-script-runner-set-dyld-library-path.patch' to the store","type":0}"#,
        LogMessage::Start {
            fields: None,
            id: 1264799149195466,
            level: VerbosityLevel::Chatty,
            parent: 0,
            text: "copying '/nix/store/rfqxfljma55x8ybmyg07crnarvqx62sr-nixpkgs-src/pkgs/development/compilers/llvm/18/llvm/lit-shell-script-runner-set-dyld-library-path.patch' to the store".into(),
            r#type: ActivityType::Unknown,
        },
        true
    )]
	#[case::stop(
        r#"@nix {"action":"stop","id":1264799149195466}"#,
        LogMessage::Stop {
            id: 1264799149195466,
        },
        true
    )]
	#[case::start_with_fields(
        r#"@nix {"action":"start","fields":["/nix/store/j3hy9syhvyqhghb13vk1433h81q50wcc-rust_tvix-store-0.1.0-linked","https://cache.nixos.org"],"id":1289035649646595,"level":4,"parent":0,"text":"querying info about '/nix/store/j3hy9syhvyqhghb13vk1433h81q50wcc-rust_tvix-store-0.1.0-linked' on 'https://cache.nixos.org'","type":109}"#,
        LogMessage::Start { fields: Some(vec![Field::String(b"/nix/store/j3hy9syhvyqhghb13vk1433h81q50wcc-rust_tvix-store-0.1.0-linked".into()),Field::String(b"https://cache.nixos.org".into())]), id: 1289035649646595, level: VerbosityLevel::Talkative, parent: 0, text: "querying info about '/nix/store/j3hy9syhvyqhghb13vk1433h81q50wcc-rust_tvix-store-0.1.0-linked' on 'https://cache.nixos.org'".into(), r#type: ActivityType::QueryPathInfo },
        true
    )]
	#[case::result(
        r#"@nix {"action":"result","fields":[0,0,0,0],"id":1289035649646594,"type":105}"#,
        LogMessage::Result {
            id: 1289035649646594,
            fields: vec![Field::Int(0), Field::Int(0), Field::Int(0), Field::Int(0)],
            r#type: ResultType::Progress
        },
        true
    )]
	#[case::msg(
        r#"@nix {"action":"msg","level":3,"msg":"  /nix/store/zdxxlb3p1vaq1dgh6vfc7c1c52ry4n2f-rust_opentelemetry-semantic-conventions-0.27.0.drv"}"#,
        LogMessage::Msg { level: VerbosityLevel::Info, msg: "  /nix/store/zdxxlb3p1vaq1dgh6vfc7c1c52ry4n2f-rust_opentelemetry-semantic-conventions-0.27.0.drv".into() },
        true
    )]
	#[case::msg_with_raw_msg(
        r#"@nix {"action":"msg","column":null,"file":null,"level":0,"line":null,"msg":"\u001b[31;1merror:\u001b[0m interrupted by the user","raw_msg":"interrupted by the user"}"#,
        LogMessage::Msg {
            level: VerbosityLevel::Error,
            msg: "\u{001b}[31;1merror:\u{001b}[0m interrupted by the user".into(),
        },
        // FUTUREWORK: represent file/column/line/raw_msg and drop the expected_roundtrip arg alltogether
        false
    )]
	#[case::result_with_fields_int(
        r#"@nix {"action":"result","fields":[101,146944],"id":15116785938335501,"type":106}"#,
        LogMessage::Result { fields: vec![
            Field::Int(101),
            Field::Int(146944),
        ], id: 15116785938335501, r#type: ResultType::SetExpected },
        true
    )]
	#[case::set_phase(
        r#"@nix {"action":"setPhase","phase":"unpackPhase"}"#,
        LogMessage::SetPhase {
            phase: "unpackPhase"
        },
        true
    )]
	#[case::set_phase_result(
        r#"@nix {"action":"result","fields":["unpackPhase"],"id":418969764757508,"type":104}"#,
        LogMessage::Result {
            fields: vec![Field::String(b"unpackPhase".into())],
            id: 418969764757508,
            r#type: ResultType::SetPhase,
        },
        true
    )]
	fn serialize_deserialize(
		#[case] input_str: &str,
		#[case] expected_logmessage: LogMessage,
		#[case] expected_roundtrip: bool,
	) {
		pretty_assertions::assert_matches!(
			LogMessage::from_json_str(input_str),
			expected_logmessage,
			"Expected from_str to return the expected LogMessage"
		);

		if expected_roundtrip {
			assert_eq!(
				input_str,
				expected_logmessage.to_string(),
				"Expected LogMessage to roundtrip to input_str"
			);
		}
	}

	#[cfg(feature = "serde")]
	#[rstest]
	#[case::numeric("0", Field::Int(0))]
	#[case::string(r#""test!""#, Field::String(Cow::Borrowed(b"test!")))]
	// This one is actually owned, but we check for Eq here and it doesn't care.
	// See test_string_fields_cow below for checking CoW.
	#[case::string_escaped(r#""test\\a""#, Field::String(Cow::Borrowed(b"test\\a")))]
	fn test_fields(#[case] input_str: &str, #[case] expected_output: Field) {
		assert_eq!(
			expected_output,
			serde_json::from_str::<Field>(input_str).expect("must deserialize")
		);
	}

	#[cfg(feature = "serde")]
	#[test]
	fn test_string_fields_cow() {
		use pretty_assertions::assert_matches;

		assert_matches!(
			serde_json::from_str::<Field>(r#""test!""#).expect("must deserialize"),
			Field::String(Cow::Borrowed(_))
		);
		assert_matches!(
			serde_json::from_str::<Field>(r#""test\\a""#).expect("must deserialize"),
			Field::String(Cow::Owned(_))
		);
	}
}

/// CppNix's `filterANSIEscapes`, as its simple logger applies it to every
/// line: escape sequences are removed, except colour (SGR) sequences unless
/// `filter_all`, tabs expand to the next multiple of eight columns, and
/// carriage returns and bells are dropped.
pub fn filter_ansi_escapes(s: &[u8], filter_all: bool) -> Vec<u8> {
	let mut out = Vec::with_capacity(s.len());
	let mut width = 0usize;
	let mut i = 0;
	while i < s.len() {
		match s[i] {
			0x1b => {
				let start = i;
				i += 1;
				let mut last = 0u8;
				if s.get(i) == Some(&b'[') {
					i += 1;
					while i < s.len() && (0x30..=0x3f).contains(&s[i]) {
						i += 1;
					}
					while i < s.len() && (0x20..=0x2f).contains(&s[i]) {
						i += 1;
					}
					if i < s.len() && (0x40..=0x7e).contains(&s[i]) {
						last = s[i];
						i += 1;
					}
				} else if s.get(i) == Some(&b']') {
					i += 1;
					while i < s.len() && s[i] != 0x1b && s[i] != 0x07 {
						i += 1;
					}
					if i < s.len() {
						let terminator = s[i];
						i += 1;
						if terminator == 0x1b && s.get(i) == Some(&b'\\') {
							last = b'\\';
							i += 1;
						}
					}
				} else if i < s.len() && (0x40..=0x5f).contains(&s[i]) {
					i += 1;
				}
				if !filter_all && last == b'm' {
					out.extend_from_slice(&s[start..i]);
				}
			}
			b'\t' => {
				loop {
					width += 1;
					out.push(b' ');
					if width % 8 == 0 {
						break;
					}
				}
				i += 1;
			}
			b'\r' | 0x07 => i += 1,
			byte => {
				// Columns only matter for tab stops: control characters take
				// none, and other characters one.
				let len = match byte {
					0xc0..=0xdf => 2,
					0xe0..=0xef => 3,
					0xf0..=0xf7 => 4,
					_ => 1,
				}
				.min(s.len() - i);
				if byte >= 0x20 && byte != 0x7f && !(0x80..0xc0).contains(&byte) {
					width += 1;
				}
				out.extend_from_slice(&s[i..i + len]);
				i += len;
			}
		}
	}
	out
}

#[cfg(test)]
mod filter_tests {
	use super::filter_ansi_escapes;

	#[test]
	fn filters_escapes_like_cppnix() {
		let text = b"\x1b[35;1mwarning:\x1b[0m a\tb\r\x1b]8;;http://x\x1b\\link\x1b]8;;\x07";
		assert_eq!(filter_ansi_escapes(text, true), b"warning: a      blink");
		assert_eq!(
			filter_ansi_escapes(text, false),
			b"\x1b[35;1mwarning:\x1b[0m a      blink"
		);
	}
}

/// An error message as CppNix's `showErrorInfo` prints it without colour:
/// `error: ` and the message, continuation lines indented under it, and
/// each line's trailing whitespace removed, as `indent` does.
pub fn format_error(message: &str) -> String {
	let mut out = String::new();
	for (i, line) in message
		.trim_end_matches([' ', '\n', '\r', '\t'])
		.split('\n')
		.enumerate()
	{
		if i > 0 {
			out.push('\n');
		}
		let prefix = if i == 0 { "error: " } else { "       " };
		out.push_str(format!("{prefix}{line}").trim_end_matches([' ', '\n', '\r', '\t']));
	}
	out
}

#[cfg(test)]
mod format_error_tests {
	#[test]
	fn indents_continuation_lines_and_blank_lines_stay_empty() {
		assert_eq!(
			super::format_error("conflict:\n\n  a\n  b\n"),
			"error: conflict:\n\n         a\n         b"
		);
	}
}

static VERBOSITY: std::sync::atomic::AtomicU64 =
	std::sync::atomic::AtomicU64::new(VerbosityLevel::Info as u64);
static PRINT_BUILD_LOGS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// The process's verbosity, CppNix's global `verbosity`: `lvlInfo` unless
/// changed by `--verbose`, `--quiet` or `--debug`.
pub fn verbosity() -> u64 {
	VERBOSITY.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_verbosity(level: u64) {
	VERBOSITY.store(level, std::sync::atomic::Ordering::Relaxed);
}

/// Whether a message at `level` is printed.
pub fn log_enabled(level: VerbosityLevel) -> bool {
	level as u64 <= verbosity()
}

/// Whether build output is printed, as the simple logger's
/// `printBuildLogs`: on until `--no-build-output` or a raw `--log-format`.
pub fn print_build_logs() -> bool {
	PRINT_BUILD_LOGS.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_print_build_logs(print: bool) {
	PRINT_BUILD_LOGS.store(print, std::sync::atomic::Ordering::Relaxed);
}

static PREFIX_BUILD_LOGS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether printed build log lines carry `NAME> `, as the `bar` loggers'
/// `ProgressBar` prints them with `--print-build-logs`.
pub fn prefix_build_logs() -> bool {
	PREFIX_BUILD_LOGS.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_prefix_build_logs(prefix: bool) {
	PREFIX_BUILD_LOGS.store(prefix, std::sync::atomic::Ordering::Relaxed);
}
