-- 手写脱敏的 Codex 正文库（thread_items 只保留会话台读的列）。
PRAGMA journal_mode = wal;
CREATE TABLE thread_items (
    thread_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    rollout_ordinal INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    item_json TEXT NOT NULL,
    item_type TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (thread_id, turn_id, item_id)
);
INSERT INTO thread_items VALUES
 ('cccccccc-0000-4000-8000-000000000003', 't1', 'i1', 1, 1795160000000, '{"type":"userMessage","id":"i1","content":[{"type":"text","text":"接口加分页"}]}', 'userMessage'),
 ('cccccccc-0000-4000-8000-000000000003', 't1', 'i2', 2, 1795160010000, '{"type":"reasoning","id":"i2"}', 'reasoning'),
 ('cccccccc-0000-4000-8000-000000000003', 't1', 'i3', 3, 1795160020000, '{"type":"agentMessage","id":"i3","text":"分页加好了"}', 'agentMessage'),
 ('cccccccc-0000-4000-8000-000000000003', 't2', 'i4', 4, 1795169000000, '{"type":"userMessage","id":"i4","content":[{"type":"text","text":"再加排序"}]}', 'userMessage'),
 ('ffffffff-0000-4000-8000-000000000006', 't1', 'i1', 1, 1795172000000, '{"type":"userMessage","id":"i1","content":[{"type":"text","text":"只有第一句话"}]}', 'userMessage'),
 ('ffffffff-0000-4000-8000-000000000006', 't1', 'i2', 2, 1795172400000, '{"type":"agentMessage","id":"i2","text":"库里的回答"}', 'agentMessage');
