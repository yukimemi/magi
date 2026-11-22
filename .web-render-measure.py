import concurrent.futures
import json
import os
import pathlib
import subprocess
import sys

TESTS = (
    'avatar_settings_preview_isolation_reload_and_reset_on_phone',
    'durable_chat_goals_stay_compact_safe_and_navigable',
)
output = pathlib.Path(sys.argv[1])
output.mkdir(exist_ok=True)
with open(sys.argv[2], encoding='utf-8') as manifest:
    exe = next(r['executable'] for r in map(json.loads, manifest)
               if r['reason'] == 'compiler-artifact' and r['target']['name'] == 'web_render')

def run(mode, i, test):
    result = subprocess.run([exe, test, '--exact', '--nocapture'],
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            env={**os.environ, 'CI': '1', 'RUSTUP_TOOLCHAIN': 'stable'})
    (output / f'{mode}-{i}-{test}.log').write_bytes(result.stdout)
    print(mode, i, test, result.returncode, flush=True)
    if result.returncode:
        print(result.stdout.decode(errors='replace'), flush=True)
    return result.returncode != 0

with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
    for mode in ('serial', 'parallel'):
        totals = dict.fromkeys(TESTS, 0)
        for i in range(1, 21):
            if mode == 'parallel':
                pending = [pool.submit(run, mode, i, test) for test in TESTS]
                results = [f.result() for f in pending]
            else:
                results = [run(mode, i, test) for test in TESTS]
            for test, failed in zip(TESTS, results):
                totals[test] += failed
        print(mode, {k: f'{v}/20' for k, v in totals.items()}, flush=True)
