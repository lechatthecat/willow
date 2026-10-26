#!/usr/bin/env python3
"""Focused separate-process cache acceptance and deterministic scaling checks."""
import json, os, pathlib, subprocess, tempfile, sys, hashlib
binary = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/willow').resolve()
rows = []
with tempfile.TemporaryDirectory(prefix='willow-persistent-') as directory:
    root = pathlib.Path(directory)
    source = root / 'main.wi'
    cache = root / 'cache'
    env = dict(os.environ, WILLOW_QUERY_CACHE_DIR=str(cache), WILLOW_QUERY_CACHE_STATS='1')
    def call(mode='check', cold=False, output=None):
        args = [str(binary), mode, str(source)]
        if output: args += ['-o', str(output)]
        result = subprocess.run(args, env=dict(env, WILLOW_QUERY_CACHE='0' if cold else '1'), capture_output=True, text=True)
        stats = None
        diagnostics = []
        for line in result.stderr.splitlines():
            if line.startswith('willow-query-cache {'): stats = json.loads(line.split(' ', 1)[1])
            elif line.startswith('willow-query-cache write skipped:'): raise AssertionError(line)
            else: diagnostics.append(line)
        return result, stats, '\n'.join(diagnostics)
    for count in [8, 32, 128]:
        text = '\n'.join(f'fn compute{i}() -> i64 {{ return {i}; }}' for i in range(count)) + '\nfn main() { print(compute0()); }\n'
        source.write_text(text)
        seed, _, _ = call(); assert seed.returncode == 0, seed.stderr
        unchanged, stats, diagnostics = call(); assert unchanged.returncode == 0, unchanged.stderr
        assert stats['typechecks'] == 0 and stats['reused_bodies'] == count + 1, stats
        source.write_text(text.replace('return 0;', 'return 999;'))
        warm, stats, diagnostics = call(); assert warm.returncode == 0, warm.stderr
        cold, _, cold_diagnostics = call(cold=True)
        assert warm.returncode == cold.returncode and diagnostics == cold_diagnostics
        assert stats['typechecks'] == 1 and stats['reused_bodies'] == count, stats
        rows.append(dict(shape='independent-bodies', bodies=count+1, **stats))
    for shape in ['chain', 'fanout']:
        for count in [4, 16, 64]:
            for i in range(count):
                prefix = f'import m{i-1}; ' if shape == 'chain' and i else ''
                value = f'm{i-1}::value() + {i}' if prefix else str(i)
                (root / f'm{i}.wi').write_text(prefix + f'pub fn value() -> i64 {{ return {value}; }}')
            imports = f'import m{count-1};' if shape == 'chain' else '\n'.join(f'import m{i};' for i in range(count))
            source.write_text(imports + f'\nfn main() {{ print(m{count-1}::value()); }}')
            seed, _, _ = call(); assert seed.returncode == 0, seed.stderr
            unchanged, stats, _ = call(); assert unchanged.returncode == 0, unchanged.stderr
            assert stats['typechecks'] == 0, stats
            module = root/'m0.wi'; module.write_text(module.read_text().replace('return 0;', 'return 999;'))
            warm, stats, diagnostics = call(); assert warm.returncode == 0, warm.stderr
            cold, _, cold_diagnostics = call(cold=True)
            assert cold.returncode == 0 and diagnostics == cold_diagnostics
            assert stats['typechecks'] == 1 and stats['reused_bodies'] == count, stats
            rows.append(dict(shape=shape, modules=count, cache_bytes=sum(p.stat().st_size for p in cache.glob('*.cache')), **stats))
    # Native linking is executed afresh; compare complete binary bytes.
    for native_shape, native_source in [
        ('simple', 'fn value() -> i64 { return 17; }\nfn main() { print(value()); }\n'),
        ('nested-lambda', 'fn value() -> i64 { return 17; }\nfn main() { let f = |x: i64| { let g = |y: i64| y + x; return g(2); }; print(f(value())); }\n'),
    ]:
        source.write_text(native_source)
        seed, _, _ = call(); assert seed.returncode == 0, seed.stderr
        source.write_text(source.read_text().replace('return 17', 'return 23'))
        warm_path, cold_path = root/'warm', root/'cold'
        warm, stats, _ = call('build', output=warm_path); assert warm.returncode == 0, warm.stderr
        cold, _, _ = call('build', cold=True, output=cold_path); assert cold.returncode == 0, cold.stderr
        assert warm_path.read_bytes() == cold_path.read_bytes(), f'{native_shape}: warm/cold native binaries differ'
        assert subprocess.check_output([str(warm_path)]) == subprocess.check_output([str(cold_path)])
        rows.append(dict(shape='native-'+native_shape, identical_binaries=True, **stats))
    for mutation in ['truncated', 'checksum', 'invalid-json']:
        path = next(cache.glob('*.cache'))
        data = path.read_bytes()
        if mutation == 'truncated': data = data[:70]
        elif mutation == 'checksum': data = b'0' + data[1:]
        else: data = hashlib.sha256(b'!').hexdigest().encode()+b'\n!'
        path.write_bytes(data)
        result, stats, diagnostics = call()
        assert result.returncode == 0 and not stats['loaded'], (mutation, result.stderr)
        rows.append(dict(shape=mutation, recovered=True, **stats))
    # Cached success must never conceal new errors; compare complete diagnostics.
    source.write_text('fn main() { let a: i64 = "wrong"; }\n')
    warm, _, diagnostics = call()
    cold, _, cold_diagnostics = call(cold=True)
    assert warm.returncode != 0 and warm.returncode == cold.returncode
    assert diagnostics == cold_diagnostics, (diagnostics, cold_diagnostics)
    rows.append(dict(shape='error-diagnostics', identical=True))
print(json.dumps(rows, indent=2))
