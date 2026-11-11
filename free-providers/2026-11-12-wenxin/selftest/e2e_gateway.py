#!/usr/bin/env python3
"""端到端自测：**真网关**加载插件包 → 加账号 → 模型列表 → 真实转发。

与 `selftest.py` 的分工：
  · `selftest.py` 直接对插件二进制讲网关协议（帧级、错误分类、token 续期）；
  · 本脚本把插件包按**官方布局**装进一个**独立数据目录**的网关实例，走
    真实的 HTTP 面（`/api/*`、`/v1/*`），证明「应用真的发现并加载了它」。

跑法（仓库根目录）：

    # 1) 起一个隔离的网关实例（不碰 ~/.agent2api，也不碰 3065 上的生产实例）
    export AGENT2API_PROXY_HOME="$PWD/.scratch-tmp/wx-gw"
    export AGENT2API_PROXY_PORT=3066 AGENT2API_HOST=127.0.0.1 AGENT2API_ALLOW_NO_KEY=1
    ./.scratch-tmp/agent2api-server &

    # 2) 跑本脚本
    python3 free-providers/2026-11-12-wenxin/selftest/e2e_gateway.py

脚本自己起 mock 上游（本机、假 Key），不触碰任何真实上游。管理员口令是
运行时随机生成的一次性口令，**不打印、不落盘**。
"""

import http.cookiejar
import json
import os
import secrets
import sys
import threading
import urllib.error
import urllib.request
from http.server import ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from selftest import Upstream  # noqa: E402  （复用同一份 mock 上游）

GATEWAY = os.environ.get("WX_GATEWAY", "http://127.0.0.1:3066")
PASS, FAIL = [], []


def check(name, ok, detail=""):
    (PASS if ok else FAIL).append(name)
    print(("  PASS  " if ok else "  FAIL  ") + name + (("  ← " + detail) if detail and not ok else ""))
    return ok


class Api:
    def __init__(self, base):
        self.base = base
        self.jar = http.cookiejar.CookieJar()
        self.opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(self.jar))

    def call(self, path, payload=None, method=None, raw=False, timeout=90, headers=None):
        status, body = self._call(path, payload, method, raw, timeout, headers)
        # 管理接口回 `{success, data}` 信封；这里统一拆开，用例只看 data
        if isinstance(body, dict) and "success" in body and "data" in body:
            return status, body["data"]
        return status, body

    def _call(self, path, payload=None, method=None, raw=False, timeout=90, extra_headers=None):
        url = self.base + path
        data = None
        headers = dict(extra_headers or {})
        if payload is not None:
            data = json.dumps(payload, ensure_ascii=False).encode()
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(url, data=data, headers=headers,
                                        method=method or ("POST" if data else "GET"))
        try:
            with self.opener.open(request, timeout=timeout) as response:
                body = response.read().decode("utf-8", "replace")
                return response.status, (body if raw else json.loads(body or "{}"))
        except urllib.error.HTTPError as error:
            body = error.read().decode("utf-8", "replace")
            try:
                return error.code, json.loads(body or "{}")
            except Exception:
                return error.code, {"raw": body}


def main():
    upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    port = upstream.server_address[1]
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    mock = "http://127.0.0.1:%d" % port
    print("mock 上游：%s" % mock)

    api = Api(GATEWAY)
    status, _ = api.call("/api/panel/status")
    if status != 200:
        print("网关不可达：%s（先按文件头注释起隔离实例）" % GATEWAY)
        return 2

    # 一次性管理员口令：随机、不打印、不落盘
    username = "selftest"
    password = secrets.token_urlsafe(18)
    status, body = api.call("/api/panel/setup", {"username": username, "password": password})
    if status == 409:
        print("该实例已注册过管理员，跳过注册（用现有会话）")
    else:
        check("面板注册成功（取得会话）", status == 200, json.dumps(body)[:200])

    # 插件是否被应用发现
    status, session = api.call("/api/session")
    check("/api/session 可读", status == 200, json.dumps(session)[:200])
    accounts_blob = session.get("accounts") or {}
    providers = accounts_blob.get("providers") or []
    ids = [item.get("id") for item in providers]
    wenxin = next((item for item in providers if item.get("id") == "wenxin"), None)
    print("    （/api/session 的家清单：%s）" % (str(ids)[:200] or "[]"))
    check("应用发现插件家 wenxin（/api/session 的家清单里有它）", wenxin is not None, str(ids)[:200])
    if wenxin:
        check("标签来自插件（文心（千帆））", wenxin.get("label") == "文心（千帆）",
              str(wenxin.get("label")))
        check("provider 来源标为 plugin", wenxin.get("source") == "plugin",
              str(wenxin.get("source")))
        methods = wenxin.get("loginMethods") or []
        check("添加账号表单由插件申报驱动（loginMethods 含 token 表单）",
              any(item.get("kind") == "token" for item in methods),
              json.dumps(methods)[:200])
        features = (session.get("accountFeatures") or session.get("account_features") or {}).get("wenxin") or {}
        check("账号行渲染参数来自插件（identifier=authMode，usage=false）",
              features.get("identifier") == ["authMode"] and features.get("usage") is False,
              json.dumps(features)[:200])

    # 建一把**只允许 wenxin** 的网关 Key：/v1 面 fail-closed，且限家后
    # 同名模型不会被路由到别家（结论可归因）
    gateway_key = "sk-dev-selftest-" + secrets.token_hex(8)
    status, keyed = api.call("/api/keys", {
        "name": "wenxin 自测", "key": gateway_key, "allowedProviders": ["wenxin"],
    })
    check("创建只允许 wenxin 的网关 Key", status == 200, json.dumps(keyed)[:200])
    auth = {"Authorization": "Bearer " + gateway_key}

    # 加一条账号（凭证指向 mock 上游，假 Key）
    status, added = api.call("/api/accounts", {
        "provider": "wenxin",
        "name": "文心自测",
        "apiKey": "bce-v3/ALTAK-SELFTEST/0000000000000",
        "baseUrl": mock + "/v2",
    })
    check("添加账号成功（走插件的 account.normalize）", status == 200, json.dumps(added)[:300])
    accounts = [item for item in (added.get("list") or []) if isinstance(item, dict)]
    row = added.get("account") or next(
        (item for item in accounts if item.get("provider") == "wenxin"), None)
    if not check("账号出现在列表里", row is not None, json.dumps(accounts)[:300]):
        return 1
    public_blob = json.dumps(row, ensure_ascii=False)
    check("账号列表不含凭证本体", "ALTAK-SELFTEST" not in public_blob
          and "accessToken" not in public_blob, public_blob[:300])
    check("账号行带 tokenTail（尾 4 字符）", row.get("tokenTail") == "0000"
          or "0000" in public_blob, public_blob[:300])
    check("账号行由插件的 account_view 产出（authMode=API Key）",
          row.get("authMode") == "API Key", public_blob[:300])

    # 插件进程真的被网关拉起来了（宿主 spawn 的入口二进制）
    running = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            exe = os.readlink("/proc/%s/exe" % entry)
        except OSError:
            continue
        if "free-wenxin" in exe:
            running.append(exe)
    check("网关已 spawn 插件入口二进制（free-wenxin 进程在跑）",
          len(running) > 0, str(running))

    # 模型列表
    status, models = api.call("/v1/models")
    rows = models.get("data") or []
    wenxin_models = [item.get("id") for item in rows if item.get("owned_by") == "wenxin"]
    check("/v1/models 出现 wenxin 的模型", len(wenxin_models) > 0, str(len(rows)))
    check("/v1/models 含 ernie-5.1", "ernie-5.1" in wenxin_models, str(wenxin_models[:8]))

    # 非流式对话（网关把插件恒流式的帧聚合成本文 + usage）
    status, chat = api.call("/v1/chat/completions", {
        "model": "ernie-5.1",
        "messages": [{"role": "user", "content": "回复两个字"}],
        "stream": False,
    }, headers=auth)
    content = ""
    usage = {}
    if status == 200:
        choice = (chat.get("choices") or [{}])[0]
        content = (choice.get("message") or {}).get("content") or ""
        usage = chat.get("usage") or {}
    check("非流式对话 200 且正文为「你好」", status == 200 and content == "你好",
          "status=%s content=%r body=%s" % (status, content, json.dumps(chat)[:200]))
    check("非流式响应的 usage 非零（total_tokens=11）",
          usage.get("total_tokens") == 11, json.dumps(usage))

    # 流式对话（逐块消费 + [DONE]）
    request = urllib.request.Request(
        GATEWAY + "/v1/chat/completions",
        data=json.dumps({
            "model": "ernie-5.1",
            "messages": [{"role": "user", "content": "回复两个字"}],
            "stream": True,
        }).encode(),
        headers={"Content-Type": "application/json", "Authorization": "Bearer " + gateway_key},
        method="POST",
    )
    frames, done, text = [], False, ""
    try:
        with api.opener.open(request, timeout=90) as response:
            for raw in response:
                line = raw.decode("utf-8", "replace").strip()
                if not line.startswith("data:"):
                    continue
                data = line[5:].strip()
                if data == "[DONE]":
                    done = True
                    continue
                try:
                    frame = json.loads(data)
                except Exception:
                    continue
                frames.append(frame)
                for choice in frame.get("choices") or []:
                    text += (choice.get("delta") or {}).get("content") or ""
    except urllib.error.HTTPError as error:
        check("流式对话可建连", False, str(error.code))
    check("流式对话逐块下发且拼出「你好」", text == "你好", repr(text))
    check("流式对话以 [DONE] 结束", done, repr(frames[-1:])[:200])

    # 上游错误 → 客户端可读状态（凭证被拒）
    status, bad = api.call("/api/accounts", {
        "provider": "wenxin",
        "name": "文心自测-坏Key",
        "apiKey": "bce-v3/ALTAK-SELFTEST-BAD/0000000000000",
        "baseUrl": mock + "/v2",
    })
    check("被上游拒绝的 Key 不会被落库（插件 normalize 拦下）", status != 200,
          "status=%s body=%s" % (status, json.dumps(bad)[:200]))

    upstream.shutdown()
    print("\n结果：%d 项通过，%d 项失败" % (len(PASS), len(FAIL)))
    if FAIL:
        print("失败项：" + "；".join(FAIL))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
