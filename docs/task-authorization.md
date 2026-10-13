# Connection authorization and device-local sudo

## Current source behavior

New code reads, writes and terminal operations use the authenticated connection's
current account scopes, enrolled-device scopes and Agent local permissions.
There is no TaskGrant creation, expiry, renewal or command-business approval.
OAuth token-family revocation, owner/device checks, local allow flags, code path
boundaries, schema validation and unknown-operation/idempotency guards remain.
Terminal commands have local-user authority outside code roots; cwd is not a
filesystem sandbox.

task_id is a correlation label. authorization_version is accepted only for
compatibility and grants no authority. The retired owner form returns 410 after
authentication. task_resume only observes its original request or operation.
Existing queued task bindings stay held; upgrade does not dispatch previously
blocked work. Historical rows and receipts are retained. Existing dispatched
operations keep their original observation semantics.

The task-authorization protocol marker remains 2 solely for stored-history and
existing updater compatibility. The manifest separately advertises connection
authorization and retirement of grants. This source change is not evidence of
Gateway, Agent, connector-schema or fleet deployment.

## Optional sudo execution

On macOS/Linux, an Agent advertises local_sudo_v1. terminal_exec accepts
sudo=true for a noninteractive shell; the default remains ordinary local-user
execution. The Gateway rejects it when the selected Agent has not advertised
support. Interactive PTYs and the legacy maintenance adapter cannot use this
field. Windows has no provider in this implementation.

The Agent first invokes the system /usr/bin/sudo with -n -v. If authentication is
needed, it retrieves a credential registered locally by the user, invokes sudo
-S -p "" -v through a private stdin pipe, closes that pipe, and checks the actual
exit status. Only then does it start sudo -n -- with the configured shell once.
The business command has stdin EOF and never receives password bytes, including
when cached or NOPASSWD policy avoids reading a password. No prompt matching,
password argument, custom timed grant or command classifier is involved.

This follows the machine's existing sudo timestamp and sudoers policy. A zero
timestamp lifetime, requiretty, unavailable/locked credential store, denied sudo
policy or expired cache can prevent execution. These conditions produce a
visible failure; they never trigger automatic command replay or sudoers edits.
Credential removal prevents future lookup; existing OS authentication caches and
already running processes follow OS policy. See the [sudo timestamp manual](https://www.sudo.ws/docs/man/sudoers_timestamp.man/).

The credential identity is service fun.hackerlife.remote-hosts.sudo and account
<canonical Code Agent device UUID>:<effective OS uid>. macOS uses that user's
Keychain generic-password entry; Linux uses that login session's Secret Service
through /usr/bin/secret-tool. The user registers/rotates/deletes it using local OS
interfaces. No MCP tool accepts the password or selects another credential.
Keychain access may still need direct OS consent. This does not import an SSH
connector vault or create a credential silently.

Authentication failures return command_started=false, a safe error code and
validation exit status. Command execution uses the existing Agent-owned terminal
capture, process identity, combined stdout/stderr, cursor and real exit status.
A privileged command writing a separate root-only file does not make that file
readable; callers should use captured output where required. Cancellation follows
OS signalling permissions and does not promise to kill every root descendant.

## Client confirmations and rollout

Annotations reflect actual behavior. terminal_read and the retired observation
tool are read-only; arbitrary terminal_exec remains write-capable and open-world.
Gemini App's write confirmation is controlled by Google, while Gemini CLI and
Codex have their own approval settings. Server metadata cannot promise unattended
host approval or bypass an existing rejection. Google's [Gemini custom-app documentation](https://support.google.com/gemini/answer/17209137) explicitly requires manual confirmation for write actions.

Before production deployment, review the exact Gateway/Agent targets, connection
scopes, removal of legacy TaskGrant restrictions, treatment of held operations,
and the optional root-shell capability for a locally registered credential.
The implementation does not register real passwords, alter sudoers, enable a
privileged daemon or deploy itself. Keep verification in the main checkout and
existing shared target using the fixed build runner and targeted tests.
