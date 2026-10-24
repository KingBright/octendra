#!/usr/bin/env python3
"""Fresh Octendra installs only. Default action is a read-only plan.

Never reads legacy configuration, provisions credentials, changes DNS/proxies,
imports signing identities, changes TCC/ACL/policy, updates or stops a service.
Run on the target machine with an independently provisioned private config.
"""
import argparse
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import socket
import subprocess
import sys
import xml.etree.ElementTree as ET
from urllib.parse import urlsplit

MAC_ID = 'fun.hackerlife.octendra.agent'
WINDOWS_TASK = 'Octendra Code Agent'
BLOCKED_COMPONENTS = {'remote-hosts', 'remote-hosts-code', 'remote_hosts', 'remote-play', 'remote-play-code', 'octendra-code'}
DROP_CODE = ('import os,sys; os.setgroups([]); os.setgid(int(sys.argv[2])); '
             'os.setuid(int(sys.argv[1])); os.execv(sys.argv[3],sys.argv[3:])')


class Refusal(ValueError):
    pass


class PrivateArgumentParser(argparse.ArgumentParser):
    def error(self, message):
        raise Refusal('invalid or missing command-line option; use --help; auth values belong in private config files')


def digest(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1048576), b''):
            h.update(chunk)
    return h.hexdigest()


def checked(argv):
    result = subprocess.run(argv, capture_output=True, text=True, timeout=30)
    if result.returncode:
        raise Refusal('platform command failed: ' + Path(argv[0]).name +
                      '; exit=' + str(result.returncode) + '; inspect this new instance only')
    return result.stdout.strip()


def clean_path(value):
    if not value or any(c in str(value) for c in '\r\n\0'):
        raise Refusal('an absolute path without control characters is required')
    path = Path(os.path.abspath(os.path.expanduser(str(value))))
    for part in (path, *path.parents):
        if part.is_symlink():
            raise Refusal('symlink/reparse paths are not installation targets')
        if part.exists() and getattr(part.lstat(), 'st_file_attributes', 0) & 0x400:
            raise Refusal('symlink/reparse paths are not installation targets')
    return path


def new_namespace(value):
    path = clean_path(value)
    parts = {p.lower() for p in path.parts}
    normalized = lambda value: re.sub(r'[-_ ]', '', value)
    if ('octendra' not in parts or
            {normalized(p) for p in parts}.intersection(normalized(p) for p in BLOCKED_COMPONENTS)):
        raise Refusal('new paths must contain an octendra directory and no legacy install component')
    return path


def origin(value):
    if not isinstance(value,str) or not value or '${' in value:
        raise Refusal('missing new --public-origin; no existing origin is used')
    # urlsplit silently removes some whitespace/control characters. Reject them
    # before parsing instead of accepting a different origin from the input.
    if any(c.isspace() or ord(c)<32 or ord(c)==127 for c in value):
        raise Refusal('public origin must be one exact HTTPS hostname and optional valid port')
    try:
        parsed = urlsplit(value)
        hostname,port = parsed.hostname,parsed.port
    except ValueError as error:
        raise Refusal('public origin has an invalid hostname or port') from error
    if (parsed.scheme != 'https' or not hostname or parsed.username is not None
            or parsed.password is not None or parsed.path not in ('', '/')
            or '?' in value or '#' in value or '*' in value
            or parsed.netloc.endswith(':') or (port is not None and not 1<=port<=65535)):
        raise Refusal('public origin must be one exact HTTPS hostname and optional valid port')
    try:
        ipaddress.ip_address(hostname)
    except ValueError:
        pass
    else:
        raise Refusal('public origin must use the confirmed DNS hostname, not an IP address')
    labels=hostname.split('.')
    if (len(hostname)>253 or len(labels)<2 or labels[-1].isdigit()
            or any(len(label)>63 or not re.fullmatch(r'[A-Za-z0-9](?:[A-Za-z0-9-]*[A-Za-z0-9])?',label)
                   for label in labels)):
        raise Refusal('public origin must use the confirmed fully qualified DNS hostname')
    return 'https://' + hostname.lower() + (':'+str(port) if port not in (None,443) else '')


def host_platform():
    name = {'darwin': 'macos', 'linux': 'linux', 'win32': 'windows'}.get(sys.platform)
    machine = platform.machine().lower()
    if not name or (name == 'macos' and machine not in ('arm64', 'aarch64')) or (
            name != 'macos' and machine not in ('amd64', 'x86_64')):
        raise Refusal('supported targets are macOS arm64 and Linux/Windows x86-64')
    return name


def task_sid():
    return checked(['powershell.exe', '-NoProfile', '-NonInteractive', '-Command',
                    '[Security.Principal.WindowsIdentity]::GetCurrent().User.Value'])


def service_target(target, role, label):
    if target == 'linux':
        return (Path('/etc/systemd/system') if role == 'gateway' else
                Path.home()/'.config/systemd/user') / label
    if target == 'macos':
        return Path.home()/'Library/LaunchAgents'/(label+'.plist')
    return None


def probe_service(target, role, label, destination):
    if destination is not None and (destination.exists() or destination.is_symlink()):
        raise Refusal('new service definition already exists; never overwrite it')
    if target == 'linux':
        command = ['systemctl'] + (['--user'] if role == 'agent' else [])
        result = subprocess.run(command + ['show', label, '--property=LoadState'],
                                capture_output=True, text=True, timeout=15)
        if result.returncode or result.stderr.strip() or result.stdout.strip() != 'LoadState=not-found':
            raise Refusal('new systemd identity exists or service status is unknown')
    elif target == 'macos':
        result = subprocess.run(['launchctl', 'print', f'gui/{os.getuid()}/{label}'],
                                capture_output=True, text=True, timeout=15)
        # launchctl's documented missing-service response; other failures are unknown.
        if result.returncode == 0 or 'Could not find service' not in result.stderr:
            raise Refusal('new launchd identity exists or service status is unknown')
    else:
        code = "$s=New-Object -ComObject Schedule.Service;$s.Connect();$f=$s.GetFolder('\\');" + \
               "$n=@($f.GetTasks(1)|Where-Object {$_.Name -eq 'Octendra Code Agent'}).Count;" + \
               'Write-Output $n'
        if checked(['powershell.exe', '-NoProfile', '-NonInteractive', '-Command', code]) != '0':
            raise Refusal('new Windows task identity already exists')


def port_available(port):
    if not isinstance(port, int) or port < 1024 or port > 65535 or port == 18787:
        raise Refusal('choose a nonprivileged new loopback port; 18787 is reserved for legacy')
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
            probe.bind(('127.0.0.1', port))
    except OSError as error:
        raise Refusal('new loopback port is occupied; no fallback port selected') from error


def unit_quote(value):
    return '"' + str(value).replace('\\', '\\\\').replace('"', '\\"').replace('%', '%%').replace('$','$$') + '"'


def trusted_python():
    # Numeric service identities on NAS need no passwd/group entry. The only
    # root-executed code is this fixed privilege drop, never a user-owned script.
    binary=Path(sys.executable).resolve(strict=True)
    for value in (binary,Path(sys.base_prefix).resolve(strict=True),Path(os.__file__).resolve(strict=True)):
        for p in (value,*value.parents):
            s=p.stat()
            if s.st_uid!=0 or s.st_mode & 0o022:
                raise Refusal('numeric Gateway identity requires an existing root-owned, non-writable Python runtime')
    return binary


def windows_task(binary, config, sid):
    ns = 'http://schemas.microsoft.com/windows/2004/02/mit/task'
    ET.register_namespace('', ns)
    def child(parent, name, text=None, **attrs):
        node = ET.SubElement(parent, '{'+ns+'}'+name, attrs)
        node.text = text
        return node
    task = ET.Element('{'+ns+'}Task', {'version': '1.2'})
    reg = child(task, 'RegistrationInfo'); child(reg, 'Description', 'Octendra independent device agent')
    triggers = child(task, 'Triggers'); logon = child(triggers, 'LogonTrigger')
    child(logon, 'Enabled', 'true'); child(logon, 'UserId', sid)
    principals = child(task, 'Principals'); principal = child(principals, 'Principal', id='CurrentUser')
    child(principal, 'UserId', sid); child(principal, 'LogonType', 'InteractiveToken')
    child(principal, 'RunLevel', 'LeastPrivilege')
    settings = child(task, 'Settings'); child(settings, 'MultipleInstancesPolicy', 'IgnoreNew')
    child(settings, 'DisallowStartIfOnBatteries', 'false'); child(settings, 'StopIfGoingOnBatteries', 'false')
    child(settings, 'ExecutionTimeLimit', 'PT0S'); child(settings, 'Enabled', 'true')
    actions = child(task, 'Actions', Context='CurrentUser'); action = child(actions, 'Exec')
    child(action, 'Command', str(binary))
    child(action, 'Arguments', subprocess.list2cmdline(['agent', '--config', str(config)]))
    # Task Scheduler receives a Unicode COM BSTR after ReadAllText. A UTF-8
    # declaration contradicts that input and fails with 0x8004131a.
    return ET.tostring(task, encoding='utf-8', xml_declaration=False)


def definition(target, role, label, binary, config, args):
    if target == 'windows':
        return windows_task(binary, config, task_sid())
    if target == 'macos':
        return plistlib.dumps({'Label': label,
            'ProgramArguments': [str(binary), 'agent', '--config', str(config)],
            'RunAtLoad': True, 'KeepAlive': True, 'ThrottleInterval': 10,
            'ProcessType': 'Background', 'Umask': 0o077})
    user = ''
    if role == 'gateway':
        if not args.service_user or not args.service_group:
            raise Refusal('Gateway requires existing --service-user and --service-group')
        if any(not re.fullmatch(r'[A-Za-z0-9_.-]+', v) for v in (args.service_user, args.service_group)):
            raise Refusal('invalid existing service account name')
        if getattr(args,'_numeric_runtime',False):
            prefix=[str(args._trusted_python),'-I','-S','-c',DROP_CODE,args.service_user,args.service_group]
        else:
            user = f'User={args.service_user}\nGroup={args.service_group}\n';prefix=[]
    else: prefix=[]
    command=prefix+[str(binary),role,'--config',str(config)]
    agent_hardening = 'NoNewPrivileges=true\n' if role == 'agent' else ''
    return ('[Unit]\nDescription=Octendra independent '+role+'\nAfter=network-online.target\n'
            '[Service]\nType=simple\n'+user+'ExecStart='+' '.join(unit_quote(x) for x in command)+
            '\nRestart=on-failure\nRestartSec=5\nUmask=0077\n'+agent_hardening+
            '[Install]\nWantedBy='+('multi-user.target' if role == 'gateway' else 'default.target')+'\n').encode()


def mac_signature(args, raw_sha):
    if not args.signed_binary or not args.signing_receipt:
        raise Refusal('macOS needs the existing signing owner\'s --signed-binary and --signing-receipt')
    binary = clean_path(args.signed_binary); receipt = json.loads(clean_path(args.signing_receipt).read_text())
    certificate = receipt.get('certificate_sha1', '')
    if (receipt.get('candidate_sha256') != raw_sha or receipt.get('installed_sha256') != digest(binary)
            or receipt.get('code_identifier') != MAC_ID or not re.fullmatch(r'[0-9a-fA-F]{40}', certificate)):
        raise Refusal('signed candidate receipt does not match this release and new macOS identity')
    # codesign -R treats an unprefixed value as a requirements file path.
    requirement = '=identifier "'+MAC_ID+'" and certificate leaf = H"'+certificate+'"'
    checked(['codesign', '--verify', '--strict', '-R', requirement, str(binary)])
    return binary


def plan(args):
    # Check missing authorization before reading any configuration or writing files.
    new_origin = origin(args.public_origin)
    if not args.new_auth_provisioned or not args.config or not args.root or not args.state_dir:
        raise Refusal('require --new-auth-provisioned and explicit new --config, --root, --state-dir; no credentials generated or copied')
    if not args.legacy_origin or new_origin == origin(args.legacy_origin):
        raise Refusal('require a different explicit --legacy-origin; never reuse its OAuth audience')
    target = host_platform()
    if args.role == 'gateway' and target != 'linux':
        raise Refusal('this Gateway service entry targets Linux/NAS only')
    root, state, config = new_namespace(args.root), new_namespace(args.state_dir), new_namespace(args.config)
    if config != root/'config'/(args.role+'.json') or state == root or state in root.parents:
        raise Refusal('config must be NEW_ROOT/config/ROLE.json; state must be a separate new directory')
    if state.exists():
        raise Refusal('new state directory is occupied; never open or reuse an existing database')
    if (root/'config/releases').exists() or (root/'bin/releases').exists():
        raise Refusal('parallel installation must not inherit or seed upgrade release pointers')
    if not state.parent.is_dir():
        raise Refusal('new state parent must already exist; parent directories are not claimed')
    if not config.is_file():
        raise Refusal('new private config is missing; provision it separately without copying legacy credentials')
    permitted={'config','private'} if args.action!='activate' else {'config','private','bin','service','parallel-install.json','activation-request.json'}
    if any(p.name not in permitted or p.is_symlink() for p in root.iterdir()):
        raise Refusal('new root is occupied by unrecognized existing files or links')
    if {p.name for p in config.parent.iterdir()}!={args.role+'.json'}:
        raise Refusal('new config directory is occupied; only this role configuration may be present')
    info = config.stat()
    if target != 'windows' and info.st_mode & 0o077:
        raise Refusal('new private configuration must have mode 0600 or stricter')
    config_bytes = config.read_bytes()
    config_sha = hashlib.sha256(config_bytes).hexdigest()
    cfg = json.loads(config_bytes)
    if '${' in config_bytes.decode('utf-8'):
        raise Refusal('new configuration has unfilled placeholders; independently provision authentication first')
    if clean_path(cfg.get('state_dir', '')) != state:
        raise Refusal('configuration state_dir differs from the explicit new directory')
    if cfg.get('public_url' if args.role == 'gateway' else 'gateway_url') != new_origin:
        raise Refusal('configuration origin differs from the explicit new HTTPS origin')
    if args.role == 'gateway':
        if (not cfg.get('owner') or not str(cfg.get('password_hash', '')).startswith('$argon2')
                or cfg.get('devices') != [] or not isinstance(cfg.get('redirect_uris'), list)
                or cfg.get('forge_browser') is not None):
            raise Refusal('fresh Gateway requires a separately provisioned owner hash, empty devices and no inherited Forge profile')
        if cfg.get('bind') != f'127.0.0.1:{args.port}':
            raise Refusal('new Gateway must bind only the explicit loopback port')
        port_available(args.port)
        import pwd
        import grp
        if not args.service_user or not args.service_group:
            raise Refusal('explicit existing service account or verified numeric UID/GID are required')
        args._numeric_runtime=args.service_user.isdecimal() and args.service_group.isdecimal()
        if args._numeric_runtime:
            uid,gid=int(args.service_user),int(args.service_group)
            args._trusted_python=trusted_python()
        else:
            try:
                uid, gid = pwd.getpwnam(args.service_user).pw_uid, grp.getgrnam(args.service_group).gr_gid
            except KeyError as error:
                raise Refusal('specified unprivileged service account must already exist') from error
        if uid <= 0 or gid <= 0 or info.st_uid != uid or info.st_gid != gid:
            raise Refusal('new Gateway config must already belong to its existing unprivileged service account')
        for directory in config.parents:
            s = directory.stat()
            bits = (s.st_mode >> 6 if s.st_uid == uid else s.st_mode >> 3
                    if s.st_gid == gid else s.st_mode)
            if not bits & 1:
                raise Refusal('new service account cannot traverse the configuration path')
    else:
        uid, gid = (os.getuid(), os.getgid()) if target != 'windows' else (None, None)
        if (not cfg.get('device_id') or not cfg.get('device_token') or not cfg.get('roots')
                or not isinstance(cfg.get('allow_write'), bool) or not isinstance(cfg.get('allow_exec'), bool)):
            raise Refusal('new Agent needs its separately enrolled identity and explicit authorized roots/write/exec flags')
        if target != 'windows' and info.st_uid != uid:
            raise Refusal('new Agent config must belong to the current account')
    package = clean_path(args.package); manifest_path = package/'manifest.json'
    if not re.fullmatch(r'[0-9a-f]{64}', args.manifest_sha256 or '') or digest(manifest_path) != args.manifest_sha256:
        raise Refusal('require the exact verified --manifest-sha256; local manifest is not its own trust anchor')
    manifest = json.loads(manifest_path.read_text())
    if manifest.get('instance_upgrade_protocol')!=1:
        raise Refusal('bundle lacks verified instance-bound upgrade support; use the rebuilt delivery')
    filename = 'octendra-code-'+{'linux':'linux-amd64','macos':'macos-arm64','windows':'windows-amd64.exe'}[target]
    source = clean_path(package/filename); expected = manifest['artifacts'][filename]['sha256']
    if digest(source) != expected:
        raise Refusal('release binary checksum mismatch')
    if target == 'macos':
        source = mac_signature(args, expected)
    checked([str(source), 'check'] + (['--agent'] if args.role == 'agent' else []) +
            ['--config', str(config)])
    label = (MAC_ID if target == 'macos' else WINDOWS_TASK if target == 'windows' else
             'octendra-'+args.role+'.service')
    destination = service_target(target, args.role, label)
    if destination is not None: destination = clean_path(destination)
    probe_service(target, args.role, label, destination)
    binary = root/'bin'/('octendra-code.exe' if target == 'windows' else 'octendra-code')
    candidate = root/'service'/('task.xml' if target == 'windows' else label+'.plist' if target == 'macos' else label)
    marker = root/'parallel-install.json'
    binary_sha = digest(source)
    if args.action == 'activate':
        if not marker.is_file() or (root/'activation-request.json').exists():
            raise Refusal('stage once before activation; an existing activation request must be observed, never replayed')
        saved = json.loads(marker.read_text())
        if (saved.get('binary_sha256') != binary_sha or saved.get('config_sha256') != config_sha
                or saved.get('manifest_sha256') != args.manifest_sha256
                or not binary.is_file() or digest(binary) != binary_sha
                or not candidate.is_file() or digest(candidate) != saved.get('definition_sha256')):
            raise Refusal('staged installation differs from its original receipt')
    elif binary.exists() or marker.exists() or candidate.exists() or (root/'bin').exists() or (root/'service').exists():
        raise Refusal('new installation path is occupied; no replacement or automatic resume')
    content = definition(target, args.role, label, binary, config, args)
    if args.action == 'activate' and target=='linux':
        identity_path=root/'bin/upgrade-identity.json'
        expected_identity={'schema':1,'role':args.role,'service':label,'executable':str(binary),
                           'config':str(config),'state_dir':str(state),'definition':str(destination),
                           'definition_sha256':hashlib.sha256(content).hexdigest()}
        if identity_path.is_symlink() or not identity_path.is_file() or json.loads(identity_path.read_text())!=expected_identity:
            raise Refusal('staged instance upgrade identity differs from its new service binding')
    if args.action == 'activate' and hashlib.sha256(content).hexdigest() != saved['definition_sha256']:
        raise Refusal('current account or service definition differs from the original staged plan')
    return {'target':target,'role':args.role,'root':root,'state':state,'config':config,'source':source,
            'binary':binary,'binary_sha256':binary_sha,'candidate':candidate,'definition':content,
            'destination':destination,'label':label,'marker':marker,'uid':uid,'gid':gid,
            'manifest_sha256':args.manifest_sha256,'config_sha256':config_sha,'origin':new_origin,
            'runtime_identity':(args.service_user+':'+args.service_group) if args.role=='gateway' else 'current interactive account',
            'upgrade_policy':'installed Linux instance binding required; no inferred service/path targets'}


def exclusive(path, data, mode):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, mode)
    with os.fdopen(fd, 'wb') as stream:
        stream.write(data)
        stream.flush(); os.fsync(stream.fileno())


def public_plan(p):
    return {k:str(p[k]) for k in ('role','target','root','state','config','binary','candidate','destination','label','origin','runtime_identity','upgrade_policy')}


def stage(p):
    if digest(p['config']) != p['config_sha256']:
        raise Refusal('new configuration changed after preflight')
    (p['root']/'bin').mkdir(mode=0o755)
    (p['root']/'service').mkdir(mode=0o700)
    exclusive(p['binary'], p['source'].read_bytes(), 0o755)
    if digest(p['binary']) != p['binary_sha256']:
        raise Refusal('candidate changed during copy; inspect only this new installation')
    exclusive(p['candidate'], p['definition'], 0o600)
    if p['target']=='linux':
        identity={'schema':1,'role':p['role'],'service':p['label'],
                  'executable':str(p['binary']),'config':str(p['config']),
                  'state_dir':str(p['state']),'definition':str(p['destination']),
                  'definition_sha256':digest(p['candidate'])}
        exclusive(p['root']/'bin/upgrade-identity.json',(json.dumps(identity,indent=2)+'\n').encode(),0o644)
        if p['role']=='gateway':
            # Only the newly-created binary directory belongs to the selected
            # runtime; self-upgrade can stage bytes without touching old paths.
            os.chown(p['root']/'bin',p['uid'],p['gid'],follow_symlinks=False)
            os.chown(p['binary'],p['uid'],p['gid'],follow_symlinks=False)
    record = {'binary_sha256':p['binary_sha256'],'config_sha256':p['config_sha256'],
              'definition_sha256':digest(p['candidate']),'manifest_sha256':p['manifest_sha256'],
              'plan':public_plan(p),'credentials_created_or_copied':False}
    exclusive(p['marker'], (json.dumps(record,indent=2)+'\n').encode(), 0o600)


def activate(p):
    if p['role']=='gateway' and os.geteuid()!=0:
        raise Refusal('Gateway activation requires the existing root administrative channel; no elevation is requested')
    if digest(p['config']) != p['config_sha256']:
        raise Refusal('new configuration changed after preflight')
    # The request is exclusive: a partial/unknown result cannot automatically replay.
    exclusive(p['root']/'activation-request.json', (json.dumps(public_plan(p))+'\n').encode(), 0o600)
    p['state'].mkdir(mode=0o700)
    if p['role'] == 'gateway': os.chown(p['state'], p['uid'], p['gid'], follow_symlinks=False)
    if p['destination'] is not None:
        if not p['destination'].parent.is_dir():
            raise Refusal('service definition parent is missing; no system configuration created')
        exclusive(p['destination'], p['candidate'].read_bytes(), 0o644 if p['target']=='linux' else 0o600)
    if p['target'] == 'linux':
        command = ['systemctl'] + (['--user'] if p['role']=='agent' else [])
        checked(command+['daemon-reload']); checked(command+['enable',p['label']])
        checked(command+['start',p['label']])
    elif p['target'] == 'macos':
        checked(['launchctl','bootstrap',f'gui/{os.getuid()}',str(p['destination'])])
    else:
        # TASK_CREATE=2 (never update), TASK_LOGON_INTERACTIVE_TOKEN=3; no password,
        # RunAs, UAC request, ACL override or ExecutionPolicy change.
        xml = str(p['candidate']).replace("'", "''")
        code = "$s=New-Object -ComObject Schedule.Service;$s.Connect();$f=$s.GetFolder('\\');" + \
               "$x=[IO.File]::ReadAllText('"+xml+"');$u=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value;" + \
               "$t=$f.RegisterTask('Octendra Code Agent',$x,2,$u,$null,3,$null);$t.Run($null)|Out-Null"
        checked(['powershell.exe','-NoProfile','-NonInteractive','-Command',code])


def parser():
    p=PrivateArgumentParser(description=__doc__)
    p.add_argument('--action',choices=['plan','stage','activate'],default='plan')
    p.add_argument('--role',choices=['gateway','agent'],required=True)
    p.add_argument('--package',default=str(Path(__file__).resolve().parent))
    for name in ('manifest-sha256','root','state-dir','config','public-origin','legacy-origin',
                 'service-user','service-group','signed-binary','signing-receipt'):
        p.add_argument('--'+name)
    p.add_argument('--port',type=int,default=18788)
    p.add_argument('--new-auth-provisioned',action='store_true')
    return p


def main():
    args=None
    try:
        args=parser().parse_args()
        p=plan(args)
        if args.action=='stage': stage(p)
        elif args.action=='activate': activate(p)
        print(json.dumps({'state':'activation_submitted' if args.action=='activate' else args.action+'_completed',
                          'plan':public_plan(p),'runtime_verified':False,
                          'service_started':None if args.action=='activate' else False,
                          'public_listener_created':False,'legacy_changed':False,
                          'credentials_created_or_copied':False}))
    except (ValueError,OSError,KeyError,subprocess.SubprocessError) as error:
        # Never dump configuration contents, auth values or platform command stderr.
        print(json.dumps({'state':'stopped','reason':str(error) if isinstance(error,Refusal)
                          else type(error).__name__,'action':args.action if args else 'unparsed',
                          'next':'inspect the new-instance inputs/request; do not replay an uncertain activation'}),file=sys.stderr)
        return 2
    return 0


if __name__=='__main__':
    sys.exit(main())
