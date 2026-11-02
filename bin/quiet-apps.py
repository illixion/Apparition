"""List a device's apps over pymobiledevice3's userspace tunnel. See `quiet-apps` for why.

Nothing here goes through CoreDevice: the tunnel is built in this process on top of the
device's RemotePairing service with pymobiledevice3's own pairing record, so Apple's remoted
never registers the device's services and DeviceHub never connects to its UniversalHID
service. That connection's later drop is what wakes a sleeping Apple TV.
"""

import argparse
import asyncio
import json
import logging
import sys

from pymobiledevice3.bonjour import browse_remotepairing, iter_browse_remotepairing
from pymobiledevice3.pair_records import iter_remote_pair_records_by_identifier
from pymobiledevice3.remote import tunnel_service, userspace_tunnel
from pymobiledevice3.remote.tunnel_service import PEER_ALT_IRK_KEY, _match_remote_pair_record
from pymobiledevice3.services.installation_proxy import InstallationProxyService


def fail(msg, code=1):
    print(json.dumps({"error": msg}), file=sys.stdout)
    print(f"quiet-apps: {msg}", file=sys.stderr)
    sys.exit(code)


def pair_records():
    """{identifier: altIRK} for every record that can be matched to a Bonjour advert."""
    irks = {}
    for identifier, _path, rec in iter_remote_pair_records_by_identifier():
        irk = rec.get(PEER_ALT_IRK_KEY)
        if irk is not None:
            irks[identifier] = irk
    return irks


async def browse(timeout):
    irks = pair_records()
    out = []
    for a in await browse_remotepairing(timeout=timeout):
        out.append({
            "identifier": a.properties.get("identifier"),
            "addresses": [x.full_ip for x in a.addresses],
            "port": a.port,
            # The record identifier is the stable name pymobiledevice3 saved at pairing time; the
            # TXT `identifier` rotates. Adverts are matched to records by their altIRK.
            "record": _match_remote_pair_record(a, irks),
        })
    print(json.dumps({"adverts": out}, indent=1))


async def find_advert(record_id, timeout):
    """The advert that matches pairing record `record_id`, stopping the browse as soon as it answers."""
    irks = pair_records()
    if record_id not in irks:
        fail(f"no pymobiledevice3 pairing record {record_id}; pair once with `pymobiledevice3 remote pair` "
             "while the TV shows Settings > Remotes and Devices > Remote App and Devices, then use the "
             "identifier it prints (see --browse)", 78)
    mine = {record_id: irks[record_id]}
    answers = iter_browse_remotepairing(timeout=timeout)
    try:
        async for a in answers:
            if _match_remote_pair_record(a, mine) == record_id:
                return a
    finally:
        await answers.aclose()
    return None


async def connect_remotepairing(identifier, advert):
    """A verified RemotePairing session with the device, from the saved record only (never pairs)."""
    last = None
    # Prefer the routable addresses; link-local ones need a scope the stack may not have.
    addrs = sorted(advert.addresses, key=lambda x: x.full_ip.startswith("fe80"))
    for addr in addrs:
        svc = tunnel_service.RemotePairingTunnelService(identifier, addr.full_ip, advert.port)
        try:
            await svc.connect(autopair=False)
            if svc.encryption_key is None:
                await svc.close()
                fail(f"pairing record for {identifier} was not accepted by the device; delete it and pair again", 78)
            return svc
        except Exception as e:  # try the next address
            last = e
            try:
                await svc.close()
            except Exception:
                pass
    fail(f"could not reach the RemotePairing service of {identifier}: {last!r}", 69)


async def list_apps(identifier, timeout, all_apps):
    advert = await find_advert(identifier, timeout)
    if advert is None:
        fail(f"device {identifier} is not advertising RemotePairing on this network (off, or not on the LAN)", 69)
    svc = await connect_remotepairing(identifier, advert)

    # UserspaceRsdTunnel picks its provider through this module-level hook, which starts from
    # usbmux (USB). Hand it the Wi-Fi RemotePairing session instead.
    async def provider(_serial, _autopair, _fallback=True):
        return svc, None
    userspace_tunnel._create_no_root_tunnel_provider = provider

    async with userspace_tunnel.UserspaceRsdTunnel(serial=identifier, autopair=False) as rsd:
        apps = await InstallationProxyService(lockdown=rsd).get_apps(application_type="Any" if all_apps else "User")
    result = []
    for bundle_id, info in sorted(apps.items()):
        result.append({
            "bundleIdentifier": bundle_id,
            "name": info.get("CFBundleDisplayName") or info.get("CFBundleName") or bundle_id,
            "version": info.get("CFBundleShortVersionString"),
            "bundleVersion": info.get("CFBundleVersion"),
        })
    print(json.dumps({"result": {"apps": result}}, indent=1))


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--identifier", help="the device's RemotePairing identifier (see --browse)")
    p.add_argument("--browse", action="store_true", help="list RemotePairing adverts and which are paired")
    p.add_argument("--timeout", type=float, default=20.0, help="Bonjour wait in seconds")
    p.add_argument("--all", action="store_true", help="system apps too (default: user apps)")
    p.add_argument("-v", "--verbose", action="store_true")
    args = p.parse_args()
    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.WARNING, stream=sys.stderr)
    if args.browse:
        asyncio.run(browse(args.timeout))
    elif args.identifier:
        asyncio.run(list_apps(args.identifier, args.timeout, args.all))
    else:
        p.error("--identifier or --browse is required")


if __name__ == "__main__":
    main()
