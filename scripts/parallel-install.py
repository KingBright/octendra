#!/usr/bin/env python3
"""Fresh Octendra installs and one narrowly bound Linux UMask correction.

Default actions are read-only plans. The repair never starts/enables a service.

Never reads legacy configuration, provisions credentials, changes DNS/proxies,
imports signing identities, changes TCC/ACL/policy, or stops a service.
Run on the target machine with an independently provisioned private config.
"""
import argparse
import contextlib
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import socket
import stat
import subprocess
import sys
import uuid
import xml.etree.ElementTree as ET
from urllib.parse import urlsplit

MAC_ID = 'fun.hackerlife.octendra.agent'
WINDOWS_TASK = 'Octendra Code Agent'
LINUX_HOME_ALIAS = Path('/home')
LINUX_CANONICAL_HOME = Path('/var/home')
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


def host_artifact(name):
    # This installer is also retained standalone beside installed-instance receipts.
    # Keep selection local rather than introducing a new runtime helper dependency.
    arch = {'aarch64':'arm64', 'arm64':'arm64', 'x86_64':'amd64', 'amd64':'amd64'}.get(platform.machine().lower())
    if (name, arch) not in {('macos','arm64'), ('linux','arm64'), ('linux','amd64'), ('windows','amd64')}:
        raise Refusal('supported targets are macOS arm64, Linux arm64/x86-64 and Windows x86-64')
    return 'octendra-code-'+name+'-'+arch+('.exe' if name=='windows' else '')


def host_platform():
    name = {'darwin': 'macos', 'linux': 'linux', 'win32': 'windows'}.get(sys.platform)
    host_artifact(name)
    return name


def task_sid():
    return checked(['powershell.exe', '-NoProfile', '-NonInteractive', '-Command',
                    '[Security.Principal.WindowsIdentity]::GetCurrent().User.Value'])


def linux_directory(info, owners, owner_bits=0):
    if (not stat.S_ISDIR(info.st_mode) or info.st_uid not in owners
            or info.st_mode & 0o022
            or info.st_mode & owner_bits != owner_bits
            or getattr(info, 'st_file_attributes', 0) & 0x400):
        raise Refusal('Linux user-unit directory ownership, permissions or type is unsafe')


def linux_home_path(value):
    """Normalize only the verified OS /home -> /var/home alias, never user links."""
    if (not value or any(c in str(value) for c in '\r\n\0')
            or Path(value).anchor != '/' or '..' in Path(value).parts):
        raise Refusal('Linux account home must be an absolute unambiguous path')
    path = Path(value)
    if LINUX_HOME_ALIAS == path or LINUX_HOME_ALIAS in path.parents:
        link = LINUX_HOME_ALIAS.lstat()
        if stat.S_ISLNK(link.st_mode):
            # Symlink mode 0777 is normal; replacement authority is determined
            # by the root-owned, non-writable parent and the pinned target.
            for parent in reversed(LINUX_HOME_ALIAS.parents):
                linux_directory(parent.lstat(), {0})
            target = os.readlink(LINUX_HOME_ALIAS)
            if (link.st_uid != 0 or getattr(link, 'st_file_attributes', 0) & 0x400
                    or target not in ('var/home', str(LINUX_CANONICAL_HOME))
                    or Path(os.path.abspath(LINUX_HOME_ALIAS.parent / target)) != LINUX_CANONICAL_HOME):
                raise Refusal('untrusted Linux system home alias')
            path = LINUX_CANONICAL_HOME / path.relative_to(LINUX_HOME_ALIAS)
    return path


def linux_user_home():
    import pwd
    uid = os.getuid()
    if uid != os.geteuid():
        raise Refusal('Linux Agent requires the existing current-account identity')
    home = linux_home_path(pwd.getpwuid(uid).pw_dir)
    for parent in reversed(home.parents):
        linux_directory(parent.lstat(), {0})
    linux_directory(home.lstat(), {uid}, 0o100)
    if os.environ.get('HOME') and linux_home_path(os.environ['HOME']) != home:
        raise Refusal('HOME differs from the verified Linux account home')
    config = os.environ.get('XDG_CONFIG_HOME')
    if config and linux_home_path(config) != home / '.config':
        raise Refusal('custom XDG_CONFIG_HOME is not this verified user-unit directory')
    return home


@contextlib.contextmanager
def linux_unit_parent(destination):
    """Bind the entire canonical directory chain before any new unit write."""
    home = linux_user_home()
    if destination != home / '.config/systemd/user/octendra-agent.service':
        raise Refusal('Linux user-unit destination differs from the verified home')
    parent = destination.parent
    chain = list(reversed((parent, *parent.parents)))
    descriptors = []
    proof = []
    flags = getattr(os, 'O_PATH', os.O_RDONLY) | os.O_DIRECTORY | os.O_NOFOLLOW
    try:
        for path in chain:
            fd = (os.open(path, flags) if not descriptors else
                  os.open(path.name, flags, dir_fd=descriptors[-1]))
            descriptors.append(fd)
            info = os.fstat(fd)
            user_path = path == home or home in path.parents
            linux_directory(info, {os.getuid()} if user_path else {0},
                            0o300 if path == parent else 0o100 if user_path else 0)
            proof.append((info.st_dev, info.st_ino, info.st_uid, stat.S_IMODE(info.st_mode)))
        yield descriptors[-1]
        if linux_user_home() != home:
            raise Refusal('Linux home changed during unit preparation')
        for path, expected in zip(chain, proof):
            info = path.lstat()
            user_path = path == home or home in path.parents
            linux_directory(info, {os.getuid()} if user_path else {0},
                            0o300 if path == parent else 0o100 if user_path else 0)
            actual = (info.st_dev, info.st_ino, info.st_uid, stat.S_IMODE(info.st_mode))
            if not stat.S_ISDIR(info.st_mode) or actual != expected:
                raise Refusal('Linux user-unit directory changed during preparation')
    finally:
        for fd in reversed(descriptors):
            os.close(fd)


def service_target(target, role, label):
    if target == 'linux':
        return (Path('/etc/systemd/system') if role == 'gateway' else
                linux_user_home()/'.config/systemd/user') / label
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
    child(settings, 'StartWhenAvailable', 'true')
    recovery = child(settings, 'RestartOnFailure')
    child(recovery, 'Interval', 'PT1M'); child(recovery, 'Count', '999')
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
            '\nRestart=on-failure\nRestartSec=5\nUMask=0077\n'+agent_hardening+
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
    filename = host_artifact(target)
    if filename not in manifest['artifacts']:
        raise Refusal('verified package lacks this target architecture')
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
    if target == 'linux' and args.role == 'agent':
        with linux_unit_parent(destination):
            probe_service(target, args.role, label, destination)
    else:
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


def exclusive(path, data, mode, *, dir_fd=None, force_mode=False):
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    fd = (os.open(path, flags, mode) if dir_fd is None else
          os.open(path.name, flags | os.O_NOFOLLOW, mode, dir_fd=dir_fd))
    with os.fdopen(fd, 'wb') as stream:
        if force_mode: os.fchmod(stream.fileno(), mode)
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


def activation_files(p, unit_parent=None):
    # The request is exclusive: a partial/unknown result cannot automatically replay.
    exclusive(p['root']/'activation-request.json', (json.dumps(public_plan(p))+'\n').encode(), 0o600)
    p['state'].mkdir(mode=0o700)
    if p['role'] == 'gateway': os.chown(p['state'], p['uid'], p['gid'], follow_symlinks=False)
    if p['destination'] is not None:
        if unit_parent is None and not p['destination'].parent.is_dir():
            raise Refusal('service definition parent is missing; no system configuration created')
        exclusive(p['destination'], p['candidate'].read_bytes(),
                  0o644 if p['target']=='linux' else 0o600, dir_fd=unit_parent)


def activate(p):
    if p['role']=='gateway' and os.geteuid()!=0:
        raise Refusal('Gateway activation requires the existing root administrative channel; no elevation is requested')
    if digest(p['config']) != p['config_sha256']:
        raise Refusal('new configuration changed after preflight')
    if p['target']=='linux' and p['role']=='agent':
        with linux_unit_parent(p['destination']) as directory:
            activation_files(p, directory)
    else:
        activation_files(p)
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



# This is deliberately not a general installed-unit editor. Historical stage and
# activation receipts remain immutable; only the current upgrade binding moves.
REPAIR_FILES = ('umask-repair-request.json', 'umask-repair-reload.json', 'umask-repair-result.json')


def sha_bytes(data):
    return hashlib.sha256(data).hexdigest()


def repair_file_proof(info):
    return tuple(getattr(info, key) for key in ('st_dev','st_ino','st_uid','st_gid','st_mode','st_nlink',
                                               'st_size','st_mtime_ns','st_ctime_ns'))


def repair_read(fd, name, private=False, limit=1048576):
    before = os.stat(name, dir_fd=fd, follow_symlinks=False)
    if (not stat.S_ISREG(before.st_mode) or before.st_uid != os.getuid()
            or before.st_nlink != 1 or before.st_mode & (0o7077 if private else 0o7022)
            or before.st_size > limit):
        raise Refusal('repair input type, owner, permissions or size is unsafe')
    source = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=fd)
    try:
        opened = os.fstat(source)
        if repair_file_proof(opened) != repair_file_proof(before):
            raise Refusal('repair input changed while opening')
        with os.fdopen(source, 'rb', closefd=False) as stream:
            data = stream.read(limit + 1)
        if repair_file_proof(os.fstat(source)) != repair_file_proof(opened) or len(data) > limit:
            raise Refusal('repair input changed while reading')
    finally:
        os.close(source)
    return data, stat.S_IMODE(before.st_mode)


@contextlib.contextmanager
def repair_directories(root, destination):
    home = linux_user_home()
    if root != home / '.local/share/octendra/agent':
        raise Refusal('UMask repair is limited to the canonical installed Linux Agent root')
    descriptors = {}; proof = {}
    with linux_unit_parent(destination) as bound_unit_fd:
        if bound_unit_fd is None:
            raise Refusal('repair requires a bound Linux unit directory')
        # linux_unit_parent intentionally uses O_PATH on Linux. Open an I/O
        # descriptor for fsync without resolving the visible path again.
        unit_fd = os.open('.', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=bound_unit_fd)
        unit_chain = (destination.parent, *destination.parent.parents)
        unit_proof = {path:(path.lstat().st_dev, path.lstat().st_ino, path.lstat().st_uid,
                           stat.S_IMODE(path.lstat().st_mode)) for path in unit_chain}
        try:
            info = os.fstat(unit_fd)
            if (info.st_dev, info.st_ino, info.st_uid, stat.S_IMODE(info.st_mode)) != unit_proof[destination.parent]:
                raise Refusal('bound unit directory changed before repair')
            paths = [home, home/'.local', home/'.local/share', home/'.local/share/octendra', root,
                     root/'bin', root/'service', root/'config', root/'state']
            for path in paths:
                fd = (os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW) if path == home else
                      os.open(path.name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                              dir_fd=descriptors[path.parent]))
                descriptors[path] = fd
                info = os.fstat(fd); linux_directory(info, {os.getuid()}, 0o100)
                if path == root/'state' and stat.S_IMODE(info.st_mode) != 0o700:
                    raise Refusal('repair requires the existing private state directory')
                proof[path] = (info.st_dev, info.st_ino, info.st_uid, stat.S_IMODE(info.st_mode))
            def stable():
                if linux_user_home() != home:
                    raise Refusal('repair account home changed')
                for path, expected in (proof | unit_proof).items():
                    info = path.lstat()
                    if (info.st_dev, info.st_ino, info.st_uid, stat.S_IMODE(info.st_mode)) != expected:
                        raise Refusal('repair directory changed during the transaction')
            yield descriptors, unit_fd, stable
            stable()
        finally:
            for fd in reversed(list(descriptors.values())): os.close(fd)
            os.close(unit_fd)


def repair_status(destination):
    text = checked(['systemctl', '--user', 'show', 'octendra-agent.service',
                    '--property=LoadState,ActiveState,SubState,MainPID,ControlPID,Job,FragmentPath,UMask,NoNewPrivileges'])
    pairs = [line.split('=', 1) for line in text.splitlines()]
    if any(len(pair) != 2 for pair in pairs) or len({pair[0] for pair in pairs}) != len(pairs):
        raise Refusal('installed service status is ambiguous')
    value = dict(pairs)
    fragment = clean_path(linux_home_path(value.get('FragmentPath', '')))
    if (set(value) != {'LoadState','ActiveState','SubState','MainPID','ControlPID','Job',
                       'FragmentPath','UMask','NoNewPrivileges'}
            or value['LoadState'] != 'loaded' or fragment != destination
            or value['NoNewPrivileges'] != 'yes' or value['UMask'] not in ('0022','0077')):
        raise Refusal('installed service identity or effective hardening differs')
    return value


def repair_inactive(destination):
    value = repair_status(destination)
    if (value['ActiveState'] != 'inactive' or value['SubState'] != 'dead'
            or value['MainPID'] != '0' or value['ControlPID'] != '0' or value['Job']):
        raise Refusal('repair requires the bound service inactive with no pending job')
    return value


def repair_producer():
    # Linux start ticks distinguish the original producer from a reused PID.
    return {'pid':os.getpid(), 'start_ticks':Path('/proc/self/stat').read_text().rsplit(')', 1)[1].split()[19]}


def repair_producer_finished(value):
    if (not isinstance(value, dict) or set(value) != {'pid','start_ticks'}
            or type(value['pid']) is not int or value['pid'] <= 1
            or not isinstance(value['start_ticks'], str)
            or not re.fullmatch(r'[0-9]+', value['start_ticks'])):
        raise Refusal('repair producer identity is invalid')
    try:
        now = Path('/proc')/str(value['pid'])/'stat'
        return now.read_text().rsplit(')', 1)[1].split()[19] != value['start_ticks']
    except FileNotFoundError:
        return True


def repair_replace(fd, name, before, after, mode, request_id, *, recover=False):
    current, current_mode = repair_read(fd, name, private=mode == 0o600)
    if current_mode != mode or current not in (before, after):
        raise Refusal('repair target changed; no foreign bytes are overwritten')
    original_proof = repair_file_proof(os.stat(name, dir_fd=fd, follow_symlinks=False))
    temporary = '.'+name+'.umask-'+request_id+'.tmp'
    if current == after:
        if not recover:
            raise Refusal('repair target changed before its recorded replacement')
        # A leftover temp is never silently deleted or treated as an unrelated file.
        try: os.stat(temporary, dir_fd=fd, follow_symlinks=False)
        except FileNotFoundError: pass
        else: raise Refusal('unexpected repair temporary beside an already updated target')
        return
    try:
        staged, staged_mode = repair_read(fd, temporary, private=mode == 0o600)
        if not recover:
            raise Refusal('existing repair temporary must be observed, never adopted by a new apply')
        if staged != after or staged_mode != mode:
            raise Refusal('repair temporary differs from this exact transaction')
    except FileNotFoundError:
        exclusive(Path(temporary), after, mode, dir_fd=fd, force_mode=True)
    # Compare again immediately before replacement, including the bound directory.
    if (repair_read(fd, name, private=mode == 0o600) != (before, mode)
            or repair_file_proof(os.stat(name, dir_fd=fd, follow_symlinks=False)) != original_proof):
        raise Refusal('repair target changed before replacement')
    os.replace(temporary, name, src_dir_fd=fd, dst_dir_fd=fd)
    os.fsync(fd)


def repair_umask(args):
    if host_platform() != 'linux' or args.role != 'agent':
        raise Refusal('UMask repair only supports the existing Linux user Agent')
    if (not re.fullmatch(r'[0-9a-f]{64}', args.expected_definition_sha256 or '')
            or not re.fullmatch(r'[0-9a-f]{64}', args.expected_activation_sha256 or '')
            or not re.fullmatch(r'[0-9a-f]{64}', args.expected_stage_sha256 or '')
            or not re.fullmatch(r'[0-9a-f]{64}', args.manifest_sha256 or '')):
        raise Refusal('repair requires externally verified original definition, activation and package hashes')
    try:
        if str(uuid.UUID(args.repair_request_id)) != args.repair_request_id: raise ValueError()
    except (ValueError, TypeError, AttributeError):
        raise Refusal('repair requires one canonical immutable request UUID')
    root = new_namespace(args.root)
    config, state, binary = root/'config/agent.json', root/'state', root/'bin/octendra-code'
    destination = service_target('linux', 'agent', 'octendra-agent.service')
    if ((args.config and clean_path(args.config) != config)
            or (args.state_dir and clean_path(args.state_dir) != state)):
        raise Refusal('repair paths differ from the bound installed instance')
    package = clean_path(args.package)
    if digest(package/'manifest.json') != args.manifest_sha256:
        raise Refusal('repair package differs from the verified manifest')
    manifest_bytes = (package/'manifest.json').read_bytes()
    if sha_bytes(manifest_bytes) != args.manifest_sha256:
        raise Refusal('repair manifest changed while reading')
    manifest = json.loads(manifest_bytes)
    helper = clean_path(__file__)
    if (manifest.get('instance_upgrade_protocol') != 1 or helper != package/'parallel-install.py'
            or manifest['artifacts']['parallel-install.py']['sha256'] != digest(helper)):
        raise Refusal('repair must run the verified packaged installer entry')
    desired = definition('linux', 'agent', 'octendra-agent.service', binary, config, args)
    old = desired.replace(b'\nUMask=0077\n', b'\nUmask=0077\n')
    if (old == desired or sha_bytes(old) != args.expected_definition_sha256
            or old.count(b'\nUmask=0077\n') != 1):
        raise Refusal('repair accepts only the exact original Umask to UMask correction')
    expected_identity = {'schema':1, 'role':'agent', 'service':'octendra-agent.service',
                         'executable':str(binary), 'config':str(config), 'state_dir':str(state),
                         'definition':str(destination), 'definition_sha256':sha_bytes(old)}
    desired_identity = dict(expected_identity, definition_sha256=sha_bytes(desired))
    with repair_directories(root, destination) as (directories, unit_fd, stable):
        root_fd, bin_fd = directories[root], directories[root/'bin']
        marker, _ = repair_read(root_fd, 'parallel-install.json', True)
        # Lock the immutable historical receipt, never a replaceable progress file.
        import fcntl
        lock = os.open('parallel-install.json', os.O_RDONLY | os.O_NOFOLLOW, dir_fd=root_fd)
        try:
            try: fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError: raise Refusal('another installed-instance repair is active')
            if repair_file_proof(os.fstat(lock)) != repair_file_proof(os.stat('parallel-install.json', dir_fd=root_fd, follow_symlinks=False)):
                raise Refusal('historical receipt changed before repair lock')
            if sha_bytes(marker) != args.expected_stage_sha256:
                raise Refusal('original historical stage receipt differs')
            activation, _ = repair_read(root_fd, 'activation-request.json', True)
            if sha_bytes(activation) != args.expected_activation_sha256:
                raise Refusal('original activation request differs; never fresh activate')
            saved = json.loads(marker); activated = json.loads(activation)
            bound = {'target':'linux', 'role':'agent', 'root':str(root), 'state':str(state),
                     'config':str(config), 'binary':str(binary), 'candidate':str(root/'service/octendra-agent.service'),
                     'destination':str(destination), 'label':'octendra-agent.service'}
            if (any(saved['plan'].get(key) != value or activated.get(key) != value for key,value in bound.items())
                    or saved['plan'] != activated or saved['definition_sha256'] != sha_bytes(old)
                    or not re.fullmatch(r'[0-9a-f]{64}', saved.get('manifest_sha256',''))):
                raise Refusal('original installed path or historical definition binding differs')
            config_bytes, config_mode = repair_read(directories[root/'config'], 'agent.json', True)
            binary_bytes, binary_mode = repair_read(bin_fd, 'octendra-code', limit=268435456)
            if (binary_mode not in (0o700,0o755) or sha_bytes(config_bytes) != saved['config_sha256'] or sha_bytes(binary_bytes) != saved['binary_sha256']
                    or manifest['artifacts'][host_artifact('linux')]['sha256'] != saved['binary_sha256']):
                raise Refusal('installed binary/config or verified package binary changed')
            cfg = json.loads(config_bytes)
            if cfg.get('state_dir') != str(state) or cfg.get('gateway_url') != saved['plan']['origin']:
                raise Refusal('installed configuration path/origin differs')
            def no_upgrade_history():
                try: os.stat('releases', dir_fd=bin_fd, follow_symlinks=False)
                except FileNotFoundError: return
                raise Refusal('existing upgrade activity/history requires separate observation')
            no_upgrade_history()
            identity, identity_mode = repair_read(bin_fd, 'upgrade-identity.json')
            candidate, candidate_mode = repair_read(directories[root/'service'], 'octendra-agent.service', True)
            unit, unit_mode = repair_read(unit_fd, destination.name)
            if (identity_mode not in (0o600,0o644) or candidate_mode != 0o600 or unit_mode not in (0o600,0o644)
                    or json.loads(identity) not in (expected_identity, desired_identity)
                    or candidate not in (old, desired) or unit not in (old, desired)):
                raise Refusal('current definition or upgrade identity is outside this repair')
            # The original installer respected the maintenance process umask.
            # Bind and preserve its stricter modes; never grant extra read bits.
            file_modes={'binary':binary_mode,'config':config_mode,'identity':identity_mode,
                        'candidate':candidate_mode,'unit':unit_mode}
            anchor = {'schema':1, 'kind':'linux-agent-umask0077', 'request_id':args.repair_request_id,
                      'root':str(root), 'original_definition_sha256':sha_bytes(old),
                      'activation_sha256':sha_bytes(activation), 'history_sha256':sha_bytes(marker),
                      'config_sha256':sha_bytes(config_bytes), 'binary_sha256':sha_bytes(binary_bytes),
                      'manifest_sha256':args.manifest_sha256,'file_modes':file_modes}
            try:
                request_bytes = repair_read(root_fd, REPAIR_FILES[0], True)[0]
                request = json.loads(request_bytes)
                if not isinstance(request, dict): raise Refusal('repair request is invalid')
            except FileNotFoundError: request = None;request_bytes = None
            if request is not None:
                if (set(request) != set(anchor) | {'old_identity','producer'}
                        or any(request.get(key) != value for key,value in anchor.items())
                        or json.loads(request['old_identity']) != expected_identity):
                    raise Refusal('repair request differs from the original immutable transaction')
                old_identity = request['old_identity'].encode()
            else:
                if (unit != old or candidate != old or json.loads(identity) != expected_identity
                        or any(os.path.lexists(root/name) for name in REPAIR_FILES[1:])):
                    raise Refusal('unrecorded installed repair state; inspect without replay')
                old_identity = (json.dumps(expected_identity, indent=2)+'\n').encode()
                if identity != old_identity:
                    raise Refusal('original upgrade identity bytes differ from the staged binding')
            new_identity = (json.dumps(desired_identity, indent=2)+'\n').encode()
            if identity not in (old_identity, new_identity):
                raise Refusal('current upgrade identity bytes changed outside this transaction')
            reload_record = {'request_id':args.repair_request_id, 'action':'daemon-reload-once'}
            try:
                reload_bytes = repair_read(root_fd, REPAIR_FILES[1], True)[0]
                reload_saved = json.loads(reload_bytes)
                if reload_saved != reload_record or request is None:
                    raise Refusal('repair reload intent differs from the immutable transaction')
            except FileNotFoundError: reload_saved = None;reload_bytes = None
            def observed():
                return {'unit':sha_bytes(repair_read(unit_fd, destination.name)[0]),
                        'candidate':sha_bytes(repair_read(directories[root/'service'], destination.name, True)[0]),
                        'identity':sha_bytes(repair_read(bin_fd, 'upgrade-identity.json')[0])}
            complete = {'unit':sha_bytes(desired), 'candidate':sha_bytes(desired), 'identity':sha_bytes(new_identity)}
            def record(name, value):
                data = (json.dumps(value, indent=2)+'\n').encode()
                exclusive(Path(name), data, 0o600, dir_fd=root_fd, force_mode=True)
                os.fsync(root_fd)
                return data
            def unchanged():
                stable();no_upgrade_history()
                for fd,name,allowed,mode in ((unit_fd,destination.name,(old,desired),unit_mode),
                        (directories[root/'service'],destination.name,(old,desired),candidate_mode),
                        (bin_fd,'upgrade-identity.json',(old_identity,new_identity),identity_mode)):
                    content,current_mode = repair_read(fd,name,private=mode==0o600)
                    if current_mode != mode or content not in allowed:
                        raise Refusal('repair target changed outside its exact before/after images')
                if (repair_read(root_fd, 'parallel-install.json', True)[0] != marker
                        or repair_read(root_fd, 'activation-request.json', True)[0] != activation
                        or repair_read(directories[root/'config'], 'agent.json', True) != (config_bytes,config_mode)
                        or repair_read(bin_fd, 'octendra-code', limit=268435456) != (binary_bytes,binary_mode)):
                    raise Refusal('immutable repair inputs changed during the transaction')
                if request is not None and repair_read(root_fd, REPAIR_FILES[0], True)[0] != request_bytes:
                    raise Refusal('immutable repair request changed')
                if reload_saved is not None and repair_read(root_fd, REPAIR_FILES[1], True)[0] != reload_bytes:
                    raise Refusal('immutable reload intent changed')
            try:
                result = json.loads(repair_read(root_fd, REPAIR_FILES[2], True)[0])
                if not isinstance(result, dict): raise Refusal('repair result is invalid')
            except FileNotFoundError: result = None
            if result is not None:
                if (not request or reload_saved != reload_record
                        or result != {'request_id':args.repair_request_id,'state':'complete','hashes':complete}
                        or observed() != complete or repair_status(destination)['UMask'] != '0077'):
                    raise Refusal('completed repair evidence no longer matches the instance')
                return {'state':'repair_complete', 'read_only':True, 'request_id':args.repair_request_id}
            repair_inactive(destination)
            if args.repair_mode in ('plan','observe'):
                return {'state':'repair_pending_recovery' if request else 'repair_planned',
                        'read_only':True, 'request_id':args.repair_request_id, 'hashes':observed()}
            if args.repair_mode == 'apply':
                if request is not None: raise Refusal('existing repair request must be observed, then explicitly recovered; never replay apply')
                unchanged()
                request = dict(anchor, old_identity=old_identity.decode(), producer=repair_producer())
                request_bytes = record(REPAIR_FILES[0], request)
            elif args.repair_mode == 'recover':
                if not request or not repair_producer_finished(request['producer']):
                    raise Refusal('recovery requires a verified terminated original repair producer')
            for fd,name,before,after,mode in ((unit_fd,destination.name,old,desired,unit_mode),
                    (directories[root/'service'],destination.name,old,desired,candidate_mode),
                    (bin_fd,'upgrade-identity.json',old_identity,new_identity,identity_mode)):
                unchanged();repair_inactive(destination)
                repair_replace(fd,name,before,after,mode,args.repair_request_id,recover=args.repair_mode=='recover')
            unchanged()
            if observed() != complete: raise Refusal('repaired file bindings did not converge')
            if reload_saved is None:
                repair_inactive(destination)
                reload_bytes = record(REPAIR_FILES[1], reload_record);reload_saved = reload_record
                checked(['systemctl','--user','daemon-reload'])
            # An existing intent is never replayed: observe whether the manager
            # already loaded the new definition. Unknown outcomes stay pending.
            if repair_inactive(destination)['UMask'] != '0077':
                raise Refusal('daemon-reload result is unknown or ineffective; observe, never replay')
            unchanged()
            if observed() != complete: raise Refusal('repair target changed after reload')
            record(REPAIR_FILES[2], {'request_id':args.repair_request_id,'state':'complete','hashes':complete})
            return {'state':'repair_complete','read_only':False,'request_id':args.repair_request_id,
                    'service_started':False,'activation_preserved':True,'historical_stage_preserved':True}
        finally:
            os.close(lock)


def parser():
    p=PrivateArgumentParser(description=__doc__)
    p.add_argument('--action',choices=['plan','stage','activate','repair-umask'],default='plan')
    p.add_argument('--role',choices=['gateway','agent'],required=True)
    p.add_argument('--package',default=str(Path(__file__).resolve().parent))
    for name in ('manifest-sha256','root','state-dir','config','public-origin','legacy-origin',
                 'service-user','service-group','signed-binary','signing-receipt'):
        p.add_argument('--'+name)
    p.add_argument('--repair-mode',choices=['plan','observe','apply','recover'],default='plan')
    for name in ('repair-request-id','expected-definition-sha256','expected-activation-sha256','expected-stage-sha256'):
        p.add_argument('--'+name)
    p.add_argument('--port',type=int,default=18788)
    p.add_argument('--new-auth-provisioned',action='store_true')
    return p


def main():
    args=None
    try:
        args=parser().parse_args()
        if args.action == 'repair-umask':
            print(json.dumps(repair_umask(args)))
            return 0
        p=plan(args)
        if args.action=='stage': stage(p)
        elif args.action=='activate': activate(p)
        print(json.dumps({'state':'activation_submitted' if args.action=='activate' else args.action+'_completed',
                          'plan':public_plan(p),'runtime_verified':False,
                          'service_started':None if args.action=='activate' else False,
                          'public_listener_created':False,'legacy_changed':False,
                          'credentials_created_or_copied':False}))
    except (ValueError,OSError,KeyError,TypeError,AttributeError,IndexError,subprocess.SubprocessError) as error:
        # Never dump configuration contents, auth values or platform command stderr.
        print(json.dumps({'state':'stopped','reason':str(error) if isinstance(error,Refusal)
                          else type(error).__name__,'action':args.action if args else 'unparsed',
                          'next':'inspect the new-instance inputs/request; do not replay an uncertain activation'}),file=sys.stderr)
        return 2
    return 0


if __name__=='__main__':
    sys.exit(main())
