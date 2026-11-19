# The noxios manual: the repository's docs/*.md as HTML pages and as
# section 7 manual pages, and `noxios-help`, which opens the HTML index the
# way `nixos-help` opens the NixOS manual.
{ config, lib, pkgs, ... }:
let
	repository = "https://github.com/SINGH-RAJVEER/nix-rs/blob/main";
	docs = lib.fileset.toSource {
		root = ../../docs;
		fileset = lib.fileset.fileFilter (file: file.hasExt "md") ../../docs;
	};

	manual = pkgs.runCommand "noxios-manual-${config.system.nixos.release}" {
		nativeBuildInputs = [ pkgs.lowdown ];
	} ''
		html="$out/share/doc/noxios"
		man="$out/share/man/man7"
		mkdir -p "$html" "$man"
		{
			echo "# noxios manual"
			echo
			echo "The documentation of noxios ${config.system.nixos.label} and its package manager, noxi."
			echo "Each page is also a manual page: \`man noxios-NAME\`."
			echo
		} > index.md
		for page in ${docs}/*.md; do
			name="$(basename "$page" .md)"
			title="$(sed -n 's/^# //p;T;q' "$page")"
			echo "- [''${title:-$name}]($name.md) (\`noxios-$name\`)" >> index.md
			# Links between pages point at their HTML; links out of docs/
			# point at the repository.
			lowdown -s -Thtml -M "title=''${title:-$name}" "$page" \
				| sed -E 's#href="([A-Za-z0-9_-]+)\.md(\#[^"]*)?"#href="\1.html\2"#g; s#href="\.\./([^"]*)"#href="${repository}/\1"#g' \
				> "$html/$name.html"
			upper="$(echo "noxios-$name" | tr a-z A-Z)"
			lowdown -s -Tman -M "title=$upper" -M section=7 "$page" > "$man/noxios-$name.7"
		done
		lowdown -s -Thtml -M "title=noxios manual" index.md \
			| sed -E 's#href="([A-Za-z0-9_-]+)\.md"#href="\1.html"#g' > "$html/index.html"
		lowdown -s -Tman -M title=NOXIOS -M section=7 index.md > "$man/noxios.7"
	'';

	help = pkgs.writeShellScriptBin "noxios-help" ''
		# noxios-help [PAGE]: opens the noxios manual, or one of its pages
		# (`noxios-help system`), in $BROWSER, xdg-open or w3m.
		page="${manual}/share/doc/noxios/''${1:-index}.html"
		if [ ! -e "$page" ]; then
			echo "noxios-help: no manual page '$1'; the pages are:" >&2
			ls ${manual}/share/doc/noxios | sed 's/\.html$//' >&2
			exit 1
		fi
		# Finds first executable browser in a colon-separated list.
		# (see how xdg-open defines BROWSER)
		browser="$(
			IFS=: ; for b in $BROWSER; do
				[ -n "$(type -P "$b" || true)" ] && echo "$b" && break
			done
		)"
		if [ -z "$browser" ]; then
			browser="$(type -P xdg-open || true)"
			if [ -z "$browser" ]; then
				browser="${pkgs.w3m-nographics}/bin/w3m"
			fi
		fi
		exec "$browser" "$page"
	'';
in
{
	environment.systemPackages = [ manual help ];
	system.build.noxiosManual = manual;
}
