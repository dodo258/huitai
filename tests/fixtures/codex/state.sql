-- 手写脱敏的 Codex 状态库（threads 表只保留会话台读的列）。@ROOT@ 在测试里换成临时目录。
PRAGMA journal_mode = wal;
CREATE TABLE threads (
    id TEXT PRIMARY KEY,
    rollout_path TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    source TEXT NOT NULL,
    cwd TEXT NOT NULL,
    title TEXT NOT NULL,
    archived INTEGER NOT NULL DEFAULT 0,
    git_branch TEXT,
    first_user_message TEXT NOT NULL DEFAULT '',
    updated_at_ms INTEGER,
    name TEXT
);
INSERT INTO threads VALUES
 ('cccccccc-0000-4000-8000-000000000003', '@ROOT@/sessions/2026/11/02/rollout-2026-11-02T10-00-00-cccccccc-0000-4000-8000-000000000003.jsonl',
  1795160000, 1795169100, 'vscode', '/Users/demo/projects/api', '原始标题', 0, 'main', '接口加分页', 1795169100000, '库里的名字'),
 ('dddddddd-0000-4000-8000-00000000000a', '@ROOT@/archived_sessions/rollout-x.jsonl',
  1795160000, 1795160000, 'cli', '/Users/demo/projects/api', '已归档', 1, NULL, '已归档', 1795160000000, NULL),
 ('eeeeeeee-0000-4000-8000-00000000000b', '@ROOT@/sessions/2026/11/02/rollout-gone.jsonl',
  1795160000, 1795160000, '{"subagent":{"thread_spawn":{"parent_thread_id":"cccccccc-0000-4000-8000-000000000003"}}}', '/Users/demo/projects/api', '子代理', 0, NULL, '子代理', 1795160000000, NULL),
 ('ffffffff-0000-4000-8000-000000000006', '@ROOT@/sessions/2026/10/01/rollout-2026-10-01T00-00-00-ffffffff-0000-4000-8000-000000000006.jsonl',
  1795170000, 1795172400, 'exec', '/Users/demo', '', 0, NULL, '只有第一句话', NULL, NULL);
