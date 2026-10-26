//! The command line of `nix`, read as CppNix 2.35.2's `Args::parseCmdline`
//! and `MultiCommand` read it.
//!
//! Arguments are read from left to right. A compound short flag such as
//! `-Lv` is expanded where a flag is expected, `--` ends the flags, and
//! every other argument starting with `-` must be a flag: the global flags
//! are looked up first, then those of each selected command from the
//! outermost to the innermost. A flag takes its arity's values verbatim and
//! is handled at once, so a handler's error stops parsing. Positional
//! arguments select the subcommands, through the top-level aliases, and are
//! then collected for the innermost command's expected arguments.

use std::collections::VecDeque;

use super::table::{COMMANDS, GLOBAL_FLAGS};
use crate::cli::Failure;

/// A flag of a command or a global flag, from the reference's `__dump-cli`.
#[derive(Debug)]
pub struct Flag {
	pub name: &'static str,
	pub short: Option<char>,
	/// The number of values; `None` takes the rest of the command line.
	pub arity: Option<usize>,
	/// The experimental feature that must be enabled once the flag is used.
	pub feature: Option<&'static str>,
}

/// A positional argument a command expects.
#[derive(Debug)]
pub struct Arg {
	pub label: &'static str,
	/// The number of values; `None` takes every remaining one.
	pub arity: Option<usize>,
	pub optional: bool,
}

/// A `nix` command.
#[derive(Debug)]
pub struct Command {
	/// The words after `nix`, such as `hash convert`.
	pub path: &'static str,
	/// The experimental feature the command needs.
	pub feature: Option<&'static str>,
	pub flags: &'static [Flag],
	pub args: &'static [Arg],
}

impl Command {
	/// The command with the words `path` after `nix`.
	pub fn find(path: &str) -> Option<&'static Command> {
		COMMANDS
			.binary_search_by(|command| command.path.cmp(path))
			.ok()
			.map(|i| &COMMANDS[i])
	}

	/// Whether the command only selects a subcommand.
	pub fn is_multi(&self) -> bool {
		matches!(
			self.args,
			[Arg {
				label: "subcommand",
				arity: Some(1),
				optional: true
			}]
		)
	}

	/// The names of the subcommands of the command at `path`, or of the
	/// top-level commands when `path` is empty.
	pub fn subcommands(path: &str) -> impl Iterator<Item = &'static str> + '_ {
		COMMANDS.iter().filter_map(move |command| {
			let rest = if path.is_empty() {
				command.path
			} else {
				command.path.strip_prefix(path)?.strip_prefix(' ')?
			};
			(!rest.contains(' ')).then_some(rest)
		})
	}
}

/// Find a flag given as `--name` or `-c` among `flags`.
pub fn find_flag(flags: &'static [Flag], arg: &str) -> Option<&'static Flag> {
	if let Some(name) = arg.strip_prefix("--") {
		flags.iter().find(|flag| flag.name == name)
	} else {
		let mut chars = arg.chars();
		let (Some('-'), Some(c), None) = (chars.next(), chars.next(), chars.next()) else {
			return None;
		};
		flags.iter().find(|flag| flag.short == Some(c))
	}
}

/// The top-level aliases of `nix`: deprecated ones warn when used.
const ALIASES: &[(&str, bool, &[&str])] = &[
	("add-to-store", true, &["store", "add-path"]),
	("cat-nar", true, &["nar", "cat"]),
	("cat-store", true, &["store", "cat"]),
	("copy-sigs", true, &["store", "copy-sigs"]),
	("dev-shell", true, &["develop"]),
	("diff-closures", true, &["store", "diff-closures"]),
	("doctor", true, &["config", "check"]),
	("dump-path", true, &["store", "dump-path"]),
	("hash-file", true, &["hash", "file"]),
	("hash-path", true, &["hash", "path"]),
	("ls-nar", true, &["nar", "ls"]),
	("ls-store", true, &["store", "ls"]),
	(
		"make-content-addressable",
		true,
		&["store", "make-content-addressed"],
	),
	("optimise-store", true, &["store", "optimise"]),
	("ping-store", true, &["store", "info"]),
	("realisation", true, &["store", "build-trace"]),
	("shell", false, &["env", "shell"]),
	("show-config", true, &["config", "show"]),
	("show-derivation", true, &["derivation", "show"]),
	("sign-paths", true, &["store", "sign"]),
	("to-base16", true, &["hash", "to-base16"]),
	("to-base32", true, &["hash", "to-base32"]),
	("to-base64", true, &["hash", "to-base64"]),
	("verify", true, &["store", "verify"]),
];

/// What a command does with its flags and arguments.
pub trait Handler<C> {
	/// A flag of this command, by the name it was registered under (an
	/// alias has its own name), with its values.
	fn flag(&mut self, ctx: &mut C, name: &str, values: Vec<String>) -> Result<(), Failure>;

	/// The values of the expected argument `label`.
	fn args(&mut self, ctx: &mut C, label: &str, values: Vec<String>) -> Result<(), Failure>;
}

/// The callbacks the parser needs from the program.
pub trait Program: Sized {
	type Handler: Handler<Self>;

	/// Handle a global flag.
	fn global_flag(&mut self, flag: &'static Flag, values: Vec<String>) -> Result<(), Failure>;

	/// The handler for a selected command.
	fn handler(&mut self, command: &'static Command) -> Self::Handler;

	/// Print a warning.
	fn warn(&mut self, message: &str);
}

/// A selected command with the arguments it still expects.
pub struct Level<H> {
	pub command: &'static Command,
	pub handler: H,
	expected: VecDeque<&'static Arg>,
}

/// The result of parsing: the selected commands from the outermost, and
/// the experimental features the used flags need, in order of first use.
pub struct Parsed<H> {
	pub levels: Vec<Level<H>>,
	pub flag_features: Vec<&'static str>,
}

/// `Suggestions::bestMatches(...).trim()` formatted as CppNix appends it
/// to an error.
pub fn did_you_mean<'a>(candidates: impl Iterator<Item = &'a str>, query: &str) -> String {
	let mut matches: Vec<(usize, &str)> = candidates
		.map(|candidate| (levenshtein(query, candidate), candidate))
		.collect();
	matches.sort();
	matches.dedup();
	let matches: Vec<&str> = matches
		.into_iter()
		.take_while(|(distance, _)| *distance <= 2)
		.take(5)
		.map(|(_, candidate)| candidate)
		.collect();
	match matches.as_slice() {
		[] => String::new(),
		[one] => format!("\nDid you mean {one}?"),
		[first, middle @ .., last] => {
			let mut s = format!("\nDid you mean one of {first}");
			for m in middle {
				s.push_str(", ");
				s.push_str(m);
			}
			format!("{s} or {last}?")
		}
	}
}

fn levenshtein(a: &str, b: &str) -> usize {
	let (a, b) = (a.as_bytes(), b.as_bytes());
	let mut v0: Vec<usize> = (0..=b.len()).collect();
	let mut v1 = vec![0; b.len() + 1];
	for (i, ca) in a.iter().enumerate() {
		v1[0] = i + 1;
		for (j, cb) in b.iter().enumerate() {
			let substitution = if ca == cb { v0[j] } else { v0[j] + 1 };
			v1[j + 1] = (v0[j + 1] + 1).min(v1[j] + 1).min(substitution);
		}
		std::mem::swap(&mut v0, &mut v1);
	}
	v0[b.len()]
}

/// Parse `args`, the command line after the program name.
pub fn parse<P: Program>(
	program: &mut P,
	args: Vec<String>,
) -> Result<Parsed<P::Handler>, Failure> {
	let mut parser = Parser {
		levels: Vec::new(),
		flag_features: Vec::new(),
		alias_used: false,
	};
	let mut queue: VecDeque<String> = args.into();
	let mut dash_dash = false;
	let mut pending = Vec::new();
	while let Some(mut arg) = queue.pop_front() {
		if !dash_dash {
			let bytes = arg.as_bytes();
			if bytes.len() > 2
				&& bytes[0] == b'-'
				&& bytes[1] != b'-'
				&& bytes[1].is_ascii_alphabetic()
			{
				let mut rest = Vec::new();
				for (i, c) in arg.char_indices().skip(2) {
					if c.is_ascii_alphabetic() {
						rest.push(format!("-{c}"));
					} else {
						rest.push(arg[i..].to_owned());
						break;
					}
				}
				for part in rest.into_iter().rev() {
					queue.push_front(part);
				}
				arg.truncate(2);
			}
		}
		if !dash_dash && arg == "--" {
			dash_dash = true;
		} else if !dash_dash && arg.starts_with('-') {
			if !parser.process_flag(program, &arg, &mut queue)? {
				return Err(Failure::Usage(format!("unrecognised flag '{arg}'")));
			}
		} else {
			if parser.levels.is_empty() && !parser.alias_used {
				parser.alias_used = true;
				if let Some((_, deprecated, replacement)) =
					ALIASES.iter().find(|(name, _, _)| *name == arg)
				{
					if *deprecated {
						program.warn(&format!(
							"'{arg}' is a deprecated alias for '{}'",
							replacement.join(" ")
						));
					}
					for word in replacement.iter().rev() {
						queue.push_front((*word).to_owned());
					}
					continue;
				}
			}
			pending.push(arg);
			if parser.process_args(program, &pending, false)? {
				pending.clear();
			}
		}
	}
	parser.process_args(program, &pending, true)?;
	Ok(Parsed {
		levels: parser.levels,
		flag_features: parser.flag_features,
	})
}

struct Parser<H> {
	levels: Vec<Level<H>>,
	flag_features: Vec<&'static str>,
	alias_used: bool,
}

impl<H> Parser<H> {
	fn process_flag<P: Program<Handler = H>>(
		&mut self,
		program: &mut P,
		arg: &str,
		queue: &mut VecDeque<String>,
	) -> Result<bool, Failure>
	where
		H: Handler<P>,
	{
		let (flag, level) = match find_flag(GLOBAL_FLAGS, arg) {
			Some(flag) => (flag, None),
			None => match self
				.levels
				.iter()
				.enumerate()
				.find_map(|(i, level)| Some((find_flag(level.command.flags, arg)?, Some(i))))
			{
				Some(found) => found,
				None => return Ok(false),
			},
		};
		let shown = if arg.starts_with("--") {
			format!("--{}", flag.name)
		} else {
			arg.to_owned()
		};
		if let Some(feature) = flag.feature
			&& !self.flag_features.contains(&feature)
		{
			self.flag_features.push(feature);
		}
		let values: Vec<String> = match flag.arity {
			None => queue.drain(..).collect(),
			Some(arity) => {
				if queue.len() < arity {
					return Err(Failure::Usage(format!(
						"flag '{shown}' requires {arity} argument(s), but only {} were given",
						queue.len()
					)));
				}
				queue.drain(..arity).collect()
			}
		};
		match level {
			None => program.global_flag(flag, values)?,
			Some(i) => self.levels[i].handler.flag(program, flag.name, values)?,
		}
		Ok(true)
	}

	/// `Args::processArgs` of the innermost selected command, or of `nix`
	/// itself before a command is selected.
	fn process_args<P: Program<Handler = H>>(
		&mut self,
		program: &mut P,
		args: &[String],
		finish: bool,
	) -> Result<bool, Failure>
	where
		H: Handler<P>,
	{
		let Some(level) = self.levels.last_mut() else {
			if let [name] = args {
				self.select(program, "", name)?;
				return Ok(true);
			}
			return Ok(false);
		};
		let Some(&expected) = level.expected.front() else {
			if let Some(arg) = args.first() {
				return Err(Failure::Usage(format!("unexpected argument '{arg}'")));
			}
			return Ok(true);
		};
		let mut done = false;
		if (expected.arity.is_none() && finish) || expected.arity == Some(args.len()) {
			level.expected.pop_front();
			if level.command.is_multi() {
				let path = level.command.path;
				self.select(program, path, &args[0])?;
			} else {
				level.handler.args(program, expected.label, args.to_vec())?;
			}
			done = true;
		}
		if finish
			&& let Some(level) = self.levels.last()
			&& level.expected.front().is_some_and(|arg| !arg.optional)
		{
			return Err(Failure::Usage("more arguments are required".into()));
		}
		Ok(done)
	}

	/// Select the subcommand `name` of the command at `parent`.
	fn select<P: Program<Handler = H>>(
		&mut self,
		program: &mut P,
		parent: &str,
		name: &str,
	) -> Result<(), Failure> {
		let name = if parent == "store" && name == "ping" {
			program.warn("'ping' is a deprecated alias for 'info'");
			"info"
		} else {
			name
		};
		let path = if parent.is_empty() {
			name.to_owned()
		} else {
			format!("{parent} {name}")
		};
		let command = Command::find(&path)
			.filter(|_| !name.contains(' ') && !name.is_empty())
			.ok_or_else(|| {
				Failure::Usage(format!(
					"'{name}' is not a recognised command{}",
					did_you_mean(Command::subcommands(parent), name)
				))
			})?;
		let handler = program.handler(command);
		self.levels.push(Level {
			command,
			handler,
			expected: command.args.iter().collect(),
		});
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn suggests_close_command_names() {
		assert_eq!(
			did_you_mean(["convert", "file", "path"].into_iter(), "conver"),
			"\nDid you mean convert?"
		);
		assert_eq!(did_you_mean(["convert"].into_iter(), "bogus"), "");
		assert_eq!(
			did_you_mean(["path", "patch", "bath"].into_iter(), "pat"),
			"\nDid you mean one of path, bath or patch?"
		);
	}

	#[test]
	fn finds_commands_and_subcommands() {
		assert!(Command::find("hash").unwrap().is_multi());
		assert!(!Command::find("hash convert").unwrap().is_multi());
		assert!(Command::subcommands("hash").any(|name| name == "to-sri"));
		assert!(Command::subcommands("").any(|name| name == "store"));
		assert!(!Command::subcommands("").any(|name| name == "convert"));
	}
}
