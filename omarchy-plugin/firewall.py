#!/usr/bin/env python3
"""Check OmaBeam's default sharing ports against UFW's incoming user rules."""
import argparse
import ipaddress
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
from dataclasses import dataclass

PORTS = ((9847, 'tcp', 'browser / JPEG'), (9848, 'udp', 'WebRTC'))


def subnet(value):
    try:
        if '/' not in value:
            raise ValueError('include a CIDR prefix, for example 192.168.1.0/24')
        network = ipaddress.ip_network(value, strict=True)
        if network.prefixlen == 0 or network.is_multicast or network.is_loopback:
            raise ValueError('choose a specific viewer subnet, not every address, multicast, or loopback')
        return network
    except ValueError as error:
        raise argparse.ArgumentTypeError(str(error)) from error


def run(args):
    try:
        return subprocess.run(args, capture_output=True, text=True, timeout=10,
                              env={**os.environ, 'LC_ALL': 'C'})
    except (OSError, subprocess.TimeoutExpired) as error:
        return subprocess.CompletedProcess(args, 1, '', str(error))


def admin(args, authenticate=False):
    if os.geteuid() == 0:
        return run(args)
    if not shutil.which('sudo'):
        return subprocess.CompletedProcess(args, 1, '', 'sudo is unavailable')
    result = run(['sudo', '-n', *args])
    if result.returncode and authenticate and sys.stdin.isatty():
        print('Administrator access is needed to inspect or update the firewall.', flush=True)
        # Let sudo display its password prompt directly, then capture only the
        # firewall command's output. The installer itself stays unprivileged.
        if subprocess.run(['sudo', '-v'], check=False).returncode == 0:
            result = run(['sudo', '-n', *args])
    return result


def local_networks():
    """Default-route interfaces only; never automatically open inferred networks."""
    try:
        routes = run(['ip', '-j', '-4', 'route', 'show', 'default'])
        addresses = run(['ip', '-j', '-4', 'address', 'show', 'up'])
        if routes.returncode or addresses.returncode:
            return []
        devices = {route['dev'] for route in json.loads(routes.stdout) if 'dev' in route}
        result = []
        for interface in json.loads(addresses.stdout):
            if interface.get('ifname') not in devices:
                continue
            for address in interface.get('addr_info', []):
                if address.get('family') == 'inet' and address.get('scope') == 'global':
                    value = ipaddress.ip_interface(f"{address['local']}/{address['prefixlen']}")
                    if not value.ip.is_loopback and 0 < value.network.prefixlen < 32:
                        result.append((value.network, interface['ifname']))
        return list(dict.fromkeys(result))
    except (ValueError, KeyError, TypeError):
        return []


@dataclass
class Rule:
    destination: str
    action: str
    source: str


def parse_status(text):
    if re.search(r'^Status: inactive$', text, re.M):
        return 'inactive', None, []
    if not re.search(r'^Status: active$', text, re.M):
        raise ValueError('UFW did not report an active/inactive status')
    default = re.search(r'^Default: (allow|deny|reject) \(incoming\)', text, re.M)
    rules = []
    table = False
    for line in text.splitlines():
        if re.match(r'^--\s+--', line):
            table = True
            continue
        if not table or not line.strip():
            continue
        line = line.split(' #', 1)[0].strip()
        fields = re.split(r'\s{2,}', line)
        if len(fields) != 3 or fields[1] not in ('ALLOW IN', 'DENY IN', 'REJECT IN', 'LIMIT IN', 'ALLOW OUT', 'DENY OUT', 'REJECT OUT', 'LIMIT OUT', 'ALLOW FWD', 'DENY FWD', 'REJECT FWD', 'LIMIT FWD'):
            raise ValueError('unrecognized UFW rule output')
        rules.append(Rule(*fields))
    return 'active', default.group(1) if default else None, rules


def rule_matches(rule, port, protocol, network, interface):
    """Return none / full / partial / unknown, preserving first-match order."""
    if not rule.action.endswith(' IN'):
        return 'none'
    destination, source = rule.destination, rule.source
    ipv6 = '(v6)' in destination or '(v6)' in source
    if ipv6 != (network.version == 6):
        return 'none'
    destination = destination.replace(' (v6)', '')
    source = source.replace(' (v6)', '')
    scoped = False
    if ' on ' in destination:
        destination, device = destination.rsplit(' on ', 1)
        if interface is not None and not (device == interface or device.endswith('+') and interface.startswith(device[:-1])):
            return 'none'
        scoped = interface is None
    # UFW also reports profiles and destination-address rules; avoid declaring
    # them open without proving their match. Numeric unrelated ports are safe
    # to discard before considering source or interface restrictions.
    if destination == 'Anywhere':
        port_matches = True
    elif re.fullmatch(r'[\d,:]+(?:/(?:tcp|udp))?', destination):
        spec, _, proto = destination.partition('/')
        if proto and proto != protocol:
            return 'none'
        spans = [part.split(':') for part in spec.split(',')]
        if any(len(span) > 2 for span in spans):
            return 'unknown'
        port_matches = any(int(span[0]) <= port <= int(span[-1]) for span in spans)
    else:
        return 'unknown'
    if not port_matches:
        return 'none'
    try:
        origin = ipaddress.ip_network(source) if source != 'Anywhere' else ipaddress.ip_network('0.0.0.0/0' if network.version == 4 else '::/0')
    except ValueError:
        return 'unknown'
    if origin.version != network.version or not origin.overlaps(network):
        return 'none'
    if scoped:
        return 'unknown'
    return 'full' if network.subnet_of(origin) else 'partial'


def assess(rules, default, port, protocol, network, interface):
    for rule in rules:
        match = rule_matches(rule, port, protocol, network, interface)
        if match == 'none':
            continue
        if match == 'full':
            return 'allowed' if rule.action == 'ALLOW IN' else 'blocked'
        # Partial coverage can include earlier denies. Do not mistake a later
        # broad allow for access to the entire requested subnet.
        return 'unknown'
    return {'allow': 'allowed', 'deny': 'blocked', 'reject': 'blocked'}.get(default, 'unknown')


def check(networks, text):
    state, default, rules = parse_status(text)
    if state == 'inactive':
        print('  UFW is inactive; it is not filtering these ports.')
        return state, []
    results = []
    for network, interface in networks:
        print(f'  Viewer subnet: {network}' + (f' via {interface}' if interface else ''))
        for port, protocol, label in PORTS:
            status = assess(rules, default, port, protocol, network, interface)
            print(f'    {protocol.upper()} {port} ({label}): {status.upper()} by UFW user rules')
            results.append((network, port, protocol, status))
    return state, results


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--subnet', type=subnet)
    parser.add_argument('--open-firewall', type=subnet, metavar='CIDR')
    parser.add_argument('--validate-only', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--authenticate', action='store_true', help='allow sudo authentication in an interactive terminal')
    args = parser.parse_args(argv)
    if args.subnet and args.open_firewall and args.subnet != args.open_firewall:
        parser.error('--subnet and --open-firewall must name the same network')
    if args.validate_only:
        return 0
    print('==> checking sharing ports: TCP 9847 (HTTP) and UDP 9848 (WebRTC)')
    print('  This checks UFW user rules. Custom firewall rules and network equipment can still block viewers.')
    selected = args.open_firewall or args.subnet
    discovered = local_networks()
    if selected:
        interfaces = {device for network, device in discovered if network.version == selected.version and selected.subnet_of(network)}
        networks = [(selected, next(iter(interfaces)) if len(interfaces) == 1 else None)]
    else:
        networks = discovered
    if not networks:
        print('WARNING: Could not determine the viewer subnet. Use ./install.sh --check-ports --subnet CIDR.')
        return 1
    ufw = shutil.which('ufw')
    if not ufw:
        print('WARNING: UFW was not found. Check TCP 9847 and UDP 9848 in your firewall; automatic opening supports UFW.')
        return 1
    if shutil.which('firewall-cmd'):
        firewalld = run(['firewall-cmd', '--state'])
        if (firewalld.stdout + firewalld.stderr).strip() != 'not running':
            print('WARNING: firewalld is active or could not be inspected. Review its rules for TCP 9847 and UDP 9848; no UFW rules changed.')
            return 1
    result = admin([ufw, 'status', 'verbose'], args.authenticate or bool(args.open_firewall))
    if result.returncode:
        print('WARNING: Could not inspect UFW. Run ./install.sh --check-ports in a terminal with sudo access.')
        return 1
    try:
        state, results = check(networks, result.stdout)
    except ValueError as error:
        print(f'WARNING: {error}; no firewall rules changed.')
        return 1
    if state == 'inactive':
        if args.open_firewall:
            print('WARNING: UFW is inactive; no rules changed. This installer does not enable a firewall.')
            return 1
        return 0
    missing = [item for item in results if item[3] != 'allowed']
    if missing and args.open_firewall:
        for network, port, protocol, _ in missing:
            command = [ufw, 'prepend', 'allow', 'in', 'proto', protocol, 'from', str(network), 'to', 'any', 'port', str(port), 'comment', f'OmaBeam {protocol.upper()} {port}']
            print('  Opening: ' + shlex.join(command), flush=True)
            changed = admin(command, True)
            if changed.returncode:
                print(f'WARNING: Could not open {protocol.upper()} {port}: {changed.stderr.strip() or changed.stdout.strip()}')
                print('  Earlier successful rules remain. Rerun the command to finish setup.')
                return 1
        verified = admin([ufw, 'status', 'verbose'])
        try:
            if verified.returncode:
                raise ValueError('could not read UFW after the update')
            state, results = check(networks, verified.stdout)
            if state != 'active' or any(item[3] != 'allowed' for item in results):
                raise ValueError('the requested access is still blocked or unverified; inspect UFW rule ordering and profiles')
        except ValueError as error:
            print(f'WARNING: Firewall changes could not be verified: {error}.')
            return 1
        print('  UFW allows both ports for the selected subnet; the rules persist across restart.')
    elif missing:
        print('WARNING: LAN viewing is blocked or unverified for the ports above.')
        for network, _ in networks:
            print(f'  To allow this viewer subnet: ./install.sh --check-ports --open-firewall {network}')
        return 1
    print('  Start a share and open its link from another device to verify LAN reachability.')
    print('  Custom --port / --webrtc-port values need corresponding firewall rules.')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
