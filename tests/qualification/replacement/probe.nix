# A build that records the identity and isolation it ran with, for the
# daemon mode's build-user and authorization probes. `nonce` makes each
# probe a distinct derivation; `seconds` keeps the builder alive so that
# concurrent probes overlap; `spawn` leaves a background process behind.
{
	nonce,
	seconds ? "0",
	spawn ? "",
}:
let
	pkgs = import <nixpkgs> { };
in
derivation {
	name = "qualification-probe-${nonce}";
	system = "x86_64-linux";
	builder = "${pkgs.bash}/bin/bash";
	PATH = "${pkgs.coreutils}/bin";
	inherit nonce seconds spawn;
	input = pkgs.bash;
	args = [
		"-c"
		''
			mkdir "$out"
			read -r inside host count < /proc/self/uid_map
			echo "$inside $host $count" > "$out/uid_map"
			read -r inside host count < /proc/self/gid_map
			echo "$inside $host $count" > "$out/gid_map"
			id -u > "$out/uid"
			id -g > "$out/gid"
			cat /proc/sys/kernel/hostname > "$out/hostname"
			echo "$input" > "$out/input-path"
			if test -d /build && test "$PWD" = /build; then echo yes; else echo no; fi > "$out/build-dir"
			if chmod u+w "$input" 2>/dev/null || touch "$input/qualification-probe" 2>/dev/null; then
				echo writable
			else
				echo protected
			fi > "$out/input"
			if test -n "$spawn"; then
				sleep 1717 &
			fi
			sleep "$seconds"
		''
	];
}
