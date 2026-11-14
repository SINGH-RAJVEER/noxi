import json

def generate(root):
	BASE = root / 'context-fixtures'
	(BASE / 'allowed/sub').mkdir(parents=True, exist_ok=True)
	(BASE / 'private.nix').write_text('77\n')
	(BASE / 'private.txt').write_text('private fixture\n')
	(BASE / 'allowed/default.nix').write_text('{ answer = 42; }\n')
	(BASE / 'allowed/scope.nix').write_text('x + 1\n')
	(BASE / 'allowed/data').write_text('allowed fixture\n')
	(BASE / 'allowed/sub/deep.nix').write_text('import ../default.nix\n')
	for name, target in [('allowed/escape.nix', '../private.nix'), ('allowed/escape.txt', '../private.txt'), ('allowed/local.nix', 'default.nix')]:
		p = BASE / name
		if not p.is_symlink():
			p.symlink_to(target)

	cases = []
	def add(category, expression, **kw):
		cases.append({'category': category, 'expression': expression, **kw})

	keys = [
		'/not-a-store-path',
		'/nix/store/00000000000000000000000000000000-source',
		'/nix/store/00000000000000000000000000000000-source.drv',
		'/nix/store/00000000000000000000000000000000-source.drv/sub',
		'/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-source',
	]
	fields = [
		'{}', 'null', '1', 'throw "entry"',
		'{ path = true; }', '{ path = false; }', '{ path = 0; }', '{ path = "yes"; }',
		'{ path = throw "path"; }', '{ allOutputs = true; }', '{ allOutputs = false; }',
		'{ allOutputs = null; }', '{ allOutputs = "yes"; }', '{ allOutputs = throw "all"; }',
		'{ outputs = []; }', '{ outputs = [ "out" ]; }', '{ outputs = [ "out" "dev" "out" ]; }',
		'{ outputs = [ 1 ]; }', '{ outputs = [ null ]; }', '{ outputs = [ (throw "output") ]; }',
		'{ outputs = null; }', '{ outputs = "out"; }', '{ outputs = throw "outputs"; }',
		'{ path = false; allOutputs = false; outputs = []; ignored = throw "unused"; }',
		'{ path = 1; allOutputs = throw "unused"; }',
		'{ allOutputs = 1; path = throw "first"; }',
	]
	for key in keys:
		for field in fields:
			inner = f'builtins.appendContext "value" {{ {json.dumps(key)} = {field}; }}'
			for wrap in ['({})', '(builtins.getContext ({}))', '(builtins.hasContext ({}))']:
				for trace in [False, True]:
					add('context-fields', wrap.format(inner), flags=['--show-trace'] if trace else [])

	for origin in ['null', '1', 'true', '[]', '{}', './.', '(throw "origin")', '"s"']:
		for context in ['null', '1', '[]', '"x"', '(throw "second")', '{}']:
			for trace in [False, True]:
				add('context-argument-order', f'builtins.appendContext {origin} {context}', flags=['--show-trace'] if trace else [])

	drv = keys[2]
	for output in ['"out"', '""', '1', 'null', '(throw "out")', f'(builtins.appendContext "out" {{ "{keys[1]}".path = true; }})']:
		for key in keys[1:3]:
			add('context-output-names', f'builtins.getContext (builtins.appendContext "v" {{ "{key}".outputs = [ {output} ]; }})')

	modes = [[], ['--pure-eval'], ['--restrict-eval'], ['--restrict-eval', '-I', str(BASE / 'allowed')], ['--pure-eval', '-I', str(BASE / 'allowed')], ['--pure-eval', '--impure']]
	paths = [BASE / 'allowed', BASE / 'allowed/data', BASE / 'allowed/escape.txt', BASE / 'allowed/../private.txt']
	operations = ['builtins.readFile {}', 'builtins.readDir {}', 'builtins.pathExists {}', 'builtins.readFileType {}',
		'builtins.toString {}', 'builtins.path {{ path = {}; name = "audit"; }}',
		'builtins.filterSource (_: _: true) {}']
	for flags in modes:
		for path in paths:
			for operation in operations:
				add('filesystem-policy', operation.format(str(path)), flags=flags)
		for target in ['allowed/default.nix', 'allowed/local.nix', 'allowed/escape.nix', 'allowed/sub/deep.nix', 'private.nix']:
			for operation in ['import {}', 'scopedImport {{ x = 4; }} {}']:
				add('import-policy', operation.format(str(BASE / target)), flags=flags)
		for expression in ['builtins.getEnv "HOME"', 'builtins.nixPath', 'builtins.currentSystem', 'builtins.storePath (throw "unused")']:
			add('impure-boundary', expression, flags=flags)

	configs = ['', 'experimental-features = nix-command flakes', 'experimental-features = ca-derivations', 'experimental-features = impure-derivations', 'experimental-features = pipe-operators', 'experimental-features = parse-toml-timestamps blake3-hashes']
	for config in configs:
		for control in ['__contentAddressed', '__impure']:
			for value in ['true', 'false', 'null', '1', '(throw "control")']:
				for structured in ['', '__structuredAttrs = true;']:
					add('derivation-features', f'(derivation {{ name = "audit"; system = "x86_64-linux"; builder = "/bin/sh"; {control} = {value}; {structured} }}).drvPath', config=config)
		for expression in ['builtins.attrNames builtins', 'builtins.parseFlakeRef "path:/tmp"', '1 |> (x: x + 2)', '(x: x + 2) <| 1', 'builtins.hashString "blake3" "abc"', 'builtins.fromTOML "x = 1979-05-27T07:32:00Z"']:
			add('builtin-feature-exposure', expression, config=config)
	return cases
