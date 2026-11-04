#!/usr/bin/env python3
"""Development macOS 26 workflow. No third-party Python dependencies."""
import argparse
import json
import os
from pathlib import Path
import plistlib
import socket
import shlex
import subprocess
import sys
import time
import threading
import re
import ipaddress
import hashlib
import signal

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
RUNNER = ROOT / 'build/macos-vm/vm'


def control(bundle, operation='status', **values):
    config = json.loads((bundle / 'control.json').read_text())
    with socket.create_connection(('127.0.0.1', config['port']), 2) as sock:
        sock.settimeout(60 if operation in ('screen','type','click-text') else 3)
        sock.sendall(json.dumps(dict(token=config['token'], operation=operation, **values)).encode() + b'\n')
        output = b''
        while b'\n' not in output:
            part = sock.recv(8192)
            if not part or len(output) > 16384:
                raise RuntimeError('invalid control response')
            output += part
    reply = json.loads(output)
    if not reply.get('ok'):
        raise RuntimeError(reply.get('error', 'VM control failed'))
    return reply


def live(bundle):
    try:
        return control(bundle)
    except (OSError, ValueError, RuntimeError):
        return None


def report(bundle):
    try:
        path=bundle/'guest-status/report.json'
        if path.stat().st_size > 8192: return None
        value = json.loads(path.read_text())
        boot = (bundle / 'guest-status/boot-id').read_text().strip()
        return value if value.get('boot') == boot else None
    except (OSError, ValueError):
        return None


def wait_for(check, timeout, description):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        result = check()
        if result:
            return result
        time.sleep(1)
    raise RuntimeError(f'Timed out waiting for {description}. Run status and inspect the VM; rerun setup to resume.')


def ensure_runner():
    if not RUNNER.exists() or not (RUNNER.parent/'guest-status').exists() or RUNNER.stat().st_mtime < max((HERE/name).stat().st_mtime for name in ['main.swift','guest-status.c','build.sh']):
        subprocess.run(['bash', str(HERE / 'build.sh')], check=True)


def start(bundle, recovery=False, network=False, background=False):
    existing = live(bundle)
    if existing:
        raise RuntimeError('VM is already running; use shutdown before changing boot mode')
    ensure_runner()
    logs = bundle / 'logs'
    logs.mkdir(exist_ok=True)
    # Keep the boot log, but never log control tokens, input text or credentials.
    log = open(logs / ('recovery.log' if recovery else 'boot.log'), 'ab', buffering=0)
    command = [str(RUNNER), 'setup' if recovery else 'run', str(bundle)]
    if not recovery:
        command += ['--network=on' if network else '--network=off']
    if background:
        command += ['--background']  # accessory app, window behind the others: never takes the keyboard
    process = subprocess.Popen(command, stdout=log, stderr=log, start_new_session=True)
    log.close()
    def ready():
        if process.poll() is not None:
            raise RuntimeError(f'Runner exited ({process.returncode}); see {logs}')
        state = live(bundle)
        return state if state and state.get('running') else None
    wait_for(ready, 90, 'the VM to start')
    return process


def shutdown(bundle):
    if live(bundle):
        control(bundle, 'shutdown')
        wait_for(lambda: not live(bundle), 120, 'guest shutdown')


def address(bundle):
    try:
        mac = (bundle / 'mac-address').read_text().strip().lower()
        normal = lambda text: ':'.join(f'{int(p,16):02x}' for p in text.split(':'))
        mac = normal(mac)
        leases = Path('/var/db/dhcpd_leases').read_text()
        for block in leases.split('}'):
            values = dict(line.strip().split('=', 1) for line in block.splitlines() if '=' in line)
            value = values.get('hw_address', '').split(',', 1)[-1]
            if value and normal(value) == mac:
                return values.get('ip_address')
    except (OSError, ValueError):
        pass
    return None


def tcp_ready(ip, port):
    try:
        with socket.create_connection((ip, port), 2):
            return True
    except OSError:
        return False


def state(bundle):
    return dict(bundle=str(bundle), installed=(bundle/'installed').exists(),
                provisioned=(bundle/'prepared').exists(), control=live(bundle), guest=report(bundle),
                address=address(bundle), credentials=str(bundle/'credentials.txt'))


def payload_hash():
    digest=hashlib.sha256()
    for path in [HERE/'provision.sh', HERE/'first-boot.sh', HERE/'report-status.sh', HERE/'appliance-trim.sh',
                 HERE/'appliance-disabled-user.txt', HERE/'appliance-disabled-system.txt', RUNNER.parent/'guest-status']:
        digest.update(path.read_bytes())
    return digest.hexdigest()


def prepare(args):
    bundle = args.bundle
    if not args.public_key.is_file():
        raise RuntimeError("Authorized public key file is missing")
    ensure_runner()
    if not bundle.exists():
        subprocess.run([str(RUNNER), 'create', str(args.ipsw), str(bundle)], check=True)
    if not (bundle/'restore-build').exists():
        raise RuntimeError('Unmanaged existing bundle: refusing to install over it')
    if not (bundle/'installed').exists():
        subprocess.run([str(RUNNER), 'install', str(args.ipsw), str(bundle)], check=True)
    if not (bundle/'prepared').exists() or (bundle/'prepared').read_text().strip() != payload_hash():
        shutdown(bundle)
        command = shlex.join(['/bin/bash', str(HERE/'provision.sh'), str(bundle), str(args.public_key)])
        subprocess.run(['osascript', '-e', 'do shell script '+json.dumps(command,ensure_ascii=False)+' with administrator privileges'], check=True)
        (bundle/'prepared').write_text(payload_hash())
    print(f'Account: apparition. Password: open {bundle / "credentials.txt"}', flush=True)


def verify_helper(companion):
    helper = companion.parent/'apparition-anisette-helper'
    result = subprocess.run(['/usr/bin/codesign','-d','--entitlements',':-',str(helper)], capture_output=True)
    if result.returncode:
        raise RuntimeError('The companion needs its original entitled helper beside it')
    try:
        values = plistlib.loads(result.stdout)
    except (ValueError, plistlib.InvalidFileException):
        raise RuntimeError('Could not read helper entitlements')
    if not all(values.get(key) is True for key in ['com.apple.authkit.client.private',
              'com.apple.authkit.client.internal','com.apple.private.alloy.anisette']):
        raise RuntimeError('Helper lacks required entitlements; use an existing correctly signed build')


def gateway(bundle):
    ip = address(bundle)
    if not ip:
        raise RuntimeError('No DHCP lease for this VM yet')
    output = subprocess.check_output(['/sbin/ifconfig'], text=True)
    for interface in output.split('\n'):
        match = re.search(r'inet (\d+\.\d+\.\d+\.\d+) netmask (0x[0-9a-f]+)', interface)
        if match:
            host, mask = match.groups()
            network = ipaddress.IPv4Network((host,str(ipaddress.IPv4Address(int(mask,16)))),strict=False)
            if ipaddress.IPv4Address(ip) in network:
                return host
    raise RuntimeError('No host interface matches the VM lease')


def open_terminal(bundle, attempt):
    """Utilities > Terminal: the shortcut first, then the menu by its text if the shortcut didn't take."""
    if attempt < 3:
        control(bundle,'key',key='cmd+shift+t')
    else:
        if attempt == 3: print('Cmd+Shift+T did not open Terminal; using the Utilities menu.',flush=True)
        control(bundle,'click-text',text='Utilities',region='menubar')
        time.sleep(1)
        control(bundle,'click-text',text='Terminal')
    time.sleep(3)


def recovery_input(bundle, port, completed):
    sent_command = consented = password_sent = False
    terminal_attempts = 0
    end = time.monotonic() + 900
    print('Driving only this VM window through the runner. Allow its own-window screen capture if macOS asks.',flush=True)
    while time.monotonic() < end and not completed.is_set():
        if not live(bundle):
            return
        lower = control(bundle,'screen').get('text','').lower()
        if 'language' in lower and 'english' in lower:
            control(bundle,'key',key='return')
        elif 'options' in lower and 'restart' in lower and 'shut down' in lower:
            # Startup picker. Options is the last icon, so Right twice always lands on it.
            for _ in range(2):
                control(bundle,'key',key='right'); time.sleep(0.4)
            control(bundle,'key',key='return')
            time.sleep(10)
        elif 'restore from time machine' in lower and 'disk utility' in lower:
            open_terminal(bundle, terminal_attempts)
            terminal_attempts += 1
        elif 'password for user' in lower and consented and not password_sent:
            # Never print the password or the captured terminal text.
            lines=(bundle/'credentials.txt').read_text().splitlines()
            password=next(line.removeprefix('Password: ') for line in lines if line.startswith('Password: '))
            control(bundle,'type',text=password)
            control(bundle,'key',key='return')
            password_sent=True
        elif 'booting unsigned' in lower and not consented:
            control(bundle,'type',text='y\n')
            consented=True
        elif 'bash' in lower and not sent_command:
            command=f'curl -fsS {gateway(bundle)}:{port}/setup.sh -o /tmp/s.sh && bash /tmp/s.sh\n'
            control(bundle,'type',text=command)
            sent_command=True
        elif 'error:' in lower or 'incorrect password' in lower or 'unknown user' in lower:
            raise RuntimeError('Recovery displayed an error; inspect its window. No further input sent.')
        time.sleep(1)
    if not completed.is_set():
        raise RuntimeError('Recovery did not confirm installation before timeout')


def recover(args, resume=False):
    verify_helper(args.companion)
    if not resume:
        shutdown(args.bundle)
    env = dict(os.environ, APPARITION_VM_AUTOMATED='1')
    # Server output contains only addresses/progress. The password stays in credentials.txt.
    server = subprocess.Popen([str(args.companion), 'vm-setup'], env=env, stdout=subprocess.PIPE, text=True, bufsize=1)
    ports=[]
    completed=threading.Event()
    def output():
        for line in server.stdout:
            print(line,end='',flush=True)
            match=re.search(r'curl -s [0-9.]+:(\d+)',line)
            if match: ports.append(int(match.group(1)))
            if 'VM confirmed installation' in line: completed.set()
    reader=threading.Thread(target=output,daemon=True)
    reader.start()
    try:
        if not resume: start(args.bundle, recovery=True, background=not args.manual_recovery)
        port=wait_for(lambda: ports[0] if ports else None,15,'setup server address')
        if args.manual_recovery:
            print('Select Options, language, then open Terminal (Cmd+Shift+T). Run the curl line above.',flush=True)
            print('Use apparition and credentials.txt if prompted.',flush=True)
        else:
            recovery_input(args.bundle,port,completed)
        result = server.wait(timeout=1000)
        reader.join(timeout=2)
        if result or not completed.is_set():
            raise RuntimeError('Recovery did not confirm successful installation')
        wait_for(lambda: not live(args.bundle), 120, 'recovery to shut down after installation')
    finally:
        if server.poll() is None:
            server.terminate()  # This exact process was started above.
            server.wait(timeout=10)


def setup(args):
    verify_helper(args.companion)
    prepare(args)
    if (live(args.bundle) or {}).get('recovery'):
        recover(args, resume=True)
    current = report(args.bundle)
    if not current and not live(args.bundle):
        start(args.bundle, background=True)
    owner = wait_for(lambda: (v if v and v.get('token') and v.get('owner') and v.get('provisioned') else None)
                     if (v := report(args.bundle)) else None, 300, 'the first account and volume owner')
    if not owner.get('sipDisabled'):
        recover(args)
    if not live(args.bundle):
        start(args.bundle, background=True)
    # First normal boot may set AMFI and reboot; use current-boot guest signals.
    def gui_ready():
        value = report(args.bundle)
        return value if live(args.bundle) and value and value.get('amfiActive') and value.get('consoleUID') == 501 and not value.get('setupAssistant') else None
    print('Waiting offline for Aqua readiness. Remaining setup screens, if any, must finish before networking is attached.', flush=True)
    wait_for(gui_ready, 300, 'offline GUI readiness')
    control(args.bundle, 'network-on')
    print('Network attached. Waiting for AuthKit and companion startup.', flush=True)
    wait_for(lambda: (v := report(args.bundle)) and v.get('relay'), 300, 'companion listener')
    ip = wait_for(lambda: address(args.bundle), 30, 'the VM DHCP address')
    if not tcp_ready(ip, 49160):
        raise RuntimeError(f'Guest reports a listener but host cannot reach {ip}:49160. Check the host route to the VM bridge and VPN configuration.')
    print(f'Companion ready: {ip}:49160. SSH: apparition@{ip}. Pairing and real-account sign-in must still be verified.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    for name in ['prepare','setup']:
        p=commands.add_parser(name)
        p.add_argument('ipsw', type=lambda p: Path(p).expanduser().resolve())
        p.add_argument('bundle', type=lambda p: Path(p).expanduser().resolve())
        p.add_argument('--public-key', type=lambda p: Path(p).expanduser().resolve(), required=True)
        if name == 'setup':
            p.add_argument('--manual-recovery',action='store_true',help='Use the keyboard yourself instead of own-window automation')
            p.add_argument('--companion', type=lambda p: Path(p).expanduser().resolve(), required=True)
    for name in ['status','run','recovery','shutdown','network','key','type','click','click-text','screen','force-stop','credentials']:
        p=commands.add_parser(name)
        p.add_argument('bundle', type=lambda p: Path(p).expanduser().resolve())
        if name in ['network','run']:
            p.add_argument('--network',choices=['off','on'],default='off',required=name=='network')
        if name == 'click':
            p.add_argument('x'); p.add_argument('y')
        if name == 'click-text':
            p.add_argument('text'); p.add_argument('--region',choices=['all','menubar'],default='all')
        if name in ['key','type']:
            p.add_argument('value', help='key chord, or path to a UTF-8 text file (never logged)')
        if name == 'recovery':
            p.add_argument('--manual-recovery',action='store_true')
            p.add_argument('--companion',type=lambda p:Path(p).expanduser().resolve(),required=True)
    args=parser.parse_args()
    if args.command == 'prepare': prepare(args)
    elif args.command == 'setup': setup(args)
    elif args.command == 'status': print(json.dumps(state(args.bundle),indent=2))
    elif args.command == 'run': start(args.bundle,network=args.network=='on')
    elif args.command == 'recovery': recover(args,resume=bool((live(args.bundle) or {}).get('recovery')))
    elif args.command == 'shutdown': shutdown(args.bundle)
    elif args.command == 'force-stop':
        try: control(args.bundle,'force-stop')
        except (OSError,ValueError): pass
        wait_for(lambda:not live(args.bundle),30,'forced VM stop')
    elif args.command == 'network': print(json.dumps(control(args.bundle,'network-'+args.network)))
    elif args.command == 'screen': print(json.dumps(control(args.bundle,'screen'),indent=2))
    elif args.command == 'click': control(args.bundle,'click',x=args.x,y=args.y)
    elif args.command == 'click-text': print(json.dumps(control(args.bundle,'click-text',text=args.text,region=args.region)))
    elif args.command == 'key': control(args.bundle,'key',key=args.value)
    elif args.command == 'type': control(args.bundle,'type',text=Path(args.value).read_text())
    elif args.command == 'credentials': print((args.bundle/'credentials.txt').read_text(),end='')


if __name__ == '__main__':
    def interrupted(*_): raise KeyboardInterrupt
    signal.signal(signal.SIGTERM,interrupted)
    try: main()
    except KeyboardInterrupt:
        print('Stopped. The VM is left available for status or resume.',file=sys.stderr)
        sys.exit(130)
    except (RuntimeError,OSError,subprocess.SubprocessError,ValueError) as error:
        # Errors must not include request payloads or credential contents.
        print(f'error: {error}',file=sys.stderr)
        sys.exit(1)
