def generate(root):
	def q(text):
		return '"' + text.replace('\\', '\\\\').replace('"', '\\"').replace('\n', '\\n').replace('\r', '\\r').replace('\t', '\\t').replace('${', '\\${') + '"'


	cases = []
	def add(category, expression, flags=None):
		case = {'category': category, 'expression': expression}
		if flags:
			case['flags'] = flags
		cases.append(case)


	patterns = [
		'', '.', '..', '.*', '.+', '.?', '^', '$', '^$', '^a', 'a$', '^a$',
		'a', 'b', 'a|aa', 'aa|a', '(a|aa)', '(aa|a)', '(a|ab)(b|)', '(ab|a)(b|)',
		'(a*)(a*)', '(a+)(a*)', '(a?)(a*)', '(a|b)*', '(a|ab)*', '(a(b)?)*',
		'a*', 'a+', 'a?', 'a{0}', 'a{1}', 'a{0,1}', 'a{1,2}', 'a{0,3}',
		'a**', 'a+?', 'a??', '(a*)*', '(|a)', '(a|)', '()', '()a', 'a()',
		'[ab]', '[^ab]', '[a-c]', '[-a]', '[a-]', '[]a]', '[^]a]', '[[:alpha:]]',
		'[[:digit:]]', '[[:space:]]', '[[:word:]]', '[[.hyphen.]]', '[[=a=]]',
		'é', 'é+', '[é]', '(é)', '\\.', '\\+', '\\(', '\\d', '\\n', '\\1',
		'[', '(', ')', '*', '+', '?', '{', 'a{', 'a{2,1}', 'a{,2}', '(?:a)',
		'(?=a)', 'a|', '|a', '[z-a]', '[a&&b]', '[a~b]', 'a$b', '^a|b$',
	]
	subjects = ['', 'a', 'b', 'aa', 'ab', 'aba', 'aaa', 'baab', 'a\nb', '\n', 'é', 'éa']
	for pattern in patterns:
		for subject in subjects:
			for fn in ['match', 'split']:
				add('regex-generated', f'builtins.{fn} {q(pattern)} {q(subject)}')

	fixture_root = root / 'string-fixtures'
	fixture_root.mkdir(exist_ok=True)
	for i, pattern in enumerate([b'\xff', b'(\xff)', b'[\xff]', b'\xff[', b'[\xff', b'(\xff', b'\xff)']):
		pattern_path = fixture_root / f'pattern-{i}'
		pattern_path.write_bytes(pattern)
		for j, subject in enumerate([b'\xff', b'\xffa\n\x80', b'\x00\xff']):
			subject_path = fixture_root / f'subject-{j}'
			subject_path.write_bytes(subject)
			for fn in ['match', 'split']:
				add('regex-raw-bytes', f'builtins.{fn} (builtins.readFile {q(str(pattern_path))}) (builtins.readFile {q(str(subject_path))})')

	strings = ['', 'a', 'ab', 'aaa', 'a\nb', 'é', 'éa', '${x}', '\\"', 'a b']
	for string in strings:
		add('string-length', f'builtins.stringLength {q(string)}')
		for start in [-2, -1, 0, 1, 2, 4]:
			for length in [-2, -1, 0, 1, 2, 4]:
				add('substring', f'builtins.substring ({start}) ({length}) {q(string)}')
		for old, new in [([''], ['x']), (['a'], ['x']), (['a', 'aa'], ['x', 'y']), (['aa', 'a'], ['x', 'y']), (['', 'a'], ['X', 'Y']), (['a', ''], ['X', 'Y']), (['a', 'a'], ['X', 'Y']), (['a'], []), ([], ['x'])]:
			add('replaceStrings', f'builtins.replaceStrings [ {" ".join(q(s) for s in old)} ] [ {" ".join(q(s) for s in new)} ] {q(string)}')
		for separator in ['', ',', 'é']:
			add('concatStringsSep', f'builtins.concatStringsSep {q(separator)} [ {q(string)} {q(string)} ]')

	for value in ['null', 'true', '1', '1.5', '[]', '{}', 'x: x', './.']:
		for fn, args in [('stringLength', ''), ('substring', '0 1 '), ('concatStringsSep', '"" '), ('fromJSON', ''), ('fromTOML', ''), ('match', '"" '), ('split', '"" ')]:
			add('string-type-errors', f'builtins.{fn} {args}({value})')
			add('string-type-errors-trace', f'builtins.{fn} {args}({value})', ['--show-trace'])

	json_texts = [
		'null', 'true', 'false', '0', '-0', '1', '-1', '9223372036854775807',
		'9223372036854775808', '-9223372036854775809', '1e308', '1e309', '-1e309',
		'1e-400', '1.0', '1e0', '-0.0', '[]', '{}', '[1,null,true,"é"]',
		'{"b":2,"a":1}', '{"a":1,"a":2}', '"\\u0000"', '"\\ud800"',
		'"\\udc00"', '"\\ud83d\\ude00"', '"é"', '"\\/"', '01', '+1', 'NaN',
		'Infinity', 'true false', '[1,]', '{"a":}', '{', '[', '"', '', ' ',
	]
	for text in json_texts:
		for wrap in ['', 'builtins.toJSON ', 'builtins.tryEval ']:
			add('json-boundaries', f'{wrap}(builtins.fromJSON {q(text)})')
		add('json-boundaries-trace', f'builtins.fromJSON {q(text)}', ['--show-trace'])

	toml_texts = [
		'', 'a = 1', 'a = -1', 'a = 9223372036854775807', 'a = 9223372036854775808',
		'a = -9223372036854775809', 'a = 1.0', 'a = inf', 'a = -inf', 'a = nan',
		'a = -nan', 'a = 1e309', 'a = 1e-400', 'a = "é"', 'a = "\\u0000"',
		'a = "\\ud800"', 'a = [1, "x", true]', 'a = { b = 1 }', '[a]\nb = 1',
		'a = 1\na = 2', '[a]\n[a]', 'a.b = 1\na = 2', 'a = 1979-05-27T07:32:00Z',
		'a = 1979-05-27', 'a = 07:32:00', 'a = [', 'a =', 'a = true false',
		'"a.b" = 1', 'a = 0xdead_beef', 'a = 0b1010', 'a = 0o755',
	]
	for text in toml_texts:
		for wrap in ['', 'builtins.toJSON ', 'builtins.tryEval ']:
			add('toml-boundaries', f'{wrap}(builtins.fromTOML {q(text)})')
		add('toml-boundaries-trace', f'builtins.fromTOML {q(text)}', ['--show-trace'])
	return cases
