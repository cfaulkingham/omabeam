#!/usr/bin/env python3
"""Installer firewall checks with simulated commands; never touches host rules."""
import argparse
import contextlib
import importlib.util
import io
import ipaddress
import json
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location('omabeam_firewall', ROOT / 'omarchy-plugin/firewall.py')
FW = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = FW
SPEC.loader.exec_module(FW)
NETWORK = ipaddress.ip_network('192.168.1.0/24')
HEADER = 'Status: active\nLogging: on (low)\nDefault: deny (incoming), allow (outgoing), disabled (routed)\nNew profiles: skip\n\nTo                         Action      From\n--                         ------      ----\n'


def status(*rules, default='deny'):
    return HEADER.replace('deny (incoming)', f'{default} (incoming)') + ''.join(
        f'{destination:27}{action:12}{source}\n' for destination, action, source in rules)


def completed(output, code=0):
    return subprocess.CompletedProcess([], code, output, '')


def mock_firewall(tools):
    """Subprocess fixture shared with packaging tests, including persistent rules."""
    tools.mkdir(parents=True, exist_ok=True)
    state, log = tools.parent / 'ufw-state.json', tools.parent / 'ufw-calls.jsonl'
    state.write_text('[]')

    def executable(name, code):
        path = tools / name
        path.write_text(f'#!{sys.executable}\n' + code)
        path.chmod(0o755)

    executable('ufw', f'''
import json, sys
from pathlib import Path
state, log = Path({str(state)!r}), Path({str(log)!r})
args = sys.argv[1:]
with log.open('a') as stream: stream.write(json.dumps(args) + '\\n')
rules = json.loads(state.read_text())
if args == ['status', 'verbose']:
    print({HEADER!r}, end='')
    for network, port, protocol in rules:
        print(f'{{port}}/{{protocol}}  ALLOW IN  {{network}} # OmaBeam')
else:
    assert len(args) == 13, args
    assert args[:4] == ['prepend', 'allow', 'in', 'proto'], args
    assert args[4] in ('tcp', 'udp') and args[5] == 'from', args
    assert args[7:10] == ['to', 'any', 'port'] and args[11] == 'comment', args
    assert args[10] in ('9847', '9848'), args
    rule = [args[6], args[10], args[4]]
    if rule not in rules: rules.insert(0, rule)
    state.write_text(json.dumps(rules))
''')
    executable('sudo', f'''
import subprocess, sys
assert sys.argv[1] == '-n', sys.argv
assert sys.argv[2] == {str(tools / 'ufw')!r}, sys.argv
sys.exit(subprocess.run(sys.argv[2:]).returncode)
''')
    executable('ip', f'''
import json, sys
print(json.dumps([{{'dev': 'wlan0'}}] if 'route' in sys.argv else [
    {{'ifname': 'wlan0', 'addr_info': [{{'family': 'inet', 'scope': 'global', 'local': '192.168.1.42', 'prefixlen': 24}}]}}]))
''')
    executable('firewall-cmd', "import sys\nprint('not running', file=sys.stderr)\nsys.exit(252)\n")
    return state, log


class Firewall(unittest.TestCase):
    def assess(self, text, port=9847, protocol='tcp', network=NETWORK, interface='wlan0'):
        _, default, rules = FW.parse_status(text)
        return FW.assess(rules, default, port, protocol, network, interface)

    def invoke(self, args=(), outputs=None, which=None):
        with patch.object(FW, 'local_networks', return_value=[(NETWORK, 'wlan0')]), \
             patch.object(FW.shutil, 'which', side_effect=which or (lambda name: '/usr/sbin/ufw' if name == 'ufw' else None)), \
             patch.object(FW, 'admin', side_effect=outputs or [completed(status())]) as admin, \
             contextlib.redirect_stdout(io.StringIO()) as output:
            result = FW.main(list(args))
        return result, output.getvalue(), admin

    def test_default_policies_and_both_protocols(self):
        self.assertEqual(self.assess(status()), 'blocked')
        self.assertEqual(self.assess(status(default='allow')), 'allowed')
        text = status(('9847/tcp', 'ALLOW IN', '192.168.1.0/24'))
        self.assertEqual(self.assess(text), 'allowed')
        self.assertEqual(self.assess(text, 9848, 'udp'), 'blocked')
        self.assertEqual(self.assess(text, 9847, 'udp'), 'blocked')

    def test_rule_order_and_partial_subnets(self):
        deny = ('9847/tcp', 'DENY IN', '192.168.1.0/24')
        allow = ('9847/tcp', 'ALLOW IN', 'Anywhere')
        self.assertEqual(self.assess(status(deny, allow)), 'blocked')
        self.assertEqual(self.assess(status(allow, deny)), 'allowed')
        self.assertEqual(self.assess(status(('9847/tcp', 'DENY IN', '192.168.1.15'), allow)), 'unknown')
        self.assertEqual(self.assess(status(('9847/tcp', 'ALLOW IN', '192.168.1.0/25'))), 'unknown')
        self.assertEqual(self.assess(status(('9847/tcp', 'DENY IN', '10.0.0.0/8'), allow)), 'allowed')

    def test_interfaces_outbound_profiles_and_port_ranges(self):
        self.assertEqual(self.assess(status(('Anywhere on eth0', 'DENY IN', 'Anywhere'), default='allow'), interface='wlan0'), 'allowed')
        self.assertEqual(self.assess(status(('Anywhere on eth0', 'ALLOW IN', 'Anywhere'))), 'blocked')
        self.assertEqual(self.assess(status(('Anywhere on wlan+', 'DENY IN', 'Anywhere'), default='allow')), 'blocked')
        self.assertEqual(self.assess(status(('9847 on wlan0', 'ALLOW IN', 'Anywhere')), interface=None), 'unknown')
        self.assertEqual(self.assess(status(('9847', 'ALLOW OUT', 'Anywhere'))), 'blocked')
        self.assertEqual(self.assess(status(('9847', 'LIMIT IN', 'Anywhere'))), 'blocked')
        self.assertEqual(self.assess(status(('9840:9850/tcp', 'ALLOW IN', 'Anywhere'))), 'allowed')
        self.assertEqual(self.assess(status(('80,443,9847/tcp', 'ALLOW IN', 'Anywhere'))), 'allowed')
        self.assertEqual(self.assess(status(('SomeProfile', 'DENY IN', 'Anywhere'), ('9847/tcp', 'ALLOW IN', 'Anywhere'))), 'unknown')
        self.assertEqual(self.assess(status(('192.168.1.42 9847/tcp', 'ALLOW IN', 'Anywhere'))), 'unknown')

    def test_ipv4_rules_do_not_allow_ipv6(self):
        network = ipaddress.ip_network('fd12::/64')
        self.assertEqual(self.assess(status(('9847/tcp', 'ALLOW IN', 'Anywhere')), network=network), 'blocked')
        self.assertEqual(self.assess(status(('9847/tcp (v6)', 'ALLOW IN', 'fd12::/64')), network=network), 'allowed')

    def test_invalid_cidr_and_unrecognized_status(self):
        for value in ('192.168.1.0', '192.168.1.1/24', '0.0.0.0/0', '::/0', '127.0.0.0/8', '224.0.0.0/4', 'hello'):
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                FW.subnet(value)
        self.assertEqual(FW.subnet('192.168.1.0/24'), NETWORK)
        self.assertEqual(str(FW.subnet('fd12::/64')), 'fd12::/64')
        for text in ('unexpected output', HEADER + 'cannot parse this row\n'):
            with self.assertRaises(ValueError):
                FW.parse_status(text)

    def test_default_check_never_writes_and_explains_next_step(self):
        code, output, admin = self.invoke()
        self.assertEqual(code, 1)
        self.assertIn('TCP 9847', output)
        self.assertIn('UDP 9848', output)
        self.assertIn('--check-ports --open-firewall 192.168.1.0/24', output)
        self.assertEqual(admin.call_args_list[0].args, (['/usr/sbin/ufw', 'status', 'verbose'], False))
        self.assertEqual(admin.call_count, 1)

    def test_only_missing_protocol_is_opened(self):
        http = ('9847/tcp', 'ALLOW IN', str(NETWORK))
        udp = ('9848/udp', 'ALLOW IN', str(NETWORK))
        code, output, admin = self.invoke(['--open-firewall', str(NETWORK)],
            [completed(status(http)), completed('added'), completed(status(udp, http))])
        self.assertEqual(code, 0, output)
        self.assertEqual(admin.call_count, 3)
        self.assertEqual(admin.call_args_list[1].args[0][5], 'udp')

    def test_scoped_rules_verified_and_already_allowed_skipped(self):
        opened = status(('9847/tcp', 'ALLOW IN', str(NETWORK)), ('9848/udp', 'ALLOW IN', str(NETWORK)))
        code, output, admin = self.invoke(['--open-firewall', str(NETWORK)],
            [completed(status()), completed('added'), completed('added'), completed(opened)])
        self.assertEqual(code, 0, output)
        for call, port, protocol in zip(admin.call_args_list[1:3], ('9847', '9848'), ('tcp', 'udp')):
            self.assertEqual(call.args[0], ['/usr/sbin/ufw', 'prepend', 'allow', 'in', 'proto', protocol,
                'from', str(NETWORK), 'to', 'any', 'port', port, 'comment', f'OmaBeam {protocol.upper()} {port}'])
        code, _, admin = self.invoke(['--open-firewall', str(NETWORK)], [completed(opened)])
        self.assertEqual(code, 0)
        self.assertEqual(admin.call_count, 1)

    def test_write_failure_and_failed_verification_are_not_success(self):
        code, output, admin = self.invoke(['--open-firewall', str(NETWORK)],
            [completed(status()), completed('added'), completed('failed', 1)])
        self.assertEqual(code, 1)
        self.assertIn('Earlier successful rules remain', output)
        self.assertEqual(admin.call_count, 3)
        code, output, _ = self.invoke(['--open-firewall', str(NETWORK)],
            [completed(status()), completed('added'), completed('added'), completed(status())])
        self.assertEqual(code, 1)
        self.assertIn('could not be verified', output)

    def test_inactive_unavailable_and_permission_denied_never_write(self):
        for args, expected in (([], 0), (['--open-firewall', str(NETWORK)], 1)):
            code, _, admin = self.invoke(args, [completed('Status: inactive\n')])
            self.assertEqual(code, expected)
            self.assertEqual(admin.call_count, 1)
        code, output, admin = self.invoke(which=lambda _: None)
        self.assertEqual(code, 1)
        self.assertIn('UFW was not found', output)
        admin.assert_not_called()
        code, output, admin = self.invoke(outputs=[completed('', 1)])
        self.assertEqual(code, 1)
        self.assertIn('sudo access', output)
        self.assertEqual(admin.call_count, 1)

    def test_other_active_firewall_refuses_writes(self):
        with patch.object(FW, 'run', return_value=completed('running\n')):
            code, output, admin = self.invoke(['--open-firewall', str(NETWORK)], which=lambda name: name)
        self.assertEqual(code, 1)
        self.assertIn('firewalld is active', output)
        admin.assert_not_called()

    def test_default_route_discovery_excludes_container_networks(self):
        addresses = [{'ifname': device, 'addr_info': [{'family': 'inet', 'scope': 'global', 'local': address, 'prefixlen': prefix}]}
                     for device, address, prefix in [('wlan0', '192.168.1.42', 24), ('docker0', '172.17.0.1', 16), ('tun0', '10.0.0.1', 32)]]
        with patch.object(FW, 'run', side_effect=[completed(json.dumps([{'dev': 'wlan0'}, {'dev': 'tun0'}])), completed(json.dumps(addresses))]):
            self.assertEqual(FW.local_networks(), [(NETWORK, 'wlan0')])
        with patch.object(FW, 'run', return_value=completed('invalid JSON')):
            self.assertEqual(FW.local_networks(), [])

    def test_read_only_does_not_prompt_for_sudo(self):
        with patch.object(FW.os, 'geteuid', return_value=1000), patch.object(FW.shutil, 'which', return_value='/usr/bin/sudo'), \
             patch.object(FW, 'run', return_value=completed('', 1)) as run, \
             patch.object(FW.subprocess, 'run') as prompt:
            self.assertEqual(FW.admin(['/usr/sbin/ufw', 'status', 'verbose']).returncode, 1)
            run.assert_called_once_with(['sudo', '-n', '/usr/sbin/ufw', 'status', 'verbose'])
            prompt.assert_not_called()


if __name__ == '__main__':
    unittest.main(verbosity=2)
