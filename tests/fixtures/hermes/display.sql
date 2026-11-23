-- Hermes Agent 较新版本 state.db 的脱敏样例（手写），覆盖 state.sql 没有的列：压缩归档（compacted）、显示位置
-- （display_order）、只给模型看（display_metadata.model_only）、消息 uid、正文修订号（transcript_revision 和
-- 上游 _schema_ddl.py 里那个触发器）。行号和显示位置都手写，不用上游的回填触发器。时间是秒。
PRAGMA journal_mode = WAL;

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    created_source TEXT,
    parent_session_id TEXT,
    started_at REAL NOT NULL,
    ended_at REAL,
    end_reason TEXT,
    cwd TEXT,
    git_branch TEXT,
    git_repo_root TEXT,
    title TEXT,
    title_source TEXT,
    last_activity_at REAL,
    archived INTEGER NOT NULL DEFAULT 0,
    hidden INTEGER NOT NULL DEFAULT 0,
    transcript_revision INTEGER NOT NULL DEFAULT 0
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
    _compressed_summary INTEGER NOT NULL DEFAULT 0,
    active INTEGER NOT NULL DEFAULT 1,
    compacted INTEGER NOT NULL DEFAULT 0,
    display_metadata TEXT,
    display_identity BLOB,
    display_order INTEGER,
    message_uid TEXT
);

CREATE TRIGGER messages_transcript_revision_update
AFTER UPDATE OF role, content, tool_calls, tool_call_id, tool_name ON messages
BEGIN
    UPDATE sessions SET transcript_revision = transcript_revision + 1 WHERE id = new.session_id;
END;

-- 1. 带图片的消息：内容是 NUL + "json:" + JSON（上游 _encode_content）。没有标题，用第一句有字的话；
--    第一句只有图片，跳过。
INSERT INTO sessions (id, source, created_source, started_at, cwd, title, last_activity_at)
VALUES ('20261122_090000_aa0001', 'local', 'tui', 1795280000, '/work/web', NULL, 1795280100);
INSERT INTO messages (id, session_id, role, content, timestamp, display_order) VALUES
 (1, '20261122_090000_aa0001', 'user',
  char(0) || 'json:' || '[{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}]',
  1795280010, 1),
 (2, '20261122_090000_aa0001', 'user',
  char(0) || 'json:' || '[{"type":"text","text":"截图里的提交按钮为什么是灰的"},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgo="}}]',
  1795280020, 2),
 (3, '20261122_090000_aa0001', 'assistant', '表单校验没过，按钮被禁用了。', 1795280030, 3),
 -- 字面就以这个前缀开头的文字，上游包成 JSON 字符串存。
 (4, '20261122_090000_aa0001', 'user', char(0) || 'json:' || '"照着改完再截一张"', 1795280040, 4);

-- 2. 压缩过的长对话：压缩前的原话归档成 active=0, compacted=1，仍显示；压缩摘要只给模型看；压缩时留下的最后
--    两句复制成新行（共用 display_order）；/rewind 回退的那句 active=0, compacted=0，不显示；工具返回归档后
--    又复制了一份（同 message_uid、同时间、位置更靠后），只显示一次。
INSERT INTO sessions (id, source, created_source, started_at, cwd, title, last_activity_at)
VALUES ('20261122_100000_bb0002', 'local', 'cli', 1795283600, '/work/db', '迁移数据库', 1795284000);
INSERT INTO messages (id, session_id, role, content, tool_calls, tool_name, timestamp, active, compacted,
                      display_metadata, display_order, message_uid) VALUES
 (10, '20261122_100000_bb0002', 'user', '先把订单表迁到新库', NULL, NULL, 1795283610, 0, 1, NULL, 10, 'u-10'),
 (11, '20261122_100000_bb0002', 'assistant', '',
  '[{"id":"call_1","type":"function","function":{"name":"terminal","arguments":"{\"command\":\"migrate orders\"}"}}]',
  NULL, 1795283620, 0, 1, NULL, 11, 'u-11'),
 (12, '20261122_100000_bb0002', 'tool', 'migrated 1200 rows', NULL, 'terminal', 1795283630, 0, 1, NULL, 12, 'u-12'),
 (13, '20261122_100000_bb0002', 'assistant', '订单表迁好了，索引也建了。', NULL, NULL, 1795283640, 0, 1, NULL, 13, 'u-13'),
 (14, '20261122_100000_bb0002', 'user', '这句被回退了', NULL, NULL, 1795283650, 0, 0, NULL, 14, 'u-14'),
 (15, '20261122_100000_bb0002', 'user', '再核对一遍行数', NULL, NULL, 1795283660, 0, 1, NULL, 15, 'u-15'),
 (16, '20261122_100000_bb0002', 'assistant', '行数一致。', NULL, NULL, 1795283670, 0, 1, NULL, 16, 'u-16'),
 (17, '20261122_100000_bb0002', 'user', '[压缩摘要] 之前迁了订单表', NULL, NULL, 1795283680, 1, 0,
  '{"model_only": true}', 17, 'u-17'),
 (18, '20261122_100000_bb0002', 'user', '再核对一遍行数', NULL, NULL, 1795283660, 1, 0, NULL, 15, 'u-18'),
 (19, '20261122_100000_bb0002', 'assistant', '行数一致。', NULL, NULL, 1795283670, 1, 0, NULL, 16, 'u-19'),
 (20, '20261122_100000_bb0002', 'tool', 'migrated 1200 rows', NULL, 'terminal', 1795283630, 1, 0, NULL, 20, 'u-12'),
 (21, '20261122_100000_bb0002', 'user', '接着迁用户表', NULL, NULL, 1795283900, 1, 0, NULL, 21, 'u-21'),
 (22, '20261122_100000_bb0002', 'assistant', '用户表也迁完了。', NULL, NULL, 1795284000, 1, 0, NULL, 22, 'u-22');

-- 3. 坏时间：最后活动时间和一条回复写成 1e999（SQLite 存成正无穷），另一条写成 1e20。
INSERT INTO sessions (id, source, created_source, started_at, cwd, title, last_activity_at)
VALUES ('20261122_110000_cc0003', 'local', 'tui', 1795287000, '/work/web', '导入的旧对话', 1e999);
INSERT INTO messages (id, session_id, role, content, timestamp, display_order) VALUES
 (30, '20261122_110000_cc0003', 'user', '这是从别处导入的', 1795287100, 30),
 (31, '20261122_110000_cc0003', 'assistant', '时间坏了的回复', 1e999, 31),
 (32, '20261122_110000_cc0003', 'assistant', '时间也坏了', 1e20, 32);

-- 4. 之后会被改名、原地脱敏的对话（测试里改）。
INSERT INTO sessions (id, source, created_source, started_at, cwd, title, last_activity_at)
VALUES ('20261122_120000_dd0004', 'local', 'tui', 1795290600, '/work/ops', '部署口令', 1795290700);
INSERT INTO messages (id, session_id, role, content, timestamp, display_order) VALUES
 (40, '20261122_120000_dd0004', 'user', '测试环境的口令是 hunter2', 1795290610, 40),
 (41, '20261122_120000_dd0004', 'assistant', '记下了。', 1795290700, 41);
