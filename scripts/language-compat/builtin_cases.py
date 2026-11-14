def generate(root):
	bases = {
		'add': ['1', '2'], 'sub': ['4', '2'], 'mul': ['3', '2'], 'div': ['4', '2'],
		'bitAnd': ['3', '2'], 'bitOr': ['3', '2'], 'bitXor': ['3', '2'],
		'ceil': ['1.5'], 'floor': ['1.5'], 'lessThan': ['1', '2'],
		'compareVersions': ['"1.0"', '"1.1"'], 'parseDrvName': ['"hello-1.0"'],
		'splitVersion': ['"1.0pre2"'], 'stringLength': ['"hello"'],
		'addErrorContext': ['"outer"', '42'], 'abort': ['"stop"'], 'throw': ['"stop"'],
		'trace': ['"trace"', '42'], 'traceVerbose': ['"trace"', '42'], 'warn': ['"warn"', '42'],
		'attrNames': ['{ a = 1; }'], 'attrValues': ['{ a = 1; }'],
		'getAttr': ['"a"', '{ a = 1; }'], 'hasAttr': ['"a"', '{ a = 1; }'],
		'unsafeGetAttrPos': ['"a"', '{ a = 1; }'],
		'catAttrs': ['"a"', '[ { a = 1; } {} { a = 2; } ]'],
		'intersectAttrs': ['{ a = 0; }', '{ a = 1; b = 2; }'],
		'removeAttrs': ['{ a = 1; b = 2; }', '[ "a" ]'],
		'listToAttrs': ['[ { name = "a"; value = 1; } ]'],
		'all': ['(x: x > 0)', '[ 1 2 ]'], 'any': ['(x: x > 0)', '[ 0 1 ]'],
		'concatLists': ['[ [ 1 ] [ 2 ] ]'], 'concatMap': ['(x: [ x ])', '[ 1 2 ]'],
		'elem': ['1', '[ 1 2 ]'], 'elemAt': ['[ 1 2 ]', '0'],
		'filter': ['(x: x > 0)', '[ 0 1 ]'], 'foldl\'': ['(a: x: a + x)', '0', '[ 1 2 ]'],
		'functionArgs': ['({ x, y ? 1 }: x)'], 'genList': ['(x: x + 1)', '2'],
		'genericClosure': ['{ startSet = [ { key = 1; } ]; operator = _: []; }'],
		'head': ['[ 1 2 ]'], 'tail': ['[ 1 2 ]'], 'length': ['[ 1 2 ]'],
		'map': ['(x: x + 1)', '[ 1 2 ]'], 'mapAttrs': ['(n: v: v + 1)', '{ a = 1; }'],
		'partition': ['(x: x > 0)', '[ 0 1 ]'],
		'sort': ['(a: b: a < b)', '[ 2 1 ]'],
		'groupBy': ['(x: if x > 0 then "positive" else "other")', '[ 0 1 ]'],
		'zipAttrsWith': ['(n: values: values)', '[ { a = 1; } { a = 2; } ]'],
		'seq': ['1', '42'], 'deepSeq': ['{ a = [ 1 2 ]; }', '42'],
		'toString': ['42'], 'toJSON': ['{ a = 1; }'], 'toXML': ['{ a = 1; }'],
		'fromJSON': ['"{\\"a\\":1}"'], 'fromTOML': ['"a = 1"'],
		'hashString': ['"sha256"', '"abc"'],
		'concatStringsSep': ['","', '[ "a" "b" ]'],
		'substring': ['0', '1', '"abc"'],
		'replaceStrings': ['[ "a" ]', '[ "b" ]', '"abc"'],
		'tryEval': ['42'],
	}
	variants = ['null', 'true', '17', '1.25', '"text"', '[]', '{}', '(x: x)', '(throw "argument")']
	cases = []
	for name, args in bases.items():
		for trace in [False, True]:
			flags = ['--show-trace'] if trace else []
			cases.append({'expression': 'builtins.' + name + ' ' + ' '.join('(' + a + ')' for a in args), 'category': 'builtin-valid', 'flags': flags})
			for index in range(len(args)):
				for value in variants:
					current = args.copy()
					current[index] = value
					cases.append({'expression': 'builtins.' + name + ' ' + ' '.join('(' + a + ')' for a in current), 'category': 'builtin-type-and-force', 'flags': flags})
			if len(args) > 1:
				for index in range(len(args)):
					current = ['(throw "later")'] * len(args)
					current[index] = '{}'
					cases.append({'expression': 'builtins.' + name + ' ' + ' '.join('(' + a + ')' for a in current), 'category': 'builtin-forcing-order', 'flags': flags})

	for predicate in ['isAttrs', 'isBool', 'isFloat', 'isFunction', 'isInt', 'isList', 'isNull', 'isPath', 'isString', 'typeOf']:
		for value in variants:
			cases.append({'expression': f'builtins.{predicate} ({value})', 'category': 'type-predicates'})
	return cases
