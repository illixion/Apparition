# SPDX-License-Identifier: MIT
"""Exercise late upgrade through a real debugserver on macOS, without self-writing code."""
import lldb
import struct

def check(debugger, command, result, _):
    target = debugger.CreateTarget(command.strip())
    target.BreakpointCreateByName('ap_fixture_ready')
    process = target.LaunchSimple(None, ['AP_FULL_TEST=1'], None)
    if process.GetState() != lldb.eStateStopped:
        raise RuntimeError('fixture did not stop before late attach')
    control = target.FindFirstGlobalVariable('APHookControl').GetLoadAddress()
    if control == lldb.LLDB_INVALID_ADDRESS:
        raise RuntimeError('missing control mailbox')
    error = lldb.SBError()
    def read(address, size):
        data = process.ReadMemory(address, size, error)
        if error.Fail() or len(data) != size:
            raise RuntimeError('debugger read failed: ' + str(error))
        return data
    def write(address, data):
        if process.WriteMemory(address, data, error) != len(data) or error.Fail():
            raise RuntimeError('debugger write failed: ' + str(error))
    write(control + 16, struct.pack('<Q', 0x4150484b))
    target.DeleteAllBreakpoints()
    operations = []
    for _ in range(128):
        process.Continue()
        if process.GetState() == lldb.eStateExited:
            if process.GetExitStatus() != 0:
                raise RuntimeError('fixture failed: ' + process.GetSTDERR(32768))
            if not {1, 0x4150, 0x4151, 0x4152, 0x4154}.issubset(operations):
                raise RuntimeError('fixture missed debugger operations: ' + str(operations))
            result.AppendMessage('PASS late attach, direct entry redirect, stable original and detach; ' + process.GetSTDOUT(32768))
            return
        thread = process.GetSelectedThread()
        for candidate in process:
            if candidate.GetStopReason() not in (lldb.eStopReasonNone, lldb.eStopReasonInvalid):
                thread = candidate
                break
        frame = thread.GetFrameAtIndex(0)
        reg = lambda name: frame.FindRegister(name).GetValueAsUnsigned()
        pc, op = reg('pc'), reg('x16')
        # The fixture must stop on our BRK, never on an unrelated exception.
        if read(pc, 4) != struct.pack('<I', 0xd4200000 | (0xf00d << 5)):
            raise RuntimeError('unexpected stop: ' + str(thread) + ' operations=' + str(operations) + ' control=' + repr(read(control, 40)))
        a, b, c = reg('x0'), reg('x1'), reg('x2')
        operations.append(op)
        answer = 0
        if op == 1:
            answer = process.AllocateMemory(b, lldb.ePermissionsReadable | lldb.ePermissionsExecutable, error)
            if error.Fail(): raise RuntimeError(str(error))
        elif op == 0x4150:
            write(a, read(b, c))
        elif op == 0x4151:
            for offset in range(b):
                data = read(a + offset*40, 40)
                address = struct.unpack('<Q', data[:8])[0]
                if read(address, 16) != data[8:24]: raise RuntimeError('stale entry')
                write(address, data[24:40])
        elif op == 0x4152:
            answer = 0x4150484b
        elif op == 0x4154:
            write(control + 16, bytes(8))
        else:
            raise RuntimeError('unexpected operation ' + hex(op))
        frame.FindRegister('x0').SetValueFromCString(str(answer))
        frame.FindRegister('pc').SetValueFromCString(str(pc + 4))
        if op == 0x4154:
            # Continue once to collect assertions and process exit; physical
            # detach is covered by the Rust server path/device fixture.
            pass
    raise RuntimeError('debugger request budget exceeded')

def __lldb_init_module(debugger, _):
    debugger.HandleCommand('command script add -f debugger.check aphook-check')
