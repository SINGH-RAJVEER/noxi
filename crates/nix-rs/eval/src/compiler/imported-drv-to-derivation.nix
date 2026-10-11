# CppNix 2.35.2 src/libexpr/imported-drv-to-derivation.nix (LGPL-2.1-or-later).
attrs@{ drvPath, outputs, name, ... }:
let
	commonAttrs = (builtins.listToAttrs outputsList) // {
		all = map (x: x.value) outputsList;
		inherit drvPath name;
		type = "derivation";
	};
	outputToAttrListElement = outputName: {
		name = outputName;
		value = commonAttrs // {
			outPath = builtins.getAttr outputName attrs;
			inherit outputName;
		};
	};
	outputsList = map outputToAttrListElement outputs;
in (builtins.head outputsList).value
