#!/usr/bin/env python3
"""Measure stable libtest executables without nightly or changing test caps.

Pass --binary name=executable for each Cargo test artifact. Each binary runs
normally once for wall time, then each test runs separately for attribution.
Use the same target selection, machine and options on both revisions. Compilation is
separate: cargo test --locked --no-run --timings <target selection>.
"""
import argparse
import json
import subprocess
import time


def run(command):
    started = time.monotonic()
    result = subprocess.run(command, capture_output=True, text=True)
    seconds = time.monotonic() - started
    if result.returncode:
        raise RuntimeError(result.stdout + result.stderr)
    return seconds, result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", action="append", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    report = []
    for spec in args.binary:
        name, executable = spec.split("=", 1)
        _, listing = run([executable, "--list"])
        names = [line.removesuffix(": test") for line in listing.splitlines()
                 if line.endswith(": test")]
        if not names:
            raise RuntimeError(f"{name}: no tests found")
        wall, _ = run([executable])
        print(f"{name}: binary wall {wall:.3f}s", flush=True)
        tests = []
        for test in names:
            seconds, output = run([executable, "--exact", test])
            if "1 passed;" not in output:
                raise RuntimeError(f"{name}::{test}: did not pass exactly one test")
            tests.append({"test": test, "seconds": round(seconds, 3)})
            print(f"{name}::{test}: {seconds:.3f}s", flush=True)
        report.append({"binary": name, "wall_seconds": round(wall, 3),
                       "tests": sorted(tests, key=lambda row: -row["seconds"])})
        # Preserve completed binaries if a later one fails.
        with open(args.output, "w") as output:
            json.dump(report, output, indent=2)
            output.write("\n")


if __name__ == "__main__":
    main()
