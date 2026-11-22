-- Hermes Agent 的 state.db 脱敏样例（手写）。表结构取上游 persistence/sqlite/_schema_ddl.py（v33）里会话台用到的列，
-- 外加几列不用的，模拟「表比我们知道的多」。时间是秒（带小数）。
PRAGMA journal_mode = WAL;

CREATE TABLE schema_version (version INTEGER NOT NULL);
INSERT INTO schema_version VALUES (33);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    created_source TEXT,
    user_id TEXT,
    model TEXT,
    parent_session_id TEXT,
    started_at REAL NOT NULL,
    ended_at REAL,
    end_reason TEXT,
    message_count INTEGER DEFAULT 0,
    cwd TEXT,
    git_branch TEXT,
    git_repo_root TEXT,
    title TEXT,
    last_activity_at REAL,
    archived INTEGER NOT NULL DEFAULT 0,
    hidden INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(id),
    role TEXT NOT NULL,
    content TEXT,
    tool_call_id TEXT,
    tool_calls TEXT,
    tool_name TEXT,
    timestamp REAL NOT NULL,
    token_count INTEGER,
    active INTEGER NOT NULL DEFAULT 1
);

-- 1. TUI 里开的（gateway 记成 local，入口在 created_source），有标题、分支、工具调用，还有一条被 /rewind 回退的消息。
INSERT INTO sessions (id, source, created_source, model, started_at, cwd, git_branch, git_repo_root, title, last_activity_at)
VALUES ('20261120_101500_a1b2c3', 'local', 'tui', 'demo-model', 1795000000.25, '/work/api', 'main', '/work/api',
        '整理接口文档', 1795000300.5);
INSERT INTO messages (session_id, role, content, tool_calls, tool_name, timestamp, active) VALUES
 ('20261120_101500_a1b2c3', 'user', '接口文档按模块分目录', NULL, NULL, 1795000010, 1),
 ('20261120_101500_a1b2c3', 'assistant', '', '[{"id":"call_1","type":"function","function":{"name":"terminal","arguments":"{\"command\":\"ls docs/api\"}"}}]', NULL, 1795000020, 1),
 ('20261120_101500_a1b2c3', 'tool', 'orders.md
users.md', NULL, 'terminal', 1795000030, 1),
 ('20261120_101500_a1b2c3', 'assistant', '分成订单、用户两块了。', NULL, NULL, 1795000040, 1),
 ('20261120_101500_a1b2c3', 'user', '这条被回退了', NULL, NULL, 1795000050, 0),
 ('20261120_101500_a1b2c3', 'user', '旧链接要能跳过去', NULL, NULL, 1795000060, 1),
 ('20261120_101500_a1b2c3', 'assistant', '旧路径都加了重定向。', NULL, NULL, 1795000400.75, 1);

-- 2. 老版本 CLI 直接记 source=cli：cwd 空、退用 git_repo_root；没有标题、用第一句话（多模态 JSON）；没有 last_activity_at。
INSERT INTO sessions (id, source, created_source, started_at, cwd, git_repo_root, title)
VALUES ('20261121_080000_b2c3d4', 'cli', NULL, 1795100000, '', '/work/blog', NULL);
INSERT INTO messages (session_id, role, content, timestamp) VALUES
 ('20261121_080000_b2c3d4', 'user', '[{"type":"text","text":"把草稿里的图片压一遍"},{"type":"image_url","image_url":{"url":"data:,"}}]', 1795100005),
 ('20261121_080000_b2c3d4', 'assistant', '压好了，平均小了六成。', 1795100100);

-- 3. 消息平台（Telegram）的对话：不列。
INSERT INTO sessions (id, source, created_source, started_at, cwd, title)
VALUES ('20261121_090000_c3d4e5', 'telegram', 'telegram', 1795110000, '/work/api', '群里问的');
INSERT INTO messages (session_id, role, content, timestamp) VALUES
 ('20261121_090000_c3d4e5', 'user', '在吗', 1795110001);

-- 4. 子代理：不列。
INSERT INTO sessions (id, source, created_source, parent_session_id, started_at, cwd, title)
VALUES ('20261121_091000_d4e5f6', 'subagent', 'subagent', '20261120_101500_a1b2c3', 1795110600, '/work/api', '子任务');
INSERT INTO messages (session_id, role, content, timestamp) VALUES
 ('20261121_091000_d4e5f6', 'user', '查一下目录', 1795110601);

-- 5. hermes chat -q 一次性跑的：列，算自动运行。
INSERT INTO sessions (id, source, created_source, started_at, cwd, title)
VALUES ('20261121_100000_e5f6a7', 'oneshot', 'oneshot', 1795113600, '/work/api', '每天备份检查');
INSERT INTO messages (session_id, role, content, timestamp) VALUES
 ('20261121_100000_e5f6a7', 'user', '检查备份', 1795113601),
 ('20261121_100000_e5f6a7', 'assistant', '备份正常。', 1795113650);

-- 6. Hermes 里归档了的：不列。
INSERT INTO sessions (id, source, created_source, started_at, cwd, title, archived)
VALUES ('20261121_110000_f6a7b8', 'local', 'cli', 1795117200, '/work/api', '已归档', 1);
INSERT INTO messages (session_id, role, content, timestamp) VALUES
 ('20261121_110000_f6a7b8', 'user', '旧事', 1795117201);

-- 7. 压缩后续写：父行 end_reason=compression 不列，只列续写的那行。
INSERT INTO sessions (id, source, created_source, started_at, ended_at, end_reason, cwd, title)
VALUES ('20261121_120000_a7b8c9', 'local', 'cli', 1795120800, 1795121000, 'compression', '/work/shop', '修结算按钮');
INSERT INTO messages (session_id, role, content, timestamp) VALUES
 ('20261121_120000_a7b8c9', 'user', '结算按钮没反应', 1795120801);
INSERT INTO sessions (id, source, created_source, parent_session_id, started_at, cwd, title)
VALUES ('20261121_130000_b8c9d0', 'local', 'cli', '20261121_120000_a7b8c9', 1795121000, '/work/shop', '修结算按钮 #2');
INSERT INTO messages (session_id, role, content, timestamp) VALUES
 ('20261121_130000_b8c9d0', 'user', '接着修结算', 1795121001),
 ('20261121_130000_b8c9d0', 'assistant', '优惠券算完后重新启用按钮，修好了。', 1795121100);

-- 8. 启动了没说话就退出：不列。
INSERT INTO sessions (id, source, created_source, started_at, cwd)
VALUES ('20261121_140000_c9d0e1', 'local', 'tui', 1795124400, '/work/api');
