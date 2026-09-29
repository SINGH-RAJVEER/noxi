let
	system = "x86_64-linux";
	# Two fixed-output sources with equal content and name but different builders:
	# the same output path, different derivation paths, the same hash modulo.
	source = builder: derivation {
		name = "source";
		inherit system builder;
		outputHashMode = "flat";
		outputHashAlgo = "sha256";
		outputHash = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
	};
	# Two intermediates that differ only in which source they use, so their
	# masked forms, and therefore their hashes modulo, are equal.
	intermediate = src: derivation {
		name = "intermediate";
		inherit system src;
		builder = "/bin/sh";
		outputs = [ "out" "dev" ];
	};
	a = intermediate (source "/bin/sh");
	b = intermediate (source "/bin/bash");
in
derivation {
	name = "parent";
	inherit system;
	builder = "/bin/sh";
	first = a.out;
	second = b.dev;
}
