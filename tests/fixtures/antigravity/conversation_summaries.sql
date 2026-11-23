-- Antigravity 的 conversation_summaries.db（手写的假数据）。表结构照本机 Antigravity 2.29 的库，原样抄的 schema。
-- 对话：在用的（两块正文、空的 AI 步）、没标题的（标题取第一句、工作区第一个是远程）、全文表标了归档的、
-- 标注文件里归档的、全文表标了归档但标注文件取消了的、子代理、没说过话的。
pragma journal_mode = wal;
CREATE TABLE `conversation_summaries` (`conversation_id` text,`title` text NOT NULL DEFAULT "",`preview` text NOT NULL DEFAULT "",`step_count` integer NOT NULL DEFAULT 0,`last_modified_time` datetime NOT NULL,`workspace_uris` text NOT NULL,`status` text NOT NULL DEFAULT "",`source` text NOT NULL DEFAULT "",`project_id` text NOT NULL DEFAULT "",`agent_name` text NOT NULL DEFAULT "",`parent_conversation_id` text NOT NULL DEFAULT "",`nesting_depth` integer NOT NULL DEFAULT 0,`battle_id` text NOT NULL DEFAULT "",`winning_conversation_id` text NOT NULL DEFAULT "",`not_fully_idle` numeric NOT NULL DEFAULT false,`killed` numeric NOT NULL DEFAULT false,`last_user_input_time` datetime NOT NULL,`last_user_input_step_index` integer NOT NULL DEFAULT -1,`app_data_dir` text NOT NULL DEFAULT "",`raw_summary` blob,`group_id` text NOT NULL DEFAULT "",PRIMARY KEY (`conversation_id`));
CREATE TABLE search_schema_version (version INTEGER PRIMARY KEY);
insert into search_schema_version values (3);
CREATE VIRTUAL TABLE conversations_fts USING fts5(conversation_id UNINDEXED, chunk_index UNINDEXED, chunk_first_step UNINDEXED, chunk_last_step UNINDEXED, last_modified_time UNINDEXED, step_count UNINDEXED, last_step_created_at UNINDEXED, archived UNINDEXED, user_step_byte_offsets UNINDEXED, agent_step_byte_offsets UNINDEXED, title, workspace, project, user_body, agent_body, tokenize = 'unicode61 remove_diacritics 2', prefix = '1 2 3');
insert into conversation_summaries (conversation_id,title,preview,step_count,last_modified_time,workspace_uris,status,parent_conversation_id,nesting_depth,last_user_input_time,last_user_input_step_index,app_data_dir) values ('a9e10000-1111-4aaa-8bbb-000000000001','下单接口加幂等','下单接口加幂等',13,'2026-11-20 02:15:00.5+00:00','["file:///Users/demo/projects/my%20shop"]','CASCADE_RUN_STATUS_IDLE','',0,'2026-11-20 02:14:00+00:00',10,'antigravity-cli');
insert into conversation_summaries (conversation_id,title,preview,step_count,last_modified_time,workspace_uris,status,parent_conversation_id,nesting_depth,last_user_input_time,last_user_input_step_index,app_data_dir) values ('a9e10000-2222-4aaa-8bbb-000000000002','','',2,'0001-01-01 00:00:00+00:00','["vscode-remote://ssh-remote+box/srv/app","file:///Users/demo/projects/blog"]','CASCADE_RUN_STATUS_IDLE','',0,'2026-11-19 10:00:00+00:00',0,'antigravity-cli');
insert into conversation_summaries (conversation_id,title,preview,step_count,last_modified_time,workspace_uris,status,parent_conversation_id,nesting_depth,last_user_input_time,last_user_input_step_index,app_data_dir) values ('a9e10000-3333-4aaa-8bbb-000000000003','旧的打包脚本','',2,'2026-11-01 08:00:00+00:00','["file:///Users/demo/projects/shop"]','CASCADE_RUN_STATUS_IDLE','',0,'2026-11-01 08:00:00+00:00',0,'antigravity-cli');
insert into conversation_summaries (conversation_id,title,preview,step_count,last_modified_time,workspace_uris,status,parent_conversation_id,nesting_depth,last_user_input_time,last_user_input_step_index,app_data_dir) values ('a9e10000-4444-4aaa-8bbb-000000000004','在 App 里归档的','',2,'2026-11-02 08:00:00+00:00','["file:///Users/demo/projects/shop"]','CASCADE_RUN_STATUS_IDLE','',0,'2026-11-02 08:00:00+00:00',0,'antigravity-cli');
insert into conversation_summaries (conversation_id,title,preview,step_count,last_modified_time,workspace_uris,status,parent_conversation_id,nesting_depth,last_user_input_time,last_user_input_step_index,app_data_dir) values ('a9e10000-5555-4aaa-8bbb-000000000005','取消归档的对话','',2,'2026-11-18 09:30:00+00:00','[]','CASCADE_RUN_STATUS_IDLE','',0,'2026-11-18 09:29:00+00:00',0,'antigravity-cli');
insert into conversation_summaries (conversation_id,title,preview,step_count,last_modified_time,workspace_uris,status,parent_conversation_id,nesting_depth,last_user_input_time,last_user_input_step_index,app_data_dir) values ('a9e10000-6666-4aaa-8bbb-000000000006','research','',2,'2026-11-20 02:10:00+00:00','["file:///Users/demo/projects/my%20shop"]','CASCADE_RUN_STATUS_IDLE','a9e10000-1111-4aaa-8bbb-000000000001',1,'2026-11-20 02:09:00+00:00',0,'antigravity-cli');
insert into conversation_summaries (conversation_id,title,preview,step_count,last_modified_time,workspace_uris,status,parent_conversation_id,nesting_depth,last_user_input_time,last_user_input_step_index,app_data_dir) values ('a9e10000-7777-4aaa-8bbb-000000000007','','',0,'2026-11-15 08:00:00+00:00','["file:///Users/demo/projects/shop"]','CASCADE_RUN_STATUS_IDLE','',0,'0001-01-01 00:00:00+00:00',-1,'antigravity-cli');
insert into conversations_fts (conversation_id,chunk_index,chunk_first_step,chunk_last_step,last_modified_time,step_count,last_step_created_at,archived,user_step_byte_offsets,agent_step_byte_offsets,title,workspace,project,user_body,agent_body) values ('a9e10000-1111-4aaa-8bbb-000000000001',-1,0,0,0,13,0,0,'','','下单接口加幂等','','','','');
insert into conversations_fts (conversation_id,chunk_index,chunk_first_step,chunk_last_step,last_modified_time,step_count,last_step_created_at,archived,user_step_byte_offsets,agent_step_byte_offsets,title,workspace,project,user_body,agent_body) values ('a9e10000-1111-4aaa-8bbb-000000000001',0,0,9,0,0,0,0,'[[0,0],[6,52]]','[[2,0],[3,1],[8,53]]','','','','下单接口加幂等，重复点提交会下两单
幂等键放请求头还是 body？
','
先看下单接口现在怎么处理重复请求。
放请求头 Idempotency-Key，服务端存 24 小时。
');
insert into conversations_fts (conversation_id,chunk_index,chunk_first_step,chunk_last_step,last_modified_time,step_count,last_step_created_at,archived,user_step_byte_offsets,agent_step_byte_offsets,title,workspace,project,user_body,agent_body) values ('a9e10000-1111-4aaa-8bbb-000000000001',1,10,12,0,0,0,0,'[[10,0]]','[[11,0],[12,1]]','','','','好，就这么做
','
改好了：重复的键直接回上次的结果，补了测试。
');
insert into conversations_fts (conversation_id,chunk_index,chunk_first_step,chunk_last_step,last_modified_time,step_count,last_step_created_at,archived,user_step_byte_offsets,agent_step_byte_offsets,title,workspace,project,user_body,agent_body) values ('a9e10000-2222-4aaa-8bbb-000000000002',-1,0,0,0,2,0,0,'','','','','','','');
insert into conversations_fts (conversation_id,chunk_index,chunk_first_step,chunk_last_step,last_modified_time,step_count,last_step_created_at,archived,user_step_byte_offsets,agent_step_byte_offsets,title,workspace,project,user_body,agent_body) values ('a9e10000-2222-4aaa-8bbb-000000000002',0,0,1,0,0,0,0,'[[0,0]]','[[1,0]]','','','','RSS 输出全文
别只给摘要
','feed.xml 改成输出全文了。
');
insert into conversations_fts (conversation_id,chunk_index,chunk_first_step,chunk_last_step,last_modified_time,step_count,last_step_created_at,archived,user_step_byte_offsets,agent_step_byte_offsets,title,workspace,project,user_body,agent_body) values ('a9e10000-3333-4aaa-8bbb-000000000003',-1,0,0,0,2,0,1,'','','旧的打包脚本','','','','');
insert into conversations_fts (conversation_id,chunk_index,chunk_first_step,chunk_last_step,last_modified_time,step_count,last_step_created_at,archived,user_step_byte_offsets,agent_step_byte_offsets,title,workspace,project,user_body,agent_body) values ('a9e10000-3333-4aaa-8bbb-000000000003',0,0,1,0,0,0,0,'[[0,0]]','[[1,0]]','','','','打包脚本换成 make
','换好了。
');
insert into conversations_fts (conversation_id,chunk_index,chunk_first_step,chunk_last_step,last_modified_time,step_count,last_step_created_at,archived,user_step_byte_offsets,agent_step_byte_offsets,title,workspace,project,user_body,agent_body) values ('a9e10000-4444-4aaa-8bbb-000000000004',-1,0,0,0,2,0,0,'','','在 App 里归档的','','','','');
insert into conversations_fts (conversation_id,chunk_index,chunk_first_step,chunk_last_step,last_modified_time,step_count,last_step_created_at,archived,user_step_byte_offsets,agent_step_byte_offsets,title,workspace,project,user_body,agent_body) values ('a9e10000-4444-4aaa-8bbb-000000000004',0,0,1,0,0,0,0,'[[0,0]]','[[1,0]]','','','','日志级别调低
','调成 warn 了。
');
insert into conversations_fts (conversation_id,chunk_index,chunk_first_step,chunk_last_step,last_modified_time,step_count,last_step_created_at,archived,user_step_byte_offsets,agent_step_byte_offsets,title,workspace,project,user_body,agent_body) values ('a9e10000-5555-4aaa-8bbb-000000000005',-1,0,0,0,2,0,1,'','','取消归档的对话','','','','');
insert into conversations_fts (conversation_id,chunk_index,chunk_first_step,chunk_last_step,last_modified_time,step_count,last_step_created_at,archived,user_step_byte_offsets,agent_step_byte_offsets,title,workspace,project,user_body,agent_body) values ('a9e10000-5555-4aaa-8bbb-000000000005',0,0,1,0,0,0,0,'[[0,0]]','[[1,0]]','','','','看看磁盘还剩多少
','还剩 120G。
');
insert into conversations_fts (conversation_id,chunk_index,chunk_first_step,chunk_last_step,last_modified_time,step_count,last_step_created_at,archived,user_step_byte_offsets,agent_step_byte_offsets,title,workspace,project,user_body,agent_body) values ('a9e10000-6666-4aaa-8bbb-000000000006',-1,0,0,0,2,0,0,'','','research','','','','');
insert into conversations_fts (conversation_id,chunk_index,chunk_first_step,chunk_last_step,last_modified_time,step_count,last_step_created_at,archived,user_step_byte_offsets,agent_step_byte_offsets,title,workspace,project,user_body,agent_body) values ('a9e10000-6666-4aaa-8bbb-000000000006',0,0,1,0,0,0,0,'[[0,0]]','[[1,0]]','','','','查一下常见的幂等做法
','三种：唯一键、令牌、状态机。
');
