-- OpenCode 的 opencode.db 脱敏样例（手写）。一个库里两套表，和 2.x 升级过的真库一样：
-- 1.x 的 session/message/part（v1.19.6 packages/core/src/session/sql.ts）和 2.x 的 session_v2/session_message/session_part
-- （dev 分支 packages/core/src/database/schema.gen.ts），只留会话台用到的列和几列不用的。时间是毫秒。
-- id 按上游 identifier.ts 的 descending 算法生成：ses_ + !(毫秒*4096+1) 的低 48 位十六进制 + 14 位字母数字。
PRAGMA journal_mode = WAL;

CREATE TABLE project (
    id text PRIMARY KEY,
    worktree text NOT NULL,
    vcs text,
    name text,
    time_created integer NOT NULL,
    time_updated integer NOT NULL,
    sandboxes text NOT NULL
);
INSERT INTO project VALUES ('prj_api', '/work/api', 'git', NULL, 1795000000000, 1795000000000, '[]');
INSERT INTO project VALUES ('prj_blog', '/work/blog', 'git', NULL, 1795000000000, 1795000000000, '[]');

-- ---- 1.x ----
CREATE TABLE session (
    id text PRIMARY KEY,
    project_id text NOT NULL,
    workspace_id text,
    parent_id text,
    slug text NOT NULL,
    directory text NOT NULL,
    path text,
    title text NOT NULL,
    version text NOT NULL,
    share_url text,
    cost real DEFAULT 0 NOT NULL,
    agent text,
    model text,
    time_created integer NOT NULL,
    time_updated integer NOT NULL,
    time_compacting integer,
    time_archived integer
);
CREATE TABLE message (
    id text PRIMARY KEY,
    session_id text NOT NULL,
    time_created integer NOT NULL,
    time_updated integer NOT NULL,
    data text NOT NULL
);
CREATE TABLE part (
    id text PRIMARY KEY,
    message_id text NOT NULL,
    session_id text NOT NULL,
    time_created integer NOT NULL,
    time_updated integer NOT NULL,
    data text NOT NULL
);

-- 只在 1.x 里：有标题，用户消息里夹一块系统塞的 synthetic 文字，助手调了工具。
INSERT INTO session VALUES ('ses_e11a9a1ffffeV1onlyAbCdEfGh', 'prj_api', NULL, NULL, 'brave-otter', '/work/api', NULL,
    '整理接口文档', '1.19.6', NULL, 0.12, 'build', '{"id":"m1","providerID":"p1"}', 1795000000000, 1795000400000, NULL, NULL);
INSERT INTO message VALUES ('msg_a1', 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 1795000010000, 1795000010000,
    '{"role":"user","time":{"created":1795000010000},"agent":"build","model":{"providerID":"p1","modelID":"m1"}}');
INSERT INTO part VALUES ('prt_a1a', 'msg_a1', 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 1795000010000, 1795000010000,
    '{"type":"text","text":"把接口文档按模块拆开"}');
INSERT INTO part VALUES ('prt_a1b', 'msg_a1', 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 1795000010000, 1795000010000,
    '{"type":"text","text":"系统附加的上下文","synthetic":true}');
INSERT INTO message VALUES ('msg_a2', 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 1795000020000, 1795000030000,
    '{"role":"assistant","time":{"created":1795000020000,"completed":1795000030000},"modelID":"m1","providerID":"p1"}');
INSERT INTO part VALUES ('prt_a2a', 'msg_a2', 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 1795000020000, 1795000020000,
    '{"type":"step-start"}');
INSERT INTO part VALUES ('prt_a2b', 'msg_a2', 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 1795000021000, 1795000022000,
    '{"type":"tool","tool":"read","callID":"call_1","state":{"status":"completed","input":{"filePath":"docs/api.md"},"output":"# API\n订单、用户","title":"docs/api.md","metadata":{},"time":{"start":1795000021000,"end":1795000022000}}}');
INSERT INTO part VALUES ('prt_a2c', 'msg_a2', 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 1795000025000, 1795000025000,
    '{"type":"reasoning","text":"先读文档","time":{"start":1795000025000}}');
INSERT INTO part VALUES ('prt_a2d', 'msg_a2', 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 1795000029000, 1795000029000,
    '{"type":"text","text":"分成订单、用户两块了。"}');
INSERT INTO message VALUES ('msg_a3', 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 1795000060000, 1795000060000,
    '{"role":"user","time":{"created":1795000060000}}');
INSERT INTO part VALUES ('prt_a3a', 'msg_a3', 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 1795000060000, 1795000060000,
    '{"type":"text","text":"旧链接要能跳过去"}');
INSERT INTO message VALUES ('msg_a4', 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 1795000070000, 1795000080000,
    '{"role":"assistant","time":{"created":1795000070000}}');
INSERT INTO part VALUES ('prt_a4a', 'msg_a4', 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 1795000071000, 1795000072000,
    '{"type":"tool","tool":"bash","callID":"call_2","state":{"status":"error","input":{"command":"npm test"},"error":"exit 1","time":{"start":1795000071000,"end":1795000072000}}}');
INSERT INTO part VALUES ('prt_a4b', 'msg_a4', 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 1795000079000, 1795000079000,
    '{"type":"text","text":"旧路径都加了重定向。"}');

-- 占位标题、目录为空（老对话）：标题退用第一句话，目录退用项目根目录。
INSERT INTO session VALUES ('ses_e119a5fbfffeV1fallbackAbCd', 'prj_blog', NULL, NULL, 'calm-heron', '', NULL,
    'New session - 2026-11-17T19:06:41.000Z', '1.19.6', NULL, 0, NULL, NULL, 1795001000000, 1795001100000, NULL, NULL);
INSERT INTO message VALUES ('msg_b1', 'ses_e119a5fbfffeV1fallbackAbCd', 1795001010000, 1795001010000,
    '{"role":"user","time":{"created":1795001010000}}');
INSERT INTO part VALUES ('prt_b1a', 'msg_b1', 'ses_e119a5fbfffeV1fallbackAbCd', 1795001010000, 1795001010000,
    '{"type":"text","text":"把草稿里的图片\n压一遍"}');

-- 子代理开的（parent_id）、归档的、没说过话的：都不列。
INSERT INTO session VALUES ('ses_e118b1d7fffeChildAbCdEfGhI', 'prj_api', NULL, 'ses_e11a9a1ffffeV1onlyAbCdEfGh', 'x', '/work/api', NULL,
    'Child session - 2026-11-17T19:06:42.000Z', '1.19.6', NULL, 0, NULL, NULL, 1795002000000, 1795002100000, NULL, NULL);
INSERT INTO message VALUES ('msg_c1', 'ses_e118b1d7fffeChildAbCdEfGhI', 1795002010000, 1795002010000, '{"role":"user","time":{"created":1795002010000}}');
INSERT INTO part VALUES ('prt_c1a', 'msg_c1', 'ses_e118b1d7fffeChildAbCdEfGhI', 1795002010000, 1795002010000, '{"type":"text","text":"子任务"}');
INSERT INTO session VALUES ('ses_e117bdb3fffeArchivedAbCdEf', 'prj_api', NULL, NULL, 'y', '/work/api', NULL,
    '归档了的', '1.19.6', NULL, 0, NULL, NULL, 1795003000000, 1795003100000, NULL, 1795003200000);
INSERT INTO message VALUES ('msg_d1', 'ses_e117bdb3fffeArchivedAbCdEf', 1795003010000, 1795003010000, '{"role":"user","time":{"created":1795003010000}}');
INSERT INTO part VALUES ('prt_d1a', 'msg_d1', 'ses_e117bdb3fffeArchivedAbCdEf', 1795003010000, 1795003010000, '{"type":"text","text":"旧事"}');
INSERT INTO session VALUES ('ses_e116c98ffffeEmptyAbCdEfGhI', 'prj_api', NULL, NULL, 'z', '/work/api', NULL,
    'New session - 2026-11-17T19:06:44.000Z', '1.19.6', NULL, 0, NULL, NULL, 1795004000000, 1795004000000, NULL, NULL);

-- 两套都有：2.x 复制过去以后又在 2.x 里聊了（2.x 更新）。
INSERT INTO session VALUES ('ses_e115d56bfffeDupV2newerAbCd', 'prj_api', NULL, NULL, 'd2', '/work/api', NULL,
    '修结算按钮', '1.19.6', NULL, 0, NULL, NULL, 1795005000000, 1795005100000, NULL, NULL);
INSERT INTO message VALUES ('msg_e1', 'ses_e115d56bfffeDupV2newerAbCd', 1795005010000, 1795005010000, '{"role":"user","time":{"created":1795005010000}}');
INSERT INTO part VALUES ('prt_e1a', 'msg_e1', 'ses_e115d56bfffeDupV2newerAbCd', 1795005010000, 1795005010000, '{"type":"text","text":"结算按钮点了没反应"}');

-- 两套都有：复制过去以后又回 1.x 里聊了（1.x 更新）。
INSERT INTO session VALUES ('ses_e114e147fffeDupV1newerAbCd', 'prj_blog', NULL, NULL, 'd1', '/work/blog', NULL,
    '换评论系统', '1.19.6', NULL, 0, NULL, NULL, 1795006000000, 1795006900000, NULL, NULL);
INSERT INTO message VALUES ('msg_f1', 'ses_e114e147fffeDupV1newerAbCd', 1795006010000, 1795006010000, '{"role":"user","time":{"created":1795006010000}}');
INSERT INTO part VALUES ('prt_f1a', 'msg_f1', 'ses_e114e147fffeDupV1newerAbCd', 1795006010000, 1795006010000, '{"type":"text","text":"评论换成自建的"}');
INSERT INTO message VALUES ('msg_f2', 'ses_e114e147fffeDupV1newerAbCd', 1795006800000, 1795006800000, '{"role":"user","time":{"created":1795006800000}}');
INSERT INTO part VALUES ('prt_f2a', 'msg_f2', 'ses_e114e147fffeDupV1newerAbCd', 1795006800000, 1795006800000, '{"type":"text","text":"回 1.x 接着说的一句"}');

-- ---- 2.x ----
CREATE TABLE session_v2 (
    id text PRIMARY KEY,
    project_id text NOT NULL,
    workspace_id text,
    parent_id text,
    fork_session_id text,
    slug text NOT NULL,
    directory text NOT NULL,
    path text,
    title text,
    version text NOT NULL,
    cost real DEFAULT 0 NOT NULL,
    agent text,
    model text,
    time_created integer NOT NULL,
    time_updated integer NOT NULL,
    time_busy integer,
    time_idle integer,
    idle_outcome text,
    time_archived integer
);
CREATE TABLE session_message (
    id text PRIMARY KEY,
    session_id text NOT NULL,
    type text NOT NULL,
    seq integer NOT NULL,
    time_created integer NOT NULL,
    time_updated integer NOT NULL,
    data text NOT NULL
);
CREATE TABLE session_part (
    message_id text NOT NULL,
    session_id text NOT NULL,
    ordinal integer NOT NULL,
    type text NOT NULL,
    call_id text,
    time_created integer NOT NULL,
    time_updated integer NOT NULL,
    data text NOT NULL,
    CONSTRAINT session_part_pk PRIMARY KEY(message_id, ordinal)
);

-- 2.x 复制过来的那条，之后在 2.x 里又聊了一轮（标题也改了）。
INSERT INTO session_v2 VALUES ('ses_e115d56bfffeDupV2newerAbCd', 'prj_api', NULL, NULL, NULL, 'd2', '/work/api', NULL,
    '修结算按钮（2.x）', '1.19.6', 0, 'build', NULL, 1795005000000, 1795005500000, NULL, 1795005500000, 'succeeded', NULL);
INSERT INTO session_message VALUES ('msg_g1', 'ses_e115d56bfffeDupV2newerAbCd', 'user', 1, 1795005010000, 1795005010000,
    '{"text":"结算按钮点了没反应","time":{"created":1795005010000}}');
INSERT INTO session_message VALUES ('msg_g2', 'ses_e115d56bfffeDupV2newerAbCd', 'user', 2, 1795005400000, 1795005400000,
    '{"text":"2.x 里接着说的一句","time":{"created":1795005400000}}');

-- 2.x 复制过来的那条，之后回 1.x 里聊（2.x 这边停在复制那一刻）。
INSERT INTO session_v2 VALUES ('ses_e114e147fffeDupV1newerAbCd', 'prj_blog', NULL, NULL, NULL, 'd1', '/work/blog', NULL,
    '换评论系统', '1.19.6', 0, NULL, NULL, 1795006000000, 1795006100000, NULL, NULL, NULL, NULL);
INSERT INTO session_message VALUES ('msg_h1', 'ses_e114e147fffeDupV1newerAbCd', 'user', 1, 1795006010000, 1795006010000,
    '{"text":"评论换成自建的","time":{"created":1795006010000}}');

-- 只在 2.x 里：助手消息的块一部分拆进 session_part（type 在列里），一部分还内联在 data.content。
INSERT INTO session_v2 VALUES ('ses_e113ed23fffeV2onlyAbCdEfGh', 'prj_api', NULL, NULL, NULL, 'v2', '/work/api', NULL,
    NULL, '2.3.2', 0, 'build', NULL, 1795007000000, 1795007300000, NULL, NULL, NULL, NULL);
INSERT INTO session_message VALUES ('msg_j0', 'ses_e113ed23fffeV2onlyAbCdEfGh', 'agent-switched', 1, 1795007000000, 1795007000000,
    '{"agent":"build","time":{"created":1795007000000}}');
INSERT INTO session_message VALUES ('msg_j1', 'ses_e113ed23fffeV2onlyAbCdEfGh', 'user', 2, 1795007010000, 1795007010000,
    '{"text":"给登录页加验证码","files":[],"agents":[],"skills":[],"time":{"created":1795007010000}}');
INSERT INTO session_message VALUES ('msg_j2', 'ses_e113ed23fffeV2onlyAbCdEfGh', 'assistant', 3, 1795007020000, 1795007090000,
    '{"agent":"build","model":{"id":"m1","providerID":"p1"},"content":[{"type":"text","text":"验证码加好了，用的滑块。"}],"time":{"created":1795007020000,"completed":1795007090000}}');
INSERT INTO session_part VALUES ('msg_j2', 'ses_e113ed23fffeV2onlyAbCdEfGh', 0, 'reasoning', NULL, 1795007021000, 1795007021000,
    '{"text":"先看登录页"}');
INSERT INTO session_part VALUES ('msg_j2', 'ses_e113ed23fffeV2onlyAbCdEfGh', 1, 'tool', 'call_9', 1795007022000, 1795007030000,
    '{"id":"call_9","name":"edit","state":{"status":"completed","input":{"filePath":"src/login.tsx"},"content":[{"type":"text","text":"已改 1 处"}]},"time":{"created":1795007022000}}');
INSERT INTO session_part VALUES ('msg_j2', 'ses_e113ed23fffeV2onlyAbCdEfGh', 2, 'text', NULL, 1795007031000, 1795007031000,
    '{"text":"改了 src/login.tsx。"}');
INSERT INTO session_message VALUES ('msg_j3', 'ses_e113ed23fffeV2onlyAbCdEfGh', 'idle', 4, 1795007091000, 1795007091000,
    '{"outcome":"succeeded","time":{"created":1795007091000}}');

-- 2.x 里的子代理：不列。
INSERT INTO session_v2 VALUES ('ses_e112f8fffffeV2childAbCdEfG', 'prj_api', NULL, 'ses_e113ed23fffeV2onlyAbCdEfGh', NULL, 'c', '/work/api', NULL,
    '查文档', '2.3.2', 0, NULL, NULL, 1795008000000, 1795008100000, NULL, NULL, NULL, NULL);
INSERT INTO session_message VALUES ('msg_k1', 'ses_e112f8fffffeV2childAbCdEfG', 'user', 1, 1795008010000, 1795008010000,
    '{"text":"子任务","time":{"created":1795008010000}}');
