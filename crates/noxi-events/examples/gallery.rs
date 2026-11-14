//! Renders every output shape `noxi-events` produces, in color when
//! standard error is a terminal (or `CLICOLOR_FORCE=1`), for reviewing the
//! design: `cargo run -p noxi-events --example gallery`.

use std::io::{self, Write};
use std::time::Duration;

use noxi_events::status::timed;
use noxi_events::style::{self, Styling};
use noxi_events::{
	ActivityId, Diagnostic, Label, LineSplitter, LogExcerpt, Source, Tone, codes, status,
};

fn heading(out: &mut dyn Write, styling: &Styling, title: &str) {
	let _ = writeln!(
		out,
		"\n{}\n",
		styling.paint(style::DIM, &format!("── {title}"))
	);
}

fn main() {
	let styling = Styling::for_stderr();
	let err = &mut io::stderr();

	heading(err, &styling, "noxi system switch: success");
	for (verb, text) in [
		(
			"Resolved",
			"switch flake /etc/nixos#nixosConfigurations.\"host\".config.system.build.toplevel, lock /etc/nixos/flake.lock sha256 9c1e…, revision 3f9a1c2",
		),
		(
			"Updated",
			"/etc/nixos/flake.lock: 1 input changed, 3 unchanged\nchanged  nixpkgs  github:NixOS/nixpkgs/5e2a59a -> github:NixOS/nixpkgs/b1f7a43",
		),
		(
			"Evaluating",
			"nixosConfigurations.\"host\".config.system.build.toplevel of /etc/nixos",
		),
		(
			"Built",
			"/nix/store/q6si19fj3x7376dzj67wdsazl5c8m4d1-nixos-system-host-25.11",
		),
		(
			"Preparing",
			"switch: profile /nix/var/nix/profiles/system at generation 118 -> generation 119",
		),
		(
			"Activating",
			"running /nix/store/q6si…-nixos-system-host-25.11/bin/switch-to-configuration switch",
		),
		("Verified", "running system changed, boot default changed"),
	] {
		let _ = status(err, &styling, verb, Tone::Progress, text);
	}
	let _ = timed(
		err,
		&styling,
		"Finished",
		Tone::Progress,
		"switch",
		"in",
		Duration::from_secs(134),
	);

	heading(err, &styling, "evaluation error with a suggestion");
	let text = "{ pkgs, ... }:\n{\n  environment.systemPackages = [ pkgz.git ];\n}\n";
	let start = text.find("pkgz").unwrap();
	let diagnostic = Diagnostic::error(Some(codes::EVALUATION), "undefined variable 'pkgz'")
		.with_source(Source {
			path: "/etc/nixos/configuration.nix".into(),
			text: text.into(),
			line_start: 1,
			primary: Label {
				range: start..start + 4,
				text: Some("not defined here".into()),
			},
			secondary: vec![Label {
				range: 2..6,
				text: Some("did you mean this argument?".into()),
			}],
		})
		.note("evaluation stopped before any build or system change")
		.suggest(
			"a variable with a similar name exists",
			start..start + 4,
			"pkgs",
		);
	let _ = err.write_all(diagnostic.render(&styling).as_bytes());

	heading(err, &styling, "build failure with a log excerpt");
	let diagnostic = Diagnostic::error(
		Some(codes::BUILD),
		"build of `openssl-3.4.1` failed in buildPhase",
	)
	.with_log(LogExcerpt {
		path: "/nix/var/log/noxi/ops/1/builds/openssl-3.4.1.log".into(),
		line_start: 4410,
		lines: vec![
			"CC crypto/evp/e_aes.o".into(),
			"crypto/evp/e_aes.c:812:5: error: implicit declaration of function 'foo'".into(),
			"make[1]: *** [Makefile:4412: crypto/evp/e_aes.o] Error 1".into(),
		],
		highlight: vec![1, 2],
	})
	.note("builder exited with status 2 after 1m 42s")
	.note("required by `nginx-1.27.3` -> `nixos-system-host-25.11`")
	.help("see the full log with `noxi logs openssl-3.4.1`");
	let _ = err.write_all(diagnostic.render(&styling).as_bytes());
	let _ = timed(
		err,
		&styling,
		"Failed",
		Tone::Failure,
		"switch",
		"after",
		Duration::from_secs(108),
	);

	heading(err, &styling, "noxi system: activation failure");
	let diagnostic = Diagnostic::error(
		Some(codes::ACTIVATION_FAILED),
		"\"/nix/store/q6si…/bin/switch-to-configuration switch\" exit status: 4; activation may have applied part of its changes",
	)
	.note("`noxi system switch` stopped in the activate phase")
	.note("the profile now points at generation 119 (boot default changed); the running system link is unchanged")
	.help("inspect the activation output above; `noxi system rollback` returns the profile to the previous generation");
	let _ = err.write_all(diagnostic.render(&styling).as_bytes());
	let _ = timed(
		err,
		&styling,
		"Failed",
		Tone::Failure,
		"switch",
		"after",
		Duration::from_millis(41_200),
	);

	heading(
		err,
		&styling,
		"noxi system: refused before changing anything",
	);
	let diagnostic = Diagnostic::error(
		Some(codes::ACTIVATION_REFUSED),
		"'noxi system switch' changes the running system or its profile and must run as root; nothing was changed",
	)
	.note("`noxi system switch` stopped in the prepare activation phase")
	.help("run 'noxi system switch' as root");
	let _ = err.write_all(diagnostic.render(&styling).as_bytes());
	let _ = timed(
		err,
		&styling,
		"Stopped",
		Tone::Failure,
		"switch",
		"after",
		Duration::from_millis(3),
	);

	heading(err, &styling, "warning and note");
	let _ = err.write_all(
		Diagnostic::warning(
			None,
			"cannot write the snapshot journal: permission denied; continuing without a record",
		)
		.render(&styling)
		.as_bytes(),
	);
	let _ = writeln!(
		err,
		"{}: --no-snapshot: no snapshot record is written",
		styling.paint(style::NOTE, "note")
	);

	heading(
		err,
		&styling,
		"parallel builds, redirected output (splitter and prefix colors; renderer not wired yet)",
	);
	let (openssl, nginx) = (ActivityId(1), ActivityId(2));
	let names = |id: ActivityId| if id == openssl { "openssl" } else { "nginx" };
	let mut splitter = LineSplitter::new();
	let mut lines = Vec::new();
	let chunks: [(ActivityId, &[u8]); 6] = [
		(openssl, b"CC crypto/evp/e_a"),
		(nginx, b"checking for PCRE2 library ... "),
		(openssl, b"es.o\nCC crypto/evp/e_chacha20_poly1305.o\n"),
		(nginx, b"found\nchecking for zlib ... found\n"),
		(openssl, b"CC crypto/sha/sha256.o\n"),
		(nginx, b"creating objs/Makefile\n"),
	];
	for (id, chunk) in chunks {
		splitter.push(id, chunk, |line| lines.push((id, line)));
	}
	let width = lines
		.iter()
		.map(|(id, _)| names(*id).len())
		.max()
		.unwrap_or(0);
	for (id, line) in lines {
		let name = names(id);
		let color = anstyle::Style::new()
			.fg_color(Some(style::prefix_color(name).into()))
			.bold();
		let _ = writeln!(
			err,
			"{} {line}",
			styling.paint(color, &format!("{name:>width$} >"))
		);
	}
}
