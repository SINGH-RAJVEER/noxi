//! `nix flake init`, `flake new` and `flake clone`, following CppNix 2.35.2
//! `flake.cc` (`CmdFlakeInitCommon`, `CmdFlakeClone`) and the fetchers'
//! `InputScheme::clone`.
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use bstr::ByteSlice;
use noxi_eval::Value;
use noxi_flakeref::input::Scheme;
use noxi_flakeref::registry::{UseRegistries, lookup_in_registries};

use super::Parsed;
use crate::cli::Failure;
use crate::instantiate::{self, Options, nix_string_literal};

fn notice(message: &str) {
	if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Notice) {
		eprintln!("{message}");
	}
}

fn print_error(message: &str) {
	if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Error) {
		eprintln!("{message}");
	}
}

/// `PathFmt`: a path in double quotes, as `std::quoted` writes it.
pub(super) fn quoted(path: &Path) -> String {
	let mut out = String::from("\"");
	for c in path.to_string_lossy().chars() {
		if c == '"' || c == '\\' {
			out.push('\\');
		}
		out.push(c);
	}
	out.push('"');
	out
}

/// `absPath`: absolute and lexically clean, without resolving symlinks.
fn abs_path(path: &Path) -> Result<PathBuf, Failure> {
	Ok(instantiate::absolute_path(&std::env::current_dir()?, path))
}

/// The physical location of a logical store path in the selected store.
fn physical(parsed: &Parsed, path: &str) -> Result<PathBuf, Failure> {
	let Some(rest) = path.strip_prefix(nix_compat::store_path::store_dir_with_slash()) else {
		return Ok(PathBuf::from(path));
	};
	let uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
	let real = crate::store::open(&uri, &parsed.settings)?.and_then(|s| s.real_store_dir);
	Ok(match real {
		Some(dir) => dir.join(rest),
		None => PathBuf::from(path),
	})
}

struct Copy {
	changed: Vec<PathBuf>,
	conflicted: usize,
}

fn copy_template(from: &Path, to: &Path, copy: &mut Copy) -> Result<(), Failure> {
	fs::create_dir_all(to)?;
	let mut entries: Vec<_> = fs::read_dir(from)?.collect::<Result<_, _>>()?;
	entries.sort_by_key(|entry| entry.file_name());
	for entry in entries {
		let from2 = entry.path();
		let to2 = to.join(entry.file_name());
		let st = fs::symlink_metadata(&from2)?;
		let existing = fs::symlink_metadata(&to2).ok();
		let kind = st.file_type();
		if kind.is_dir() {
			copy_template(&from2, &to2, copy)?;
		} else if kind.is_file() {
			let contents = fs::read(&from2)?;
			if existing.is_some() {
				if fs::read(&to2)? != contents {
					print_error(&format!(
						"refusing to overwrite existing file {}\n please merge it manually with '{}'",
						quoted(&to2),
						from2.display()
					));
					copy.conflicted += 1;
				} else {
					notice(&format!("skipping identical file: {}", from2.display()));
				}
				continue;
			}
			// `writeFile` creates with mode 0666 less the umask.
			fs::OpenOptions::new()
				.write(true)
				.create_new(true)
				.mode(0o666)
				.open(&to2)?
				.write_all(&contents)?;
		} else if kind.is_symlink() {
			let target = fs::read_link(&from2)?;
			if existing.is_some() {
				if fs::read_link(&to2)? != target {
					print_error(&format!(
						"refusing to overwrite existing file {}\n please merge it manually with '{}'",
						quoted(&to2),
						from2.display()
					));
					copy.conflicted += 1;
				} else {
					notice(&format!("skipping identical file: {}", from2.display()));
				}
				continue;
			}
			std::os::unix::fs::symlink(&target, &to2)?;
		} else {
			return Err(Failure::Error(format!(
				"path '{}' needs to be a symlink, file, or directory but instead is a unknown",
				from2.display()
			)));
		}
		notice(&format!("wrote: {}", quoted(&to2)));
		copy.changed.push(to2);
	}
	Ok(())
}

pub(super) fn init(parsed: Parsed, operation: &str) -> Result<ExitCode, Failure> {
	let dest = if operation == "new" {
		parsed
			.operands
			.first()
			.cloned()
			.ok_or_else(|| Failure::Usage("more arguments are required".into()))?
	} else {
		".".into()
	};
	let flake_dir = abs_path(Path::new(&dest))?;
	let template = parsed.value("template").unwrap_or("templates").to_owned();
	let (reference, fragment) = template.split_once('#').unwrap_or((&template, ""));
	let candidates: Vec<String> = if fragment.is_empty() {
		vec!["templates.default".into(), "defaultTemplate".into()]
	} else if let Some(exact) = fragment.strip_prefix('.') {
		vec![exact.to_owned()]
	} else {
		vec![format!("templates.{fragment}"), fragment.to_owned()]
	};
	let mut request = super::flakes::request(&parsed, "outputs")?;
	request.no_write = true;
	let mut options = Options {
		eval: true,
		strict: true,
		read_write_mode: true,
		from_args: true,
		store: parsed.value("store").map(str::to_owned),
		settings: parsed.settings.clone(),
		flake: Some(request),
		..Default::default()
	};
	options
		.settings
		.set("pure-eval", "false")
		.map_err(|e| e.to_string())?;
	let reference = nix_string_literal(reference);
	let candidates: Vec<String> = candidates.iter().map(|c| nix_string_literal(c)).collect();
	options.inputs.push(format!(
		"__noxi_flake_select (__noxi_flake {reference}) [ {} ] (__noxi_flake_label {reference})",
		candidates.join(" ")
	));
	let evaluated = match instantiate::evaluate(&options, &std::env::current_dir()?) {
		Ok(value) => value,
		Err(code) => return Ok(code),
	};
	let Some(Value::Attrs(result)) = evaluated.results.first() else {
		return Err(Failure::Error(
			"template evaluation produced no value".into(),
		));
	};
	let text = |value: &Value| -> Result<Option<String>, Failure> {
		let value = match value {
			Value::Thunk(thunk) if thunk.is_evaluated() => thunk.value().clone(),
			value => value.clone(),
		};
		match value {
			Value::String(s) => Ok(Some(s.to_str().map_err(|e| e.to_string())?.to_owned())),
			Value::Path(p) => Ok(Some(p.to_string_lossy().into_owned())),
			_ => Ok(None),
		}
	};
	let template_dir = match result.select("path") {
		Some(path) => text(path)?.unwrap_or_default(),
		None => return Err(Failure::Error("attribute 'path' missing".into())),
	};
	let welcome = match result.select("welcomeText") {
		Some(value) => text(value)?,
		None => None,
	};
	let welcome = match welcome {
		Some(text) => Some(render_markdown(&text)?),
		None => None,
	};
	drop(evaluated);
	let from = physical(&parsed, &template_dir)?;
	let mut copy = Copy {
		changed: Vec::new(),
		conflicted: 0,
	};
	copy_template(&from, &flake_dir, &mut copy)?;
	if !copy.changed.is_empty() && flake_dir.join(".git").exists() {
		let output = Command::new("git")
			.arg("-C")
			.arg(&flake_dir)
			.args(["add", "--intent-to-add", "--force", "--"])
			.args(&copy.changed)
			.stdin(Stdio::null())
			.stderr(Stdio::inherit())
			.output()?;
		if !output.status.success() {
			return Err(Failure::Error(format!(
				"program 'git' failed with exit code {}",
				output.status.code().unwrap_or(1)
			)));
		}
	}
	if let Some(welcome) = welcome {
		notice("\n");
		notice(&welcome);
	}
	if copy.conflicted > 0 {
		return Err(Failure::Error(format!(
			"encountered {} conflicts - see above",
			copy.conflicted
		)));
	}
	Ok(ExitCode::SUCCESS)
}

/// lowdown's terminal renderer without ANSI, for the Markdown subset that
/// templates use: headings, paragraphs filled to 60 columns, flat lists,
/// fenced code, block quotes and inline emphasis, code and links. Other
/// constructs fail explicitly rather than rendering differently.
/// `_NIX_TEST_RAW_MARKDOWN=1` returns the text unchanged, as CppNix does.
fn render_markdown(markdown: &str) -> Result<String, Failure> {
	if std::env::var("_NIX_TEST_RAW_MARKDOWN").is_ok_and(|v| v == "1") {
		return Ok(markdown.to_owned());
	}
	if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
		return Err(super::unsupported(
			"flake init welcome text on a terminal (ANSI Markdown rendering)",
		));
	}
	const COLS: usize = 60;
	let unsupported =
		|what: &str| super::unsupported(&format!("flake init welcome text with {what}"));
	let mut blocks: Vec<String> = Vec::new();
	let lines: Vec<&str> = markdown.lines().collect();
	let mut i = 0;
	let fill = |words: &str, prefix: &str| -> String {
		let mut out = Vec::new();
		let mut line = String::new();
		for word in words.split_whitespace() {
			if !line.is_empty() && prefix.len() + line.len() + 1 + word.chars().count() > COLS {
				out.push(format!("{prefix}{line}"));
				line.clear();
			}
			if !line.is_empty() {
				line.push(' ');
			}
			line.push_str(word);
		}
		if !line.is_empty() {
			out.push(format!("{prefix}{line}"));
		}
		out.join("\n")
	};
	while i < lines.len() {
		let line = lines[i];
		let trimmed = line.trim_start();
		if trimmed.is_empty() {
			i += 1;
			continue;
		}
		if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
			let fence = &trimmed[..3];
			let mut body = Vec::new();
			i += 1;
			while i < lines.len() && !lines[i].trim_start().starts_with(fence) {
				body.push(format!("  \u{2502} {}", lines[i]));
				i += 1;
			}
			i += 1;
			blocks.push(body.join("\n"));
			continue;
		}
		if let Some(rest) = trimmed.strip_prefix('#') {
			let level = 1 + rest.chars().take_while(|c| *c == '#').count();
			let title = inline(rest.trim_start_matches('#').trim(), &unsupported)?;
			blocks.push(if level == 1 {
				title
			} else {
				format!("{} {title}", "#".repeat(level))
			});
			i += 1;
			continue;
		}
		if trimmed.starts_with('>') {
			let mut text = Vec::new();
			while i < lines.len() && lines[i].trim_start().starts_with('>') {
				text.push(lines[i].trim_start()[1..].trim().to_owned());
				i += 1;
			}
			blocks.push(fill(&inline(&text.join(" "), &unsupported)?, "  \u{2502} "));
			continue;
		}
		let bullet = |l: &str| {
			let l = l.trim_start();
			["- ", "* ", "+ "].iter().any(|b| l.starts_with(b))
		};
		let number = |l: &str| {
			let l = l.trim_start();
			let digits = l.chars().take_while(char::is_ascii_digit).count();
			digits > 0 && l[digits..].starts_with(". ")
		};
		if bullet(line) || number(line) {
			let mut items = Vec::new();
			let mut index = 0;
			while i < lines.len() && !lines[i].trim().is_empty() {
				let l = lines[i].trim_start();
				if bullet(l) {
					items.push(format!(
						"  \u{b7} {} ",
						inline(l[2..].trim(), &unsupported)?
					));
				} else if number(l) {
					index += 1;
					let digits = l.chars().take_while(char::is_ascii_digit).count();
					items.push(format!(
						" {index}. {} ",
						inline(l[digits + 2..].trim(), &unsupported)?
					));
				} else {
					return Err(unsupported("multi-line list items"));
				}
				i += 1;
			}
			blocks.push(items.join("\n"));
			continue;
		}
		if trimmed.starts_with('|') || trimmed.starts_with('<') || line.starts_with("    ") {
			return Err(unsupported("tables, HTML or indented code"));
		}
		let mut text = Vec::new();
		while i < lines.len() && !lines[i].trim().is_empty() {
			let l = lines[i].trim_start();
			if l.starts_with('#') || l.starts_with("```") || l.starts_with('>') || bullet(l) {
				break;
			}
			text.push(l.trim_end());
			i += 1;
		}
		blocks.push(fill(&inline(&text.join(" "), &unsupported)?, ""));
	}
	let mut out = blocks.join("\n\n");
	out.push('\n');
	Ok(out)
}

/// Inline Markdown: emphasis markers removed, code spans kept as text,
/// links rendered as `TEXT URL`.
fn inline(text: &str, unsupported: &dyn Fn(&str) -> Failure) -> Result<String, Failure> {
	let mut out = String::new();
	let chars: Vec<char> = text.chars().collect();
	let mut i = 0;
	while i < chars.len() {
		match chars[i] {
			'`' => {
				let end = chars[i + 1..]
					.iter()
					.position(|c| *c == '`')
					.ok_or_else(|| unsupported("unterminated code spans"))?;
				out.extend(&chars[i + 1..i + 1 + end]);
				i += end + 2;
			}
			'*' | '_' => i += 1,
			'[' => {
				let close = chars[i..]
					.iter()
					.position(|c| *c == ']')
					.ok_or_else(|| unsupported("unterminated links"))?;
				let label: String = chars[i + 1..i + close].iter().collect();
				if chars.get(i + close + 1) != Some(&'(') {
					return Err(unsupported("reference links"));
				}
				let rest = &chars[i + close + 2..];
				let end = rest
					.iter()
					.position(|c| *c == ')')
					.ok_or_else(|| unsupported("unterminated links"))?;
				let url: String = rest[..end].iter().collect();
				out.push_str(&inline(&label, unsupported)?);
				if url.contains("://") {
					out.push(' ');
					out.push_str(&url);
				}
				i += close + 2 + end + 1;
			}
			'\\' if i + 1 < chars.len() => {
				out.push(chars[i + 1]);
				i += 2;
			}
			c => {
				out.push(c);
				i += 1;
			}
		}
	}
	Ok(out)
}

pub(super) fn clone(parsed: Parsed) -> Result<ExitCode, Failure> {
	let Some(dest) = parsed.value("dest").map(PathBuf::from) else {
		return Err(Failure::Error("missing flag '--dest'".into()));
	};
	super::flakes::request(&parsed, "outputs")?;
	let reference = parsed.operands.first().map(String::as_str).unwrap_or(".");
	let store = noxi_glue::EvaluatorStore::new();
	let mut config = noxi_glue::fetch::FetchConfig {
		cwd: std::env::current_dir()?,
		..Default::default()
	};
	config.settings.flakes = true;
	config.settings.use_registries = parsed.settings.get_bool("use-registries", true);
	config.settings.flake_registry = parsed
		.settings
		.get("flake-registry")
		.unwrap_or("https://channels.nixos.org/flake-registry.json")
		.into();
	store.configure_fetching(config);
	store.extend_flake_overrides(
		parsed
			.flags
			.get("override-flake")
			.into_iter()
			.flatten()
			.map(|v| (v[0].clone(), v[1].clone()))
			.collect(),
	);
	let parsed_ref = noxi_glue::fetch::flake::parse_reference(&store, reference)
		.map_err(|e| Failure::Error(e.to_string()))?;
	let registries = store.registries().map_err(|e| e.to_string())?;
	let (mut input, _) = lookup_in_registries(&registries, &parsed_ref.input, UseRegistries::All)
		.map_err(|e| Failure::Error(e.to_string()))?;
	// GitHub, GitLab and SourceHut inputs clone their Git repository.
	if let Some(scheme @ (Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut)) = input.scheme {
		let attr = |name: &str| noxi_flakeref::input::maybe_get_str_attr(&input.attrs, name);
		let owner = attr("owner")
			.map_err(|e| e.to_string())?
			.unwrap_or_default();
		let repo = attr("repo").map_err(|e| e.to_string())?.unwrap_or_default();
		let host = attr("host").map_err(|e| e.to_string())?.unwrap_or_else(|| {
			match scheme {
				Scheme::GitHub => "github.com",
				Scheme::GitLab => "gitlab.com",
				_ => "git.sr.ht",
			}
			.into()
		});
		let url = match scheme {
			Scheme::SourceHut => format!("git+https://{host}/{owner}/{repo}"),
			_ => format!("git+https://{host}/{owner}/{repo}.git"),
		};
		let settings = store.fetch_settings();
		let reference = input.get_ref().map_err(|e| e.to_string())?;
		let rev = input.get_rev().map_err(|e| e.to_string())?;
		input = noxi_flakeref::input::Input::from_url_string(&settings, &url, true)
			.and_then(|i| i.apply_overrides(reference.as_deref(), rev.as_ref()))
			.map_err(|e| e.to_string())?;
	}
	if input.scheme == Some(Scheme::Git) {
		let location = match input.get_source_path() {
			Some(path) => path.to_string_lossy().into_owned(),
			None => noxi_flakeref::input::maybe_get_str_attr(&input.attrs, "url")
				.map_err(|e| e.to_string())?
				.unwrap_or_default(),
		};
		let mut git = Command::new("git");
		git.arg("clone").arg(location);
		if let Some(r) = input.get_ref().map_err(|e| e.to_string())? {
			git.arg("--branch").arg(r);
		}
		if input.get_rev().map_err(|e| e.to_string())?.is_some() {
			return Err(Failure::Error(
				"cloning a specific revision is not implemented".into(),
			));
		}
		let output = git.arg(&dest).stderr(Stdio::inherit()).output()?;
		if !output.status.success() {
			return Err(Failure::Error(format!(
				"program 'git' failed with exit code {}",
				output.status.code().unwrap_or(1)
			)));
		}
		return Ok(ExitCode::SUCCESS);
	}
	if input.scheme.is_none() {
		return Err(Failure::Error(format!(
			"cannot clone unsupported input '{}'",
			noxi_flakeref::input::attrs_to_json(&input.attrs)
				.map(|j| serde_json::Value::Object(j).to_string())
				.unwrap_or_default()
		)));
	}
	if dest.exists() || fs::symlink_metadata(&dest).is_ok() {
		return Err(Failure::Error(format!(
			"cannot clone into existing path {}",
			quoted(&dest)
		)));
	}
	let source = match input.get_source_path() {
		Some(path) if input.scheme == Some(Scheme::Path) => path,
		_ => {
			let reference = noxi_flakeref::flake_ref::FlakeRef::new(input, "");
			let fetched =
				noxi_glue::fetch::flake::fetch_source(&store, &reference, UseRegistries::All)
					.map_err(|e| Failure::Error(e.to_string()))?;
			store.physical_path(&fetched.out_path)
		}
	};
	restore(&source, &dest)?;
	Ok(ExitCode::SUCCESS)
}

/// `copyRecursive` into a `RestoreSink`: regular files keep only their
/// executable bit, as a NAR does.
fn restore(from: &Path, to: &Path) -> Result<(), Failure> {
	let st = fs::symlink_metadata(from)?;
	if st.file_type().is_symlink() {
		std::os::unix::fs::symlink(fs::read_link(from)?, to)?;
	} else if st.is_dir() {
		fs::create_dir(to)?;
		let mut entries: Vec<_> = fs::read_dir(from)?.collect::<Result<_, _>>()?;
		entries.sort_by_key(|entry| entry.file_name());
		for entry in entries {
			restore(&entry.path(), &to.join(entry.file_name()))?;
		}
	} else {
		let executable = st.permissions().mode() & 0o100 != 0;
		fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.mode(if executable { 0o777 } else { 0o666 })
			.open(to)?
			.write_all(&fs::read(from)?)?;
	}
	Ok(())
}
