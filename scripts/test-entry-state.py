#!/usr/bin/env python3
"""Exercise explicit type generations on a freshly launched Apparition Lab.

Build first with --inject-backend apparition --inject --run --mcp.
This edits only Lab/EntryWorkflowState.swift and restores its source in finally.
The live app finishes with V3 data and its original boundary refusing that data.
Relaunch the app before repeating the experiment.
"""
import argparse
import json
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--device', default='AVP')
    args = parser.parse_args()
    repo = Path(__file__).resolve().parent.parent
    source = repo / 'Lab/EntryWorkflowState.swift'
    original = source.read_text()
    if 'struct EntryWorkflowStateV2' in original:
        raise SystemExit('Restore the baseline state source and rebuild before running.')

    def inject(snippet, expected=None, fixture=None):
        if fixture:
            source.write_text((repo / 'Lab/Patches' / fixture).read_text())
        result = subprocess.run(['apparition', 'inject', 'com.illixion.apparition.lab',
            '--device', args.device, '--file', 'Lab/EntryWorkflowState.swift', '--run', snippet],
            cwd=repo, capture_output=True, text=True)
        envelope = None
        for line in result.stdout.splitlines():
            try:
                envelope = json.loads(line)
            except ValueError:
                pass
        if result.returncode or not envelope or envelope.get('isError'):
            raise RuntimeError(result.stdout + result.stderr)
        report = json.loads(envelope['content'][0]['text'])
        if not report.get('ok'):
            raise RuntimeError(report)
        output = report.get('output')
        if expected is not None and output != expected:
            raise AssertionError(f'{fixture or "run"}: expected {expected!r}, got {output!r}')
        print(f'{fixture or "run"}: {output}', flush=True)
        return output

    try:
        start = int(inject('entryWorkflowState(0)'))
        if start < 0:
            raise RuntimeError('Relaunch the baseline lab before repeating the experiment.')
        total = start + 5
        inject('entryWorkflowState(5)', str(total))
        # Reject after constructing a candidate, before storing it: V1 survives.
        inject(r'let result = entryWorkflowState(-999); return "\(result):\((try? EntryWorkflowStateSlot.slot.read(generation: "v1", as: EntryWorkflowStateV1.self))?.total ?? -1):\(EntryWorkflowStateSlot.slot.revision)"',
               f'-2:{total}:2', 'StateV2.swift')
        total += 2
        inject(r'let result = entryWorkflowState(2); let state = try! EntryWorkflowStateSlot.slot.read(generation: "v2", as: EntryWorkflowStateV2.self); return "\(result):\(state.steps):\(state.label):\(EntryWorkflowStateSlot.slot.revision)"',
               f'{total}:1:migrated:3')
        total += 3
        inject(r'let result = entryWorkflowState(3); return "\(result):\((try! EntryWorkflowStateSlot.slot.read(generation: "v2", as: EntryWorkflowStateV2.self)).steps)"', f'{total}:2')
        total += 4
        inject(r'let result = entryWorkflowState(4); return "\(result):\((try! EntryWorkflowStateSlot.slot.read(generation: "v3", as: EntryWorkflowStateV3.self)).history)"',
               f'{total}:[2, 4]', 'StateV3.swift')
        inject(r'let result = entryWorkflowState(-999); let state = try! EntryWorkflowStateSlot.slot.read(generation: "v3", as: EntryWorkflowStateV3.self); return "\(result):\(state.total):\(state.history):\(EntryWorkflowStateSlot.slot.revision)"', f'-2:{total}:[2, 4]:5')
        total += 1
        inject(r'let result = entryWorkflowState(1); return "\(result):\((try! EntryWorkflowStateSlot.slot.read(generation: "v3", as: EntryWorkflowStateV3.self)).history)"', f'{total}:[2, 4, 1]')
        inject(r'let result = entryWorkflowState(10); return "\(result):\((try! EntryWorkflowStateSlot.slot.read(generation: "v3", as: EntryWorkflowStateV3.self)).total):\(EntryWorkflowStateSlot.slot.revision)"',
               f'-1:{total}:6', 'StateRollback.swift')
        print('PASS: two migrations, retained added state, rejected candidate, safe old-code refusal.', flush=True)
    finally:
        source.write_text(original)


if __name__ == '__main__':
    main()
