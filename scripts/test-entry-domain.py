#!/usr/bin/env python3
"""Verify cross-file optimized callers on a freshly launched Apparition Lab.

Build with --inject --inject-backend apparition --run --mcp first.
Only the domain Leaf/Caller files and EntryWorkflowDomainOutside.swift are edited.
Source is restored in finally; a successful run also restores the live entries.
"""
import argparse
import json
from pathlib import Path
import re
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--device', default='AVP')
    args = parser.parse_args()
    repo = Path(__file__).resolve().parent.parent
    domain = Path('Lab/DebugTraceImplementationDomain')
    leaf, caller = repo/domain/'Leaf.swift', repo/domain/'Caller.swift'
    outside = repo/'Lab/EntryWorkflowDomainOutside.swift'
    original = {path: path.read_text() for path in (leaf, caller, outside)}
    pid = None
    probe = r'return "\(entryWorkflowDomainFirst(10, 0)):\(entryWorkflowDomainSecond(10, 0)):\(entryWorkflowDomainOutside(10, 1))"'

    def inject(files=(), run=probe, expected=None, refusal=None):
        nonlocal pid
        command = ['apparition', 'inject', 'com.illixion.apparition.lab', '--device', args.device]
        for file in files:
            command += ['--file', str(file)]
        if run is not None:
            command += ['--run', run]
        result = subprocess.run(command, cwd=repo, capture_output=True, text=True)
        match = re.search(r'\(pid (\d+)\)', result.stdout + result.stderr)
        if match:
            if pid is not None and pid != match[1]:
                raise AssertionError('The app relaunched during the experiment')
            pid = match[1]
        envelope = None
        for line in result.stdout.splitlines():
            try:
                envelope = json.loads(line)
            except ValueError:
                pass
        if not isinstance(envelope, dict):
            raise RuntimeError(result.stdout + result.stderr)
        text = envelope['content'][0]['text']
        if refusal:
            if not result.returncode or not envelope.get('isError') or refusal not in text:
                raise AssertionError(result.stdout + result.stderr)
            print(f'PASS refusal: {refusal}', flush=True)
            return
        if result.returncode or envelope.get('isError'):
            raise RuntimeError(result.stdout + result.stderr)
        report = json.loads(text)
        if not report.get('ok') or report.get('output') != expected:
            raise AssertionError(report)
        print(f'PASS output: {expected}', flush=True)
        return report

    try:
        inject(expected='31:131:11')
        leaf.write_text(original[leaf].replace('value * 3', 'value * 7'))
        report = inject([domain/'Leaf.swift'], expected='71:171:11')
        if len(report.get('recompiled', [])) != 2 or len(report['domainCoverage']['recompiledSources']) != 3:
            raise AssertionError('The leaf edit did not regenerate both entries and all three files')
        # An independent entry cannot start bypassing the domain's stable roots.
        outside.write_text(original[outside].replace('value + offset', 'DebugTraceImplementationDomainLeaf.leaf(value) + offset'))
        try:
            inject(['Lab/EntryWorkflowDomainOutside.swift'], run=None, refusal='candidate app module')
        finally:
            outside.write_text(original[outside])
        inject(run='return ApparitionLab.DebugTraceImplementationDomainLeaf.leaf(10)', refusal='snippet accesses implementation helpers')
        leaf.write_text(original[leaf].replace('leaf(_ value: Int)', 'leaf(_ value: Int, _ bias: Int)').replace('value * 3', '(value * 7) + bias'))
        caller.write_text(original[caller].replace('leaf(value)', 'leaf(value, 2)'))
        inject([domain/'Leaf.swift'], expected='73:173:11')
        leaf.write_text(original[leaf])
        caller.write_text(original[caller])
        inject([domain/'Leaf.swift'], expected='31:131:11')
        if pid is None:
            raise AssertionError('Could not verify the live PID')
        print(f'PASS: cross-file invalidation, both refusals, signature change and restoration in pid {pid}.', flush=True)
    finally:
        for path, source in original.items():
            path.write_text(source)


if __name__ == '__main__':
    main()
