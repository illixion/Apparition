#!/usr/bin/env python3
"""Compare native-entry and implicit-dynamic build costs in an isolated host workload.

This is a compiler/CPU microbenchmark, not an application or device performance claim.
No app is installed, patched or attached to a debugger. Reports include every sample.
"""
import argparse
import json
from pathlib import Path
import platform
import random
import statistics
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SOURCE = r'''
import Foundation

func mix(_ value: UInt64) -> UInt64 {
    var x = value
    x ^= x >> 30
    x &*= 0xbf58476d1ce4e5b9
    x ^= x >> 27
    x &*= 0x94d049bb133111eb
    return x ^ (x >> 31)
}

func kernel(_ seed: UInt64, _ count: Int) -> UInt64 {
    var x = seed
    for i in 0..<count { x = mix(x &+ UInt64(i)) }
    return x
}

BOUNDARY_ATTRIBUTES
public func boundary(_ seed: UInt64, _ count: Int) -> UInt64 { kernel(seed, count) }

let arguments = CommandLine.arguments
let seed = UInt64(arguments[1])!
let outer = Int(arguments[2])!
let inner = Int(arguments[3])!
// Warm the code before timing. A checksum is observable, preventing dead-code removal.
var checksum = boundary(seed, inner)
let started = DispatchTime.now().uptimeNanoseconds
for i in 0..<outer { checksum ^= boundary(seed &+ UInt64(i), inner) }
let elapsed = DispatchTime.now().uptimeNanoseconds - started
print("\(elapsed) \(checksum)")
'''


def run(command):
    return subprocess.run([str(a) for a in command], check=True, capture_output=True, text=True).stdout.strip()


def benchmark(samples=9, rounds=10000, inner=1000):
    attributes = '@_silgen_name("debugtrace_entry_benchmark")\n@inline(never)\n@_optimize(none)'
    variants = {
        'normal': [],
        'dynamic': ['-enable-testing', '-Xfrontend', '-enable-private-imports', '-Xfrontend', '-enable-implicit-dynamic'],
        'apparition': ['-enable-testing', '-Xfrontend', '-enable-private-imports'],
    }
    samples_by_variant = {name: [] for name in variants}
    with tempfile.TemporaryDirectory(prefix='apparition-entry-benchmark-') as directory:
        base = Path(directory)
        for name, flags in variants.items():
            source = base / (name + '.swift')
            source.write_text(SOURCE.replace('BOUNDARY_ATTRIBUTES', attributes if name == 'apparition' else ''))
            run(['xcrun', 'swiftc', '-O', *flags, source, '-o', base / name])
        # Shuffle every repetition to avoid always giving one variant a cold/hot CPU.
        rng = random.Random(87341)
        for repetition in range(samples + 1):
            names = list(variants)
            rng.shuffle(names)
            checksum = None
            for name in names:
                elapsed, result = map(int, run([base / name, '123456789', str(rounds), str(inner)]).split())
                if checksum is not None and checksum != result:
                    raise RuntimeError('backend checksums differ; timings are invalid')
                checksum = result
                if repetition:
                    samples_by_variant[name].append(elapsed / (rounds * inner))
    results = {}
    normal = statistics.median(samples_by_variant['normal'])
    for name, values in samples_by_variant.items():
        median = statistics.median(values)
        results[name] = {'nsPerIteration': values, 'medianNs': median,
                         'minNs': min(values), 'maxNs': max(values),
                         'relativeToNormal': median / normal, 'swiftFlags': ['-O', *variants[name]]}
    return {'schema': 'apparition-entry-benchmark/1', 'scope': 'host microbenchmark; no debugger or patch publication',
            'os': platform.platform(), 'machine': platform.machine(), 'swift': run(['xcrun', 'swiftc', '--version']),
            'samples': samples, 'rounds': rounds, 'inner': inner, 'results': results}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--samples', type=int, default=9)
    parser.add_argument('--rounds', type=int, default=10000)
    parser.add_argument('--inner', type=int, default=1000)
    parser.add_argument('--output', type=Path, default=ROOT / 'build/entry-performance/host.json')
    args = parser.parse_args()
    if min(args.samples, args.rounds, args.inner) < 1:
        parser.error('samples, rounds and inner must be positive')
    report = benchmark(args.samples, args.rounds, args.inner)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + '\n')
    for name, result in report['results'].items():
        print(f"{name}: {result['medianNs']:.3f} ns/iteration; {result['relativeToNormal']:.3f}x normal")
    print(f'Report: {args.output}')


if __name__ == '__main__':
    main()
