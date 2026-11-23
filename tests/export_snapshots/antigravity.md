# 下单接口加幂等

- 厂商：Antigravity
- 项目：my shop（/Users/demo/projects/my shop）
- 时间：2026-11-20 10:15（UTC+08:00）
- 对话 id：`a9e10000-1111-4aaa-8bbb-000000000001`

## 你

下单接口加幂等，重复点提交会下两单

## Antigravity

先看下单接口现在怎么处理重复请求。

## 你

幂等键放请求头还是 body？

## Antigravity

放请求头 Idempotency-Key，服务端存 24 小时。

## 你

好，就这么做

## Antigravity

改好了：重复的键直接回上次的结果，补了测试。
