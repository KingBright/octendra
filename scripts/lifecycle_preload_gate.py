"""Short maintenance admission gate; stdlib only, never imports the helper.

Vendor this file unchanged beside a project's wrapper. Admission holds SH on the
existing skills directory for the whole helper operation; begin/verify/end use
EX on that same directory inode. A durable hold never expires with a PID. This
gates adopted callers only; it does not assert complete consumer quiescence.
"""
import argparse
import contextlib
import contextvars
import fcntl
import functools
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys

PROTOCOL = 'lifecycle-preload-gate-v1'
DIRECTORY = '.rust-project-lifecycle-maintenance'
STATE = 'state.json'
PENDING = '.state.json.pending'
_ACTIVE = contextvars.ContextVar('lifecycle_preload_admission', default=None)


class Refusal(RuntimeError):
    pass


def need(ok, code):
    if not ok:raise Refusal(code)


def skills_root():
    home=Path.home()
    need(home.is_absolute() and '..' not in home.parts, 'gate_unsafe_home')
    # The trusted account home may itself be a platform-managed alias. Resolve
    # only that prefix: .codex, skills and all control members stay no-follow.
    return home.resolve(strict=True)/'.codex/skills'


def encoded(value):
    return (json.dumps(value, sort_keys=True, separators=(',', ':'))+'\n').encode()


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def parsed(raw):
    def pairs(rows):
        result={}
        for key,value in rows:
            need(key not in result, 'gate_unknown_duplicate_key');result[key]=value
        return result
    try:return json.loads(raw, object_pairs_hook=pairs)
    except (ValueError, UnicodeError) as error:raise Refusal('gate_unknown_json') from error


class Gate:
    def __init__(self, root=None):
        self.default_root=root is None
        self.root=Path(root) if root is not None else skills_root()

    def check_root(self, identity):
        current=self.root.stat(follow_symlinks=False)
        need((current.st_dev,current.st_ino)==identity
             and (not self.default_root or skills_root()==self.root), 'gate_root_changed')

    @contextlib.contextmanager
    def locked(self, exclusive=False):
        need(self.root.is_absolute() and '..' not in self.root.parts, 'gate_unsafe_root')
        fd=os.open('/',os.O_RDONLY|os.O_DIRECTORY)
        try:
            for name in self.root.parts[1:]:
                child=os.open(name,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,dir_fd=fd)
                os.close(fd);fd=child
            info=os.fstat(fd)
            need(info.st_uid==os.getuid() and not stat.S_IMODE(info.st_mode)&0o022, 'gate_unsafe_root_owner_mode')
            try:fcntl.flock(fd,(fcntl.LOCK_EX if exclusive else fcntl.LOCK_SH)|fcntl.LOCK_NB)
            except BlockingIOError as error:raise Refusal('gate_busy') from error
            identity=(info.st_dev,info.st_ino)
            self.check_root(identity)
            yield fd,identity
            self.check_root(identity)
        finally:
            os.close(fd)

    def read(self, parent):
        try:directory=os.open(DIRECTORY,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,dir_fd=parent)
        except FileNotFoundError:return None,'absent'
        try:
            info=os.fstat(directory)
            need(info.st_uid==os.getuid() and stat.S_IMODE(info.st_mode)==0o700, 'gate_unknown_directory_identity')
            need(set(os.listdir(directory))=={STATE}, 'gate_unknown_partial_or_extra_files')
            fd=os.open(STATE,os.O_RDONLY|os.O_NOFOLLOW,dir_fd=directory)
            try:
                before=os.fstat(fd)
                need(stat.S_ISREG(before.st_mode) and before.st_nlink==1 and before.st_uid==os.getuid()
                     and stat.S_IMODE(before.st_mode)==0o600 and before.st_size<=8192, 'gate_unknown_state_identity')
                raw=os.read(fd,8193);after=os.fstat(fd)
                need(len(raw)==before.st_size and (before.st_dev,before.st_ino,before.st_size,before.st_mtime_ns)==
                     (after.st_dev,after.st_ino,after.st_size,after.st_mtime_ns), 'gate_state_changed')
            finally:os.close(fd)
        finally:os.close(directory)
        value=parsed(raw)
        need(isinstance(value,dict) and set(value)=={'schema','protocol','window_id','state','revision','previous_sha256','scope_sha256'}
             and type(value['schema']) is int and value['schema']==1 and value['protocol']==PROTOCOL and value['state'] in {'hold','open'}
             and isinstance(value['window_id'],str) and re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_-]{0,79}',value['window_id'])
             and type(value['revision']) is int and value['revision']>=0
             and isinstance(value['scope_sha256'],str) and re.fullmatch('[0-9a-f]{64}',value['scope_sha256'])
             and (value['previous_sha256'] is None if value['revision']==0 else
                  isinstance(value['previous_sha256'],str) and re.fullmatch('[0-9a-f]{64}',value['previous_sha256'])),
             'gate_unknown_schema')
        return value,sha(raw)

    @contextlib.contextmanager
    def admission(self):
        with self.locked() as (parent,identity):
            value,pin=self.read(parent)
            need(value is None or value['state']=='open', 'gate_held')
            token=_ACTIVE.set((self, parent, identity, pin))
            try:yield
            finally:_ACTIVE.reset(token)

    def change(self, action, window_id, expected, scope_sha256=None):
        need(action in {'begin','end'}, 'gate_unknown_action')
        need(isinstance(window_id,str) and re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_-]{0,79}',window_id), 'gate_invalid_window')
        with self.locked(exclusive=True) as (parent,_):
            old,pin=self.read(parent)
            need(expected==pin, 'gate_cas_mismatch')
            if action=='begin':
                need(isinstance(scope_sha256,str) and re.fullmatch('[0-9a-f]{64}',scope_sha256), 'gate_invalid_scope')
                if old and old['state']=='hold':
                    need(old['window_id']==window_id and old['scope_sha256']==scope_sha256, 'gate_other_window_held')
                    return self.report(old,pin)
            else:
                need(old is not None and old['window_id']==window_id, 'gate_window_mismatch')
                # Standalone end cannot release a pending/unknown installer.
                # The eventual public updater must settle/read back its original
                # transaction before integrating an explicit end operation.
                need(not any((self.root/n).exists() or (self.root/n).is_symlink() for n in
                    ('.rust-project-lifecycle-update.json','.rust-project-lifecycle-update.json.pending',
                     '.rust-project-lifecycle-update.stage','.rust-project-lifecycle-update.backup',
                     '.rust-project-lifecycle-update.committed')), 'gate_update_transaction_requires_original_controller')
                if old['state']=='open':return self.report(old,pin)
            new=dict(schema=1,protocol=PROTOCOL,window_id=window_id,state='hold' if action=='begin' else 'open',
                     revision=old['revision']+1 if old else 0,previous_sha256=pin if old else None,
                     scope_sha256=scope_sha256 if action=='begin' else old['scope_sha256'])
            if old is None:
                os.mkdir(DIRECTORY,0o700,dir_fd=parent);os.fsync(parent)
            directory=os.open(DIRECTORY,os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW,dir_fd=parent)
            try:
                fd=os.open(PENDING,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600,dir_fd=directory)
                try:
                    os.fchmod(fd,0o600)
                    with os.fdopen(fd,'wb',closefd=False) as stream:
                        stream.write(encoded(new));stream.flush();os.fsync(fd)
                finally:os.close(fd)
                os.fsync(directory)
                os.replace(PENDING,STATE,src_dir_fd=directory,dst_dir_fd=directory);os.fsync(directory)
            finally:os.close(directory)
            actual,new_pin=self.read(parent);need(actual==new,'gate_readback_unknown')
            return self.report(actual,new_pin)

    @staticmethod
    def report(value,pin):
        return dict(protocol=PROTOCOL,state=value['state'] if value else 'legacy-open',state_sha256=pin,
                    window_id=value['window_id'] if value else None,scope_sha256=value['scope_sha256'] if value else None,
                    maintenance_ready=False,coverage='adopted-preload-callers-only')

    def verify(self,window_id,expected):
        with self.locked(exclusive=True) as (parent,_):
            value,pin=self.read(parent)
            need(value is not None and value['state']=='hold' and value['window_id']==window_id and pin==expected,
                 'gate_hold_not_verified')
            return dict(self.report(value,pin),read_only=True)


@contextlib.contextmanager
def admission():
    try:
        with Gate().admission():yield
    except (OSError, ValueError) as error:raise Refusal('gate_unavailable_or_unsafe') from error


def require_admitted():
    active=_ACTIVE.get();need(active is not None, 'gate_scoped_admission_required')
    gate,parent,identity,pin=active
    current=os.fstat(parent)
    gate.check_root(identity)
    need((current.st_dev,current.st_ino)==identity and gate.read(parent)[1]==pin, 'gate_admission_changed')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action',choices=['begin','verify','end'])
    parser.add_argument('--window-id',required=True)
    parser.add_argument('--expected-state-sha256',required=True)
    parser.add_argument('--scope-sha256')
    args=parser.parse_args()
    try:
        gate=Gate()
        result=(gate.verify(args.window_id,args.expected_state_sha256) if args.action=='verify' else
                gate.change(args.action,args.window_id,args.expected_state_sha256,args.scope_sha256))
        print(json.dumps(result));return 0
    except (Refusal,OSError,ValueError) as error:
        print(json.dumps(dict(mode='refused',error=str(error),artifacts_preserved=True)),file=sys.stderr);return 2


if __name__=='__main__':sys.exit(main())
