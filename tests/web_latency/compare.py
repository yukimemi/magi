#!/usr/bin/env python3
"""Build real baseline/candidate routers and embedded UIs with one CDP driver.

Uses temporary source archives, never changes a checkout, branch, or worktree.
Cargo uses the caller's shared target directory. Output is JSONL plus build logs.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--baseline', default='origin/main')
    parser.add_argument('--candidate', default='HEAD')
    parser.add_argument('--records', type=int, default=1000)
    parser.add_argument('--rounds', type=int, default=5)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    # Do not pay for builds if the host cannot run a real HTTP server.
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
    repo = Path(__file__).resolve().parents[2]
    env = dict(os.environ, RUSTUP_TOOLCHAIN='stable', MAGI_CONFIG_DIR='',
               MAGI_BENCH_RECORDS=str(args.records), MAGI_BENCH_ROUNDS=str(args.rounds))
    if not env.get('CARGO_TARGET_DIR'):
        raise RuntimeError('CARGO_TARGET_DIR must name the shared build cache')
    args.output.mkdir(parents=True, exist_ok=True)
    refs = {name: subprocess.check_output(['git', 'rev-parse', ref], cwd=repo, text=True).strip()
            for name, ref in [('baseline', args.baseline), ('candidate', args.candidate)]}
    (args.output / 'revisions.json').write_text(json.dumps(refs, indent=2) + '\n')
    driver = repo / 'tests/web_browser_latency.rs'
    cdp = repo / 'tests/web_render/cdp.rs'
    for label, revision in refs.items():
        with tempfile.TemporaryDirectory(prefix=f'magi-browser-{label}-') as folder:
            snapshot = Path(folder)
            archive = subprocess.check_output(['git', 'archive', revision], cwd=repo)
            subprocess.run(['tar', '-x', '-C', folder], input=archive, check=True)
            shutil.copyfile(driver, snapshot / 'tests/web_browser_latency.rs')
            shutil.copyfile(cdp, snapshot / 'tests/web_render/cdp.rs')
            with (args.output / f'{label}.log').open('w') as log:
                subprocess.run(['cargo', 'test', '--locked', '--test', 'web_browser_latency',
                                'browser_history_latency', '--', '--ignored', '--nocapture'],
                               cwd=snapshot, env=env, stdout=log, stderr=subprocess.STDOUT, check=True)
            lines = (args.output / f'{label}.log').read_text().splitlines()
            results = [dict(kind=line.split(' ', 1)[0], **json.loads(line.split(' ', 1)[1]))
                       for line in lines if line.startswith(('BROWSER_LATENCY ', 'SSE_LOAD '))]
            (args.output / f'{label}.jsonl').write_text(''.join(json.dumps(row) + '\n' for row in results))
            if not results:
                raise RuntimeError(f'{label}: no measurements recorded')
    for label in refs:
        rows = [json.loads(line) for line in (args.output / f'{label}.jsonl').read_text().splitlines()]
        groups = {}
        for row in rows:
            if row['kind'] == 'SSE_LOAD':
                print(label, json.dumps(row))
                continue
            groups.setdefault((row['route'], row['clients']), []).append(row['timing'])
        for (route, clients), samples in groups.items():
            def percentile(key, p):
                import math
                values = sorted(sample[key] for sample in samples)
                return round(values[math.ceil(len(values) * p) - 1], 2)
            print(json.dumps(dict(version=label, route=route, clients=clients, n=len(samples),
                                  first_p50=percentile('first_ms', .5), first_p95=percentile('first_ms', .95),
                                  complete_p50=percentile('complete_ms', .5), complete_p95=percentile('complete_ms', .95))))


if __name__ == '__main__':
    main()
