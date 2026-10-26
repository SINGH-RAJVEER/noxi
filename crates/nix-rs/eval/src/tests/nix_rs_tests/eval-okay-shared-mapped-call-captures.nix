[
	(builtins.map (throw "unused map function") [])
	(builtins.mapAttrs (throw "unused mapAttrs function") {})
	(builtins.genList (_: throw "unused genList body") 0)
	(builtins.zipAttrsWith (_: _: throw "unused zipAttrsWith body") [])
	(builtins.map (f: f 10) (builtins.map (x: y: x + y) [1 2 3]))
	(builtins.mapAttrs (name: value: name + toString value) { a = 1; b = 2; })
	(builtins.genList (i: i * i) 4)
	(builtins.zipAttrsWith (name: values: name + builtins.concatStringsSep "," (builtins.map toString values)) [
		{ a = 1; b = 2; }
		{ a = 3; }
	])
]
