"""Every CppNix builtin: argument types, forcing order, arity and global aliases.

`outputOf` and `fetchClosure` are excluded. Fetchers are only exercised on
argument and type validation, never on downloads.
"""

FLAKES = 'experimental-features = nix-command flakes'
DRV = '(derivation { name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; })'
SHA256 = '"sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0="'


def fixtures(root):
	base = root / 'matrix-fixtures'
	(base / 'dir/sub').mkdir(parents=True, exist_ok=True)
	(base / 'file').write_text('contents\n')
	(base / 'empty').write_text('')
	(base / 'dir/a').write_text('a\n')
	(base / 'dir/sub/b').write_text('b\n')
	(base / 'expr.nix').write_text('{ answer = 42; }\n')
	(base / 'scope.nix').write_text('x + 1\n')
	(base / 'fn.nix').write_text('{ a ? 1, b }: a + b\n')
	link = base / 'link'
	if not link.is_symlink():
		link.symlink_to('file')
	return base


def generate(root):
	base = fixtures(root)
	fix = str(base)
	file = fix + '/file'
	cases = []
	seen = set()

	def add(category, expression, flags=None, config=None):
		key = (expression, tuple(flags or []), config or '')
		if key in seen:
			return
		seen.add(key)
		case = {'category': category, 'expression': expression}
		if flags:
			case['flags'] = flags
		if config:
			case['config'] = config
		cases.append(case)

	# Valid arguments for each function builtin, in application order.
	functions = {
		'abort': ['"stop"'],
		'add': ['1', '2'],
		'addDrvOutputDependencies': [f'(builtins.unsafeDiscardOutputDependency {DRV}.drvPath)'],
		'addErrorContext': ['"ctx"', '1'],
		'all': ['(x: x)', '[ true ]'],
		'any': ['(x: x)', '[ false ]'],
		'appendContext': ['"s"', '{}'],
		'attrNames': ['{ b = 1; a = 2; }'],
		'attrValues': ['{ b = 1; a = 2; }'],
		'baseNameOf': ['"/a/b"'],
		'bitAnd': ['6', '3'],
		'bitOr': ['6', '3'],
		'bitXor': ['6', '3'],
		'break': ['1'],
		'catAttrs': ['"a"', '[ { a = 1; } ]'],
		'ceil': ['1.5'],
		'compareVersions': ['"1.0"', '"1.0pre"'],
		'concatLists': ['[ [ 1 ] [ 2 ] ]'],
		'concatMap': ['(x: [ x x ])', '[ 1 ]'],
		'concatStringsSep': ['"-"', '[ "a" "b" ]'],
		'convertHash': [f'{{ hash = {SHA256}; toHashFormat = "nix32"; }}'],
		'deepSeq': ['[ 1 ]', '2'],
		'derivationStrict': ['{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; }'],
		'dirOf': ['"/a/b"'],
		'div': ['7', '2'],
		'elem': ['1', '[ 1 ]'],
		'elemAt': ['[ 1 2 ]', '1'],
		'filter': ['(x: x)', '[ true false ]'],
		'filterSource': ['(p: t: true)', fix + '/dir'],
		'findFile': [f'[ {{ prefix = "fix"; path = "{fix}"; }} ]', '"fix/file"'],
		'floor': ['1.5'],
		"foldl'": ['(a: b: a + b)', '0', '[ 1 2 ]'],
		'fromJSON': ['"[1]"'],
		'fromTOML': ['"a = 1"'],
		'functionArgs': ['({ a ? 1, b }: a)'],
		'genList': ['(x: x)', '3'],
		'genericClosure': ['{ startSet = [ { key = 1; } ]; operator = x: []; }'],
		'getAttr': ['"a"', '{ a = 1; }'],
		'getContext': ['"s"'],
		'getEnv': ['"NIX_PATH"'],
		'groupBy': ['(x: "k")', '[ 1 ]'],
		'hasAttr': ['"a"', '{ a = 1; }'],
		'hasContext': ['"s"'],
		'hashFile': ['"sha256"', file],
		'hashString': ['"sha256"', '"abc"'],
		'head': ['[ 1 ]'],
		'import': [fix + '/expr.nix'],
		'intersectAttrs': ['{ a = 1; }', '{ a = 2; b = 3; }'],
		'isAttrs': ['{}'],
		'isBool': ['true'],
		'isFloat': ['1.5'],
		'isFunction': ['(x: x)'],
		'isInt': ['1'],
		'isList': ['[]'],
		'isNull': ['null'],
		'isPath': ['./.'],
		'isString': ['""'],
		'length': ['[ 1 ]'],
		'lessThan': ['1', '2'],
		'listToAttrs': ['[ { name = "a"; value = 1; } ]'],
		'map': ['(x: x)', '[ 1 ]'],
		'mapAttrs': ['(n: v: v)', '{ a = 1; }'],
		'match': ['"a(b)"', '"ab"'],
		'mul': ['3', '2'],
		'parseDrvName': ['"hello-1.0"'],
		'partition': ['(x: x)', '[ true false ]'],
		'path': [f'{{ path = {fix}/dir; name = "p"; }}'],
		'pathExists': [file],
		'placeholder': ['"out"'],
		'readDir': [fix + '/dir'],
		'readFile': [file],
		'readFileType': [file],
		'removeAttrs': ['{ a = 1; b = 2; }', '[ "a" ]'],
		'replaceStrings': ['[ "a" ]', '[ "b" ]', '"aa"'],
		'scopedImport': ['{ x = 1; }', fix + '/scope.nix'],
		'seq': ['1', '2'],
		'sort': ['builtins.lessThan', '[ 2 1 ]'],
		'split': ['"(b)"', '"abc"'],
		'splitVersion': ['"1.0pre2"'],
		'storePath': ['"/nix/store/00000000000000000000000000000000-x"'],
		'stringLength': ['"abc"'],
		'sub': ['3', '2'],
		'substring': ['1', '1', '"abc"'],
		'tail': ['[ 1 2 ]'],
		'throw': ['"stop"'],
		'toFile': ['"name"', '"contents"'],
		'toJSON': ['[ 1 ]'],
		'toPath': ['"/a"'],
		'toString': ['1'],
		'toXML': ['[ 1 ]'],
		'trace': ['"msg"', '1'],
		'traceVerbose': ['"msg"', '1'],
		'tryEval': ['1'],
		'typeOf': ['1'],
		'unsafeDiscardOutputDependency': ['"s"'],
		'unsafeDiscardStringContext': ['"s"'],
		'unsafeGetAttrPos': ['"a"', '{ a = 1; }'],
		'warn': ['"msg"', '1'],
		'zipAttrsWith': ['(n: v: v)', '[ { a = 1; } ]'],
	}
	flake_functions = {
		'flakeRefToString': ['{ type = "path"; path = "/x"; }'],
		'getFlake': ['"path:/x?narHash=sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA%3D"'],
		'parseFlakeRef': ['"github:a/b"'],
	}
	fetchers = {
		'fetchGit': ['{ url = "/nonexistent"; }'],
		'fetchMercurial': ['{ url = "/nonexistent"; }'],
		'fetchTarball': ['{ url = "/nonexistent"; }'],
		'fetchTree': ['{ type = "file"; url = "/nonexistent"; }'],
		'fetchurl': ['{ url = "/nonexistent"; }'],
	}
	values = ['builtins', 'currentSystem', 'false', 'langVersion', 'nixPath', 'nixVersion',
		'null', 'storeDir', 'true']

	variants = [
		'null', 'true', 'false', '0', '(-1)', '9223372036854775807', '1.5', '(-0.0)',
		'""', '"text"', '"/abs"', '[]', '[ 1 ]', '{}', '{ a = 1; }', '(x: x)',
		'({ a }: a)', 'builtins.add', '(builtins.add 1)', fix + '/file',
		'(throw "argument")', '{ __toString = self: "s"; }', '{ outPath = "/out"; }',
		'(builtins.toFile "f" "c")', '(abort "argument")',
	]
	# Fetchers never see strings, so no download is attempted.
	fetcher_variants = [v for v in variants if not v.startswith('"') and v != fix + '/file'
		and v != '(builtins.toFile "f" "c")']

	def call(name, args):
		return ' '.join(['builtins.' + name, *args])

	def matrix(name, args, choices, config=None):
		for trace in [False, True]:
			flags = ['--show-trace'] if trace else []
			add('matrix-valid', call(name, args), flags, config)
			for index in range(len(args)):
				for value in choices:
					current = args.copy()
					current[index] = value
					add('matrix-type', call(name, current), flags, config)
			if len(args) > 1:
				for index in range(len(args)):
					current = ['(throw "later")'] * len(args)
					current[index] = '{}'
					add('matrix-order', call(name, current), flags, config)

	for name, args in functions.items():
		matrix(name, args, variants)
	for name, args in flake_functions.items():
		matrix(name, args, variants, FLAKES)
		add('matrix-feature-gate', call(name, args))
	for name, args in fetchers.items():
		matrix(name, args, fetcher_variants)
		for attrs in ['{ url = 1; }', '{ url = null; }', '{ url = throw "url"; }',
				'{ url = "/nonexistent"; extra = 1; }', '{ url = "/nonexistent"; name = 1; }',
				'{ url = "/nonexistent"; sha256 = 1; }', '{ url = "/nonexistent"; narHash = "x"; }',
				'{ type = "nonexistent"; }', '{ type = 1; }', '{ name = "n"; }']:
			add('fetcher-attrs', call(name, [attrs]))
			add('fetcher-attrs', call(name, [attrs]), ['--pure-eval'])

	# Arity: printing partial applications, over-application and introspection.
	everything = {**functions, **flake_functions, **fetchers}
	for name, args in everything.items():
		config = FLAKES if name in flake_functions else None
		add('arity', 'builtins.' + name, config=config)
		add('arity', f'builtins.isFunction builtins.{name}', config=config)
		add('arity', f'builtins.functionArgs builtins.{name}', config=config)
		add('arity', f'builtins.typeOf builtins.{name}', config=config)
		add('arity', f'[ builtins.{name} ] == [ builtins.{name} ]', config=config)
		add('arity', f'builtins.{name} == builtins.{name}', config=config)
		if len(args) > 1:
			add('arity', call(name, args[:1]), config=config)
			add('arity', f'builtins.typeOf ({call(name, args[:1])})', config=config)
		if name not in fetchers:
			add('arity', f'({call(name, args)}) 1', config=config)
		add('arity-lazy', call(name, args[:-1]) if len(args) > 1 else 'builtins.' + name, config=config)
	for name in values:
		add('value-builtin', f'builtins.typeOf builtins.{name}')
		add('value-builtin', f'builtins.{name} 1')
		add('value-builtin', f'builtins.isFunction builtins.{name}')
		if name not in ['builtins', 'currentTime']:
			add('value-builtin', f'builtins.{name}')
	add('value-builtin', 'builtins.typeOf builtins.currentTime')
	add('value-builtin', 'builtins.currentTime > 0')
	add('value-builtin', 'builtins.currentTime == builtins.currentTime')
	add('value-builtin', 'builtins.currentTime', ['--pure-eval'])
	add('value-builtin', 'builtins.currentSystem', ['--pure-eval'])
	add('value-builtin', 'builtins.nixPath', ['--pure-eval'])
	add('value-builtin', 'builtins.storeDir', ['--pure-eval'])
	add('value-builtin', 'builtins.attrNames builtins')
	add('value-builtin', 'builtins.attrNames builtins', config=FLAKES)
	add('value-builtin', 'builtins.attrNames builtins', ['--pure-eval'])
	add('value-builtin', 'builtins.length (builtins.attrNames builtins)')
	add('value-builtin', 'builtins.builtins.builtins.true')
	add('value-builtin', 'builtins ? builtins')

	# Global scope: every builtin under `__name`, and the unprefixed globals.
	for name in sorted({*everything, *values, 'currentTime'}):
		config = FLAKES if name in flake_functions else None
		add('global-scope', f'builtins.typeOf __{name}', config=config)
		add('global-scope', f'let r = builtins.tryEval __{name}; in r.success', config=config)
	for name in ['abort', 'baseNameOf', 'break', 'derivation', 'derivationStrict', 'dirOf',
			'fetchGit', 'fetchMercurial', 'fetchTarball', 'fetchTree', 'fetchurl', 'false',
			'fromTOML', 'import', 'isNull', 'map', 'null', 'placeholder', 'removeAttrs',
			'scopedImport', 'throw', 'toString', 'true', 'currentSystem', 'storeDir']:
		add('global-scope', f'builtins.typeOf {name}')
		add('global-scope', f'builtins.typeOf {name}', ['--pure-eval'])
	add('global-scope', 'let builtins = { map = 1; }; in builtins.map')
	add('global-scope', 'let __add = 1; in __add')
	add('global-scope', 'let map = 1; in map')
	add('global-scope', 'with { map = 1; }; map')
	add('global-scope', 'with { __add = 1; }; __add')
	add('global-scope', '__unknownBuiltin')
	add('global-scope', 'builtins.unknownBuiltin')
	add('global-scope', 'builtins.unknownBuiltin or "default"')
	add('global-scope', 'builtins ? unknownBuiltin')
	add('global-scope', 'builtins.derivation == derivation')
	add('global-scope', 'builtins.map == map')
	add('global-scope', 'builtins.typeOf builtins.derivation')

	# Derivations, whose printed form exposes several builtins at once.
	add('derivation-shape', DRV)
	add('derivation-shape', f'builtins.attrNames {DRV}')
	add('derivation-shape', f'{DRV}.drvPath')
	add('derivation-shape', f'{DRV}.outPath')
	add('derivation-shape', f'{DRV}.type')
	add('derivation-shape', f'builtins.getContext {DRV}.drvPath')
	add('derivation-shape', f'builtins.getContext {DRV}.outPath')
	add('derivation-shape', f'builtins.getContext (builtins.unsafeDiscardOutputDependency {DRV}.drvPath)')
	add('derivation-shape', f'builtins.getContext (builtins.addDrvOutputDependencies (builtins.unsafeDiscardOutputDependency {DRV}.drvPath))')
	add('derivation-shape', f'builtins.addDrvOutputDependencies {DRV}.outPath')
	add('derivation-shape', f'builtins.addDrvOutputDependencies {DRV}.drvPath')
	add('derivation-shape', f'builtins.addDrvOutputDependencies "{{x}}"')
	add('derivation-shape', f'builtins.addDrvOutputDependencies ({DRV}.drvPath + {DRV}.outPath)')
	add('derivation-shape', f'builtins.unsafeDiscardStringContext {DRV}.drvPath')
	add('derivation-shape', f'builtins.hasContext {DRV}.outPath')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; outputs = [ "out" "dev" ]; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; outputs = [ "out" "out" ]; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; outputs = []; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; outputs = [ "drv" ]; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; args = [ 1 ]; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; env = 1; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "a b"; system = "x86_64-linux"; builder = "/bin/sh"; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = ""; system = "x86_64-linux"; builder = "/bin/sh"; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "x.drv"; system = "x86_64-linux"; builder = "/bin/sh"; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; builder = "/bin/sh"; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ system = "x86_64-linux"; builder = "/bin/sh"; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; x = null; y = true; z = false; f = 1.5; i = -3; l = [ 1 "a" null ]; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; x = {{}}; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; x = (y: y); }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; outputHash = "abc"; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; outputHash = {SHA256}; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; outputHash = {SHA256}; outputHashMode = "recursive"; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; outputHash = {SHA256}; outputHashMode = "bad"; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; outputHash = {SHA256}; outputHashAlgo = "md5"; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; __structuredAttrs = true; x = {{ a = [ 1 ]; }}; }}')
	add('derivation-shape', f'builtins.derivationStrict {{ name = "d"; system = "x86_64-linux"; builder = "/bin/sh"; __ignoreNulls = true; x = null; }}')
	return cases
