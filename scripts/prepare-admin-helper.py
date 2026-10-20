#!/usr/bin/env python3
"""Prepare a checksummed offline admin-helper bundle. Never installs or elevates."""
from __future__ import annotations
import argparse
import hashlib
import json
from pathlib import Path
import plistlib
import shutil
import struct
import uuid
import sys

# Managed-service preparation reuses this producer; it never includes installer scripts.
MANAGED_PROFILE = 'linux-managed-service-v1'
MANAGED_COMPONENT = 'octendra-admin-linux-prepared'

def prepare_managed(binary, output, device_id, account, uid, gid, home, source_commit, source_inputs_sha256):
    if binary.is_symlink() or not binary.is_file() or binary_machine(binary, 'linux') != 'x86_64':
        raise ValueError('Managed profile requires a regular Linux x86_64 ELF')
    device_id = str(uuid.UUID(device_id))
    if uid <= 0 or gid <= 0 or not account or len(account) >= 64 or not all(c.isascii() and (c.isalnum() or c in '_-') for c in account):
        raise ValueError('Invalid managed-service account')
    if not home.startswith('/') or any(p in ('', '.', '..') for p in home.split('/')[1:]) or any(c.isspace() or ord(c)<32 for c in home):
        raise ValueError('Invalid managed-service home')
    if len(source_commit)!=40 or len(source_inputs_sha256)!=64:
        raise ValueError('Missing pinned source identity')
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    shutil.copyfile(binary, output/'octendra-admin')
    (output/'octendra-admin').chmod(0o755)
    policy=dict(protocol=1,device_id=device_id,grant_id=str(uuid.uuid4()),allowed_uid=uid,
                allowed_gid=gid,home=home,platform='linux',enabled=False)
    grant=dict(protocol=1,device_id=device_id,grant_id=policy['grant_id'],enabled=False,
               not_before=0,expires_at=0,action='managed_service_restart',
               unit='octendra-code-agent.service',unit_sha256='0'*64,
               executable='/unconfirmed/octendra-code',executable_sha256='0'*64,
               agent_config='/unconfirmed/agent.json',agent_config_sha256='0'*64)
    for name,value in [('policy.json',policy),('service-grant.json',grant)]:
        (output/name).write_text(json.dumps(value,indent=2)+'\n')
        (output/name).chmod(0o600)
    root=Path(__file__).resolve().parents[1]
    (output/'service.template').write_bytes((root/'crates/octendra-admin/src/assets/linux-managed-helper.service').read_bytes())
    instructions = '''Prepared disabled data package. NOT installed, NOT activated, unsigned development artifact.
No install.py, shell installer, sudoers, DNS, service restart or automatic root action is included.

1. Independently verify manifest SHA-256 from the trusted preparation receipt; do not trust a replaced checksum next to a replaced binary.
2. An OS administrator must stage the exact verified ELF in root-owned, non-writable ancestors before executing it as root.
   Native bootstrap destination: /var/lib/octendra-admin/bin/octendra-admin (root:root 0755).
   Verify the staged ELF digest AGAIN with the trusted OS checksum utility before execution.
   Do not execute root code or a writable script directly from this package.
3. From that verified native executable, inspect-site reads the fixed service and enrolled identity only.
   /var/lib/octendra-admin/bin/octendra-admin linux-helper inspect-site --bundle <bundle> --manifest-sha256 <trusted-sha> --output <new-private-site.json>
4. Prepare one installation review with the ORIGINAL operation UUID and explicit validity/unattended scope:
   ... linux-helper prepare-install --bundle <bundle> --manifest-sha256 <trusted-sha> --request-id <original-operation-uuid> --site <site.json> --not-before <unix-seconds> --expires-at <unix-seconds> --allow-unattended --output <new-review.json>
5. Review exact device/account, fixed resources, hashes, validity, out-of-band recovery and revocation. Accept the review's plan_sha256 explicitly:
   ... linux-helper render-approved --bundle <bundle> --plan <review.json> --accept-plan-sha256 <approved-plan-sha> --output <new-private-render-directory>
   This exports data only; it does not copy to system directories or start a service.
6. Separate OS administrator installation is required for:
   policy.json -> /var/lib/octendra-admin/policy.json (root:root 0600)
   service-grant.json -> /var/lib/octendra-admin/service-grant.json (root:root 0600)
   service.template -> /etc/systemd/system/octendra-admin.service (root:root 0644)
   receipts -> /var/lib/octendra-admin/receipts (root:root 0700, preserve existing evidence)
   helper binary as above; runtime socket /run/octendra-admin/helper.sock.
   Installing the PACKAGE templates keeps both policy and grant disabled. Approved rendered files are a separate, explicit authorization decision.
   The administrator must independently verify each source and destination digest; refuse symlinks, writable ancestors or existing unknown installations.
   Service manager reload/start/enable of octendra-admin.service is a separate OS-authorized action, never done by this preparation tool.
7. Ordinary authorized UID uses the ORIGINAL operation UUID:
   octendra-admin linux-helper status --device-id <device-uuid>
   octendra-admin linux-helper plan --device-id <device-uuid> --request-id <original-operation-uuid> --output <new-plan.json>
   octendra-admin linux-helper apply --device-id <device-uuid> --plan <plan.json> --accept-plan-sha256 <approved-action-plan-sha>
   octendra-admin linux-helper receipt --device-id <device-uuid> --request-id <same-operation-uuid>
   octendra-admin linux-helper revoke --device-id <device-uuid>
   Unknown outcomes require the original receipt; never change UUID or replay.
Revoke persists the original helper revocation marker; do not delete it to re-enable. Preserve receipts on uninstall.
Only octendra-code-agent.service restart is supported. No DNS, sing-box, reboot, package install or arbitrary shell.
Other platforms need their own signed/ACL-verified adapter; this Linux package is not cross-platform activation evidence.
'''
    (output/'INSTALL.txt').write_text(instructions)
    names=['octendra-admin','policy.json','service-grant.json','service.template','INSTALL.txt']
    manifest=dict(protocol=2,profile=MANAGED_PROFILE,helper_version='0.1.0',platform='linux',machine='x86_64',
                  device_id=device_id,account=account,source_commit=source_commit,
                  source_inputs_sha256=source_inputs_sha256,validation_kind='unsigned-dev',
                  status='prepared_disabled',default_enabled=False,files={n:sha(output/n) for n in names})
    (output/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
    identity=sha(output/'manifest.json')
    (output/'MANIFEST.sha256').write_text(identity+'  manifest.json\n')
    return dict(output=str(output),manifest_sha256=identity,installed=False,**manifest)

def publish_managed(args):
    import fcntl, os, subprocess, tarfile
    import release_current as current
    root=current.primary(Path(__file__).resolve().parents[1])
    default=root/'dist/octendra-admin-linux-current'
    archive=root/'dist/octendra-admin-linux-current.tgz'
    if args.platform not in (None,'linux') or args.binary is not None or (args.output is not None and args.output.resolve()!=default):
        raise ValueError('Managed profile builds in primary main and publishes only its fixed Linux current group')
    current.require_current_names(current.preflight(root))
    lock=root/'.rust-storage/admin-helper-producer.lock'
    with lock.open('a+b') as held:
        fcntl.flock(held,fcntl.LOCK_EX|fcntl.LOCK_NB)
        env=current.snapshot_evidence(None,root)
        defaults=[default,archive]
        argv=['--component',MANAGED_COMPONENT,'--producer-pid',str(os.getpid())]
        for p in defaults:argv+=['--file',str(p)]
        (root/'dist').mkdir(exist_ok=True)
        admitted=current.invoke(root,'release-admit',*argv)
        if admitted.get('admission')!='reserved':
            raise ValueError('Managed preparation current group was not admitted')
        stages=[Path(p) for p in admitted['staged_files']]
        if stages != [p.with_name('.'+p.name+'.pending') for p in defaults]:
            current.abort(root)
            raise ValueError('Unexpected current staging paths')
        try:
            run=lambda command:subprocess.run([sys.executable,str(root/'scripts/dev-verify.py'),'--exec',*command],cwd=root,check=True)
            run(['cargo','build','-p','octendra-admin','--locked','--offline'])
            run(['cargo','zigbuild','-p','octendra-admin','--release','--locked','--offline','--target','x86_64-unknown-linux-musl'])
            current.check_inputs(root,env)
            # Cargo metadata is read through the same pinned toolchain environment.
            import importlib.util
            spec=importlib.util.spec_from_file_location('admin_dev_verify',root/'scripts/dev-verify.py')
            dev=importlib.util.module_from_spec(spec);spec.loader.exec_module(dev)
            toolchain=dev.find_toolchain()
            target=current.established_target(root,toolchain['env'])
            result=prepare_managed(target/'x86_64-unknown-linux-musl/release/octendra-admin',stages[0],
                args.device_id,args.account,args.uid,args.gid,args.home,
                subprocess.check_output(['git','rev-parse','HEAD'],cwd=root,text=True).strip(),env['snapshot_id'])
            checked=json.loads(subprocess.check_output([str(target/'debug/octendra-admin'),'linux-helper','verify-package',
                '--bundle',str(stages[0]),'--manifest-sha256',result['manifest_sha256']],cwd=root,text=True))
            if checked['default_enabled'] is not False or checked['installed'] is not False or checked['grants_authority'] is not False:
                raise ValueError('Native package verification did not confirm disabled preparation')
            receipt=dict(protocol=1,validation_kind='unsigned-dev',native_verification=checked,
                source_commit=result['source_commit'],source_inputs_sha256=env['snapshot_id'],
                linux_native_execution=False,root_installation=False,root_activation=False)
            (stages[0]/'prepared-validation.json').write_text(json.dumps(receipt,indent=2)+'\n')
            with tarfile.open(stages[1],'w:gz') as tar:
                for p in sorted(stages[0].iterdir()):
                    if p.is_symlink() or not p.is_file():raise ValueError('Non-regular package member')
                    tar.add(p,arcname=default.name+'/'+p.name,recursive=False)
            with tarfile.open(stages[1],'r:gz') as tar:
                members=tar.getmembers()
                if len(members)!=len(list(stages[0].iterdir())):raise ValueError('Archive member count mismatch')
                for m in members:
                    name=Path(m.name)
                    if not m.isfile() or name.parts[0]!=default.name or len(name.parts)!=2:
                        raise ValueError('Unsafe prepared archive member')
                    data=tar.extractfile(m).read()
                    if hashlib.sha256(data).hexdigest()!=sha(stages[0]/name.name):raise ValueError('Archive roundtrip mismatch')
            current.check_inputs(root,env)
            validation=stages[0]/'prepared-validation.json'
            published=current.invoke(root,'release-publish',*argv,'--validation-kind','unsigned-dev',
                '--validation-file',str(validation),'--validation-digest',sha(validation),
                '--expected-sha256',current.public_api().release_signature(stages[0]),
                '--expected-sha256',sha(stages[1]))
            if published.get('current_committed') is not True:raise ValueError('Current preparation publication not confirmed')
            return dict(output=str(default),archive=str(archive),manifest_sha256=result['manifest_sha256'],
                binary_sha256=result['files']['octendra-admin'],installed=False,default_enabled=False,
                publication=published)
        except BaseException:
            current.abort(root)
            raise



def sha(path: Path) -> str:
    h = hashlib.sha256()
    with path.open('rb') as f:
        for part in iter(lambda: f.read(1024 * 1024), b''):
            h.update(part)
    return h.hexdigest()


def binary_machine(path: Path, target: str) -> str:
    with path.open('rb') as f:
        header = f.read(64)
    if target == 'linux' and header[:4] == b'\x7fELF' and len(header) >= 20:
        order = '<' if header[5] == 1 else '>'
        machine = struct.unpack(order+'H', header[18:20])[0]
        return {62: 'x86_64', 183: 'aarch64'}[machine]
    if target == 'macos' and header[:4] == b'\xcf\xfa\xed\xfe':
        cpu = struct.unpack('<I', header[4:8])[0]
        return {0x0100000C: 'arm64', 0x01000007: 'x86_64'}[cpu]
    raise ValueError('Binary is not a supported executable for this platform')


def make_service(target: str) -> bytes:
    if target == 'macos':
        state = '/Library/Application Support/OctendraAdmin'
        config = dict(Label='fun.hackerlife.octendra.admin', RunAtLoad=True, KeepAlive=True,
                      UserName='root', GroupName='wheel', Umask=0o077,
                      ProgramArguments=['/Library/PrivilegedHelperTools/fun.hackerlife.octendra.admin',
                                        'serve', '--policy', state+'/policy.json',
                                        '--state-dir', state+'/receipts'],
                      StandardOutPath=state+'/helper.stdout.log',
                      StandardErrorPath=state+'/helper.stderr.log',
                      ThrottleInterval=10, ExitTimeOut=130)
        # The runtime directory must be recreated after reboot; launchd does not offer RuntimeDirectory.
        # Use the persistent protected directory for this socket instead; it is not world-writable.
        config['ProgramArguments'] += ['--socket', state+'/helper.sock']
        return plistlib.dumps(config, sort_keys=True)
    return b'''[Unit]
Description=Restricted Octendra administration helper
After=local-fs.target

[Service]
Type=simple
User=root
Group=root
ExecStart=/var/lib/octendra-admin/bin/octendra-admin serve --policy /var/lib/octendra-admin/policy.json --state-dir /var/lib/octendra-admin/receipts
Restart=on-failure
RestartSec=5
RuntimeDirectory=octendra-admin
RuntimeDirectoryMode=0755
UMask=0077
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=full
ReadWritePaths=/etc/systemd/system /var/lib/octendra-admin /run/octendra-admin
ProtectHome=read-only
RestrictAddressFamilies=AF_UNIX
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
TimeoutStopSec=130

[Install]
WantedBy=multi-user.target
'''


UNINSTALL = '''#!/usr/bin/env python3
"""Remove only the helper; keep policy, receipts and quarantined legacy files for audit."""
import sys
if not sys.flags.isolated:raise SystemExit("Use python3 -I for this administrator script.")
import argparse, os, pathlib, platform, subprocess
p=argparse.ArgumentParser();p.add_argument('--apply',action='store_true');a=p.parse_args()
mac=platform.system()=='Darwin'
service=pathlib.Path('/Library/LaunchDaemons/fun.hackerlife.octendra.admin.plist' if mac else '/etc/systemd/system/octendra-admin.service')
binary=pathlib.Path('/Library/PrivilegedHelperTools/fun.hackerlife.octendra.admin' if mac else '/var/lib/octendra-admin/bin/octendra-admin')
print('Remove helper only; keep receipts/backups; never restore old EasyTier automatically.')
if not a.apply: print(service,binary,sep='\\n');raise SystemExit(0)
if os.geteuid()!=0:raise SystemExit('Separate administrator authorization is required.')
env={'PATH':'/usr/bin:/bin:/usr/sbin:/sbin','LC_ALL':'C'}
command=['/bin/launchctl','bootout','system/fun.hackerlife.octendra.admin'] if mac else ['/usr/bin/systemctl','disable','--now','octendra-admin.service']
subprocess.run(command,check=True,env=env,stdin=subprocess.DEVNULL,timeout=150)
for path in (service,binary):
    if path.is_symlink():raise SystemExit('Refuse symlink: '+str(path))
    path.unlink()
if not mac:subprocess.run(['/usr/bin/systemctl','daemon-reload'],check=True,env=env,timeout=30)
'''


def prepare(binary: Path, output: Path, target: str, device_id: str, account: str,
            uid: int, gid: int, home: str) -> dict:
    if binary.is_symlink() or not binary.is_file():
        raise ValueError('Provide a built regular executable')
    device_id = str(uuid.UUID(device_id))
    if uid <= 0 or gid < 0 or not account or not home.startswith('/') or any(
            part in ('', '.', '..') for part in home.split('/')[1:]) or any(ord(c)<32 for c in home):
        raise ValueError('Invalid account grant')
    machine = binary_machine(binary, target)
    output.mkdir(parents=True, exist_ok=False)
    shutil.copyfile(binary, output/'octendra-admin')
    (output/'octendra-admin').chmod(0o755)
    policy = dict(protocol=1, device_id=device_id, grant_id=str(uuid.uuid4()),
                  allowed_uid=uid, allowed_gid=gid, home=home, platform=target, enabled=True)
    (output/'policy.json').write_text(json.dumps(policy, indent=2)+'\n')
    (output/'service.template').write_bytes(make_service(target))
    shutil.copyfile(Path(__file__).with_name('admin-helper-install.py'), output/'install.py')
    (output/'uninstall.py').write_text(UNINSTALL)
    names = ['octendra-admin', 'policy.json', 'service.template', 'install.py', 'uninstall.py']
    manifest = dict(protocol=1, helper_version='0.1.0', status='prepared_not_installed',
                    platform=target, machine=machine, device_id=device_id, account=account,
                    files={n: sha(output/n) for n in names})
    (output/'manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
    identity = sha(output/'manifest.json')
    (output/'MANIFEST.sha256').write_text(identity+'  manifest.json\n')
    (output/'INSTALL.txt').write_text(
        'Prepared candidate, NOT installed. No cleanup runs during installation.\n'
        'First verify this manifest digest through an independent trusted record:\n'+identity+'\n'
        'Preview without privileges: python3 -I install.py\n'
        'In a separately authorized administrator session:\n'
        'python3 -I install.py --apply --grant-legacy-cleanup --agent-config /path/to/existing/agent.json --accept-manifest-sha256 '+identity+'\n'
        'Do not transmit administrator passwords through Octendra.\n'
        'After install, use the normal authorized user and the shared helper CLI.\n'
        'A live network alone does not create an administrator authorization.\n')
    return dict(output=str(output), manifest_sha256=identity, installed=False, **manifest)


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--profile',choices=['legacy-cleanup','managed-service'],default='legacy-cleanup')
    p.add_argument('--binary',type=Path);p.add_argument('--output',type=Path)
    p.add_argument('--platform',choices=['macos','linux']);p.add_argument('--device-id',required=True)
    p.add_argument('--account',required=True);p.add_argument('--uid',type=int,required=True)
    p.add_argument('--gid',type=int,required=True);p.add_argument('--home',required=True)
    a=p.parse_args()
    if a.profile=='managed-service':
        print(json.dumps(publish_managed(a),indent=2))
    else:
        if a.binary is None or a.output is None or a.platform is None:p.error('Legacy profile requires --binary, --output and --platform')
        print(json.dumps(prepare(a.binary,a.output,a.platform,a.device_id,a.account,a.uid,a.gid,a.home),indent=2))
if __name__=='__main__':main()
