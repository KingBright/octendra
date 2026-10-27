"""Execute the actual Rust queue SQL with artificial in-memory SQLite rows."""
import json
from pathlib import Path
import re
import sqlite3
import unittest

ROOT = Path(__file__).resolve().parents[2]


class DeviceScopeDispatchTests(unittest.TestCase):
    def test_candidate_and_claim_enforce_scope_on_new_and_previously_dispatched_jobs(self):
        source = (ROOT / 'crates/octendra-code/src/job_dispatch.rs').read_text()
        match = re.search(r'\(\) => \{ r#"(.*?)"# \};', source, re.S)
        self.assertIsNotNone(match, 'test must execute the production candidate_sql literal')
        sql = match.group(1)
        roles = {
            'code:read': ['workspace_open', 'code_read', 'code_list', 'code_search',
                          'code_symbols', 'code_diff', 'file_download', 'terminal_read', 'workspace_context'],
            'code:write': ['code_apply_edits', 'change_resume', 'files_sync', 'file_upload', 'workspace_gc'],
            'terminal:exec': ['terminal_exec', 'terminal_input', 'terminal_cancel'],
            'desktop:observe': ['desktop_observer', 'desktop_frame'],
        }
        expectations = [(tool, [scope]) for scope, tools in roles.items() for tool in tools]
        expectations += [('desktop_worker', ['desktop:observe', 'desktop:input']), ('unsupported-tool', [])]
        grants = [[], ['code:read'], ['code:write'], ['terminal:exec'], ['desktop:observe'],
                  ['desktop:input'], ['desktop:observe', 'desktop:input'], [None],
                  [*roles, 'desktop:input']]
        checked = 0
        for tool, required in expectations:
            for granted in grants:
                for state in ('queued', 'dispatched'):
                    with self.subTest(tool=tool, granted=granted, state=state):
                        with sqlite3.connect(':memory:') as db:
                            db.executescript('CREATE TABLE jobs(id TEXT,device TEXT,request TEXT,updated INTEGER,state TEXT);'
                                             'CREATE TABLE kv(kind TEXT,key TEXT,value TEXT,expires INTEGER);')
                            request = {'tool': tool, 'arguments': {'workspace_id': 'device:workspace'}}
                            db.execute('INSERT INTO jobs VALUES(?,?,?,?,?)',
                                       ('job', 'device', json.dumps(request), 0, state))
                            db.execute('INSERT INTO kv VALUES(?,?,?,?)',
                                       ('online', 'device', '{"hello":{"session":"session"}}', 9999999999))
                            params = ['device', 'device', 70, '[]', json.dumps(granted),
                                      '["read","write","terminal","transfer","control"]',
                                      False, '[]', '[]', 'device', 'session']
                            allowed = bool(required) and set(required).issubset(granted)
                            self.assertEqual(bool(db.execute(sql, params).fetchone()), allowed)
                            claim = "UPDATE jobs SET state='dispatched',updated=? WHERE id=(" + sql + ') RETURNING id'
                            self.assertEqual(bool(db.execute(claim, [100, *params]).fetchone()), allowed)
                            checked += 1
        self.assertEqual(checked, 378)


if __name__ == '__main__':
    unittest.main()
