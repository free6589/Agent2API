#!/usr/bin/env python3
"""文心插件自测：本机 mock 上游 + 真实插件二进制 + 网关协议（JSON 行）。

跑法（在 `free-providers/2026-11-12-wenxin/` 下）：

    cargo build --release --offline
    python3 selftest/selftest.py            # 追加 --verbose 看每一帧

它证明什么（每一条都是**真的跑过**，不是静态断言）：

  1. 二进制能被网关拉起并握手（`init` 返回 `wenxin` 的 ProviderInfo）；
  2. `models.list` 有清单、`models.refresh` 能从上游 `/v2/models` 拉到并
     按 `type == "chat"` / `endpoints` / `status` 过滤（非对话、下线、
     不挂 chat 端点的条目必须被丢掉）；
  3. `conversation` 流式：head 200 → 多个 `chunk` 帧 → `result: null` 收尾，
     帧里是 OpenAI chat.completion.chunk 形态，末帧带 usage；
  4. 上游返回**非 SSE 整包**时，插件把它翻成「一段正文 + 收尾 + usage」的帧流
     —— 这正是网关给非流式客户端聚合时消费的形状；
  5. 上游 401 时 head 带状态码与原始错误体（网关据此调 `classify`）；
  6. `classify` 对官方错误码表的每一族给出正确的四档；
  7. AK/SK 模式：`token.ensure` 打 oauth 端点换 token 并**比较-再写回**账号；
     `token.expiring` 在临期时返回 true、`token.refresh` 能强制重取；
     V1 对话的 `result`/`is_end` 帧被翻成 OpenAI 帧；
  8. 账号规范化：API Key 被上游 401 时拒绝落库；正常时落库且公开形态
     **不含凭证本体**。

红线：本脚本只用假 Key（`bce-v3/ALTAK-SELFTEST/…`、`AKSELFTEST`），
上游是本机 mock，不出网。
"""

import base64
import json
import os
import queue
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

VERBOSE = "--verbose" in sys.argv
HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
BIN = os.path.join(ROOT, "target", "release", "free-wenxin")

PASS, FAIL = [], []


def check(name, ok, detail=""):
    (PASS if ok else FAIL).append(name)
    print(("  PASS  " if ok else "  FAIL  ") + name + (("  ← " + detail) if detail and not ok else ""))
    return ok


# ─────────────────────────── mock 上游 ───────────────────────────

MODELS_PAYLOAD = {
    "object": "list",
    "data": [
        {
            "id": "ernie-5.1",
            "object": "model",
            "owned_by": "baidu",
            "type": "chat",
            "model_name": "ERNIE 5.1",
            "prompt_tokens": 121856,
            "max_tokens": 65536,
            "endpoints": ["/v2/chat/completions", "/v2/responses"],
            "features": ["function_call"],
        },
        {
            "id": "deepseek-v4-pro",
            "object": "model",
            "type": "chat",
            "prompt_tokens": 1048576,
            "max_tokens": 393216,
            "endpoints": ["/v2/chat/completions"],
            "features": [],
        },
        # 以下三条必须被过滤掉
        {"id": "ernie-irag", "object": "model", "type": "embeddings", "endpoints": ["/v2/embeddings"]},
        {"id": "qianfan-vl-offline", "object": "model", "type": "chat", "status": "offline",
         "endpoints": ["/v2/chat/completions"]},
        {"id": "ernie-image", "object": "model", "type": "chat", "endpoints": ["/v2/responses"]},
    ],
}


class Upstream(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):  # 静音
        pass

    def _json(self, status, payload):
        body = json.dumps(payload, ensure_ascii=False).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _sse(self, frames, done=True):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        for frame in frames:
            self.wfile.write(("data: " + json.dumps(frame, ensure_ascii=False) + "\n\n").encode())
            self.wfile.flush()
        if done:
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        self.close_connection = True

    def do_GET(self):
        if self.path.startswith("/v2/models"):
            auth = self.headers.get("Authorization", "")
            if "SELFTEST-BAD" in auth:
                self._json(401, {"error": {"code": "invalid_iam_token",
                                           "message": "IAM Certification failed",
                                           "type": "invalid_request_error"}, "id": "as-bad"})
                return
            self._json(200, MODELS_PAYLOAD)
            return
        self._json(404, {"error": {"code": "not_found", "message": "no route"}})

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw or b"{}")
        except Exception:
            body = {}

        # ── V1 换 token ──────────────────────────────────────────
        if self.path.startswith("/oauth/2.0/token"):
            if "client_id=BADSECRET" in self.path:
                self._json(200, {"error": "invalid_client",
                                 "error_description": "unknown client id"})
                return
            self._json(200, {"access_token": "24.selftest.access-token",
                             "expires_in": 2592000})
            return

        # ── V1 对话（HTTP 200 + result/is_end 帧，无 [DONE]）──────
        if self.path.startswith("/rpc/2.0/ai_custom/v1/wenxinworkshop/chat"):
            if "access_token=24.selftest.access-token" not in self.path:
                self._json(200, {"error_code": 110,
                                 "error_msg": "Access token invalid or no longer valid"})
                return
            self._sse([
                {"id": "as-v1", "object": "chat.completion", "created": 1,
                 "sentence_id": 0, "is_end": False, "result": "你",
                 "need_clear_history": False, "finish_reason": "normal"},
                {"id": "as-v1", "object": "chat.completion", "created": 1,
                 "sentence_id": 1, "is_end": False, "result": "好",
                 "need_clear_history": False, "finish_reason": "normal",
                 "usage": {"prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9}},
                {"id": "as-v1", "object": "chat.completion", "created": 1,
                 "sentence_id": 2, "is_end": True, "result": "",
                 "need_clear_history": False, "finish_reason": "normal",
                 "usage": {"prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9}},
            ], done=False)
            return

        # ── V2 对话 ─────────────────────────────────────────────
        if self.path.startswith("/v2/chat/completions"):
            auth = self.headers.get("Authorization", "")
            model = body.get("model", "")
            if "SELFTEST-BAD" in auth:
                self._json(401, {"error": {"code": "invalid_iam_token",
                                           "message": "IAM Certification failed",
                                           "type": "invalid_request_error"}, "id": "as-bad"})
                return
            if model == "whole-json":
                # 上游忽略 stream 参数，整包回（旧端点偶发行为）
                self._json(200, {
                    "id": "as-whole", "object": "chat.completion", "created": 2,
                    "model": model,
                    "choices": [{"index": 0,
                                 "message": {"role": "assistant", "content": "整包回答"},
                                 "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8},
                })
                return
            self._sse([
                {"id": "as-stream", "object": "chat.completion.chunk", "created": 3,
                 "model": model,
                 "choices": [{"index": 0, "delta": {"role": "assistant", "content": "你"},
                              "finish_reason": None}]},
                {"id": "as-stream", "object": "chat.completion.chunk", "created": 3,
                 "model": model,
                 "choices": [{"index": 0, "delta": {"content": "好"}, "finish_reason": None}]},
                {"id": "as-stream", "object": "chat.completion.chunk", "created": 3,
                 "model": model,
                 "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
                {"id": "as-stream", "object": "chat.completion.chunk", "created": 3,
                 "model": model, "choices": [],
                 "usage": {"prompt_tokens": 9, "completion_tokens": 2, "total_tokens": 11}},
            ])
            return

        self._json(404, {"error": {"code": "not_found", "message": "no route"}})


# ───────────────────────── 插件进程 + 协议 ─────────────────────────

class Plugin:
    def __init__(self, accounts):
        self.proc = subprocess.Popen(
            [BIN], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            bufsize=1, text=True,
        )
        self.accounts = accounts          # id -> secret
        self.writes = []                  # store.update 的记录
        self.frames = queue.Queue()
        self.next_id = 1
        threading.Thread(target=self._reader, daemon=True).start()
        threading.Thread(target=self._stderr, daemon=True).start()

    def _reader(self):
        for line in self.proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                frame = json.loads(line)
            except Exception:
                continue
            if VERBOSE:
                print("    ← " + line[:400])
            self.frames.put(frame)
        self.frames.put(None)

    def _stderr(self):
        for line in self.proc.stderr:
            if VERBOSE:
                print("    [stderr] " + line.rstrip())

    # 网关侧回调应答
    def _answer_callback(self, frame):
        method = frame.get("method")
        params = frame.get("params") or {}
        reply = {"id": frame["id"]}
        if method == "store.get":
            account = params.get("account") or ""
            secret = self.accounts.get(account)
            if secret is None:
                reply["error"] = {"message": "account not found", "code": "not_found"}
            else:
                reply["result"] = {"id": account, "provider": "wenxin",
                                   "secret": secret, "public": {"name": account}}
        elif method == "store.update":
            self.writes.append({"account": params.get("account"),
                                "expect": params.get("expect"),
                                "secret": params.get("secret")})
            reply["result"] = {"updated": True}
        elif method == "store.list":
            reply["result"] = [{"id": key, "provider": "wenxin", "secret": value,
                                "public": {"name": key}} for key, value in self.accounts.items()]
        elif method == "store.mark_limited":
            reply["result"] = None
        elif method in ("store.ledger_get",):
            reply["error"] = {"message": "no ledger", "code": "not_found"}
        elif method in ("store.ledger_put", "settings.set"):
            reply["result"] = None
        elif method == "settings.get":
            reply["result"] = None
        elif method == "log":
            reply["result"] = None
        else:
            reply["error"] = {"message": "no method " + str(method), "code": "no_method"}
        self._send(reply)

    def _send(self, payload):
        self.proc.stdin.write(json.dumps(payload, ensure_ascii=False) + "\n")
        self.proc.stdin.flush()

    def _next(self, timeout=30):
        frame = self.frames.get(timeout=timeout)
        if frame is None:
            raise RuntimeError("插件进程已退出")
        if frame.get("method"):
            self._answer_callback(frame)
            return self._next(timeout)
        return frame

    def call(self, method, params, timeout=30):
        # 控制面方法都要求 params.provider（SDK 的 provider_of 用它找实现）
        if "provider" not in params:
            params = dict(params, provider="wenxin")
        call_id = self.next_id
        self.next_id += 1
        self._send({"id": call_id, "method": method, "params": params})
        while True:
            frame = self._next(timeout)
            if frame.get("id") != call_id:
                continue
            if frame.get("error"):
                raise RuntimeError(frame["error"].get("message"))
            return frame.get("result")

    def stream(self, method, params, timeout=30):
        """返回 (head, [chunk 文本], 收尾 result)。"""
        call_id = self.next_id
        self.next_id += 1
        self._send({"id": call_id, "method": method, "params": params})
        head, chunks, end = None, [], None
        while True:
            frame = self._next(timeout)
            if frame.get("id") != call_id:
                continue
            if frame.get("event") == "head":
                head = frame
            elif frame.get("event") == "chunk":
                chunks.append(base64.b64decode(frame["data"]).decode())
            elif frame.get("error"):
                raise RuntimeError(frame["error"].get("message"))
            elif "result" in frame:
                end = frame.get("result")
                return head, chunks, end

    def init(self, host_env):
        return self.call("init", {"env": host_env})

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=10)
        except Exception:
            self.proc.kill()


def sse_payloads(chunks):
    """把 chunk 文本切成 OpenAI SSE 帧的 JSON 列表。"""
    out = []
    for blob in chunks:
        for line in blob.splitlines():
            line = line.strip()
            if not line.startswith("data:"):
                continue
            data = line[5:].strip()
            if data == "[DONE]":
                out.append("[DONE]")
                continue
            out.append(json.loads(data))
    return out


# ───────────────────────────── 用例 ─────────────────────────────

def main():
    if not os.path.exists(BIN):
        print("缺少二进制：%s（先 cargo build --release）" % BIN)
        return 2

    upstream = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    port = upstream.server_address[1]
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    base = "http://127.0.0.1:%d" % port
    print("mock 上游：%s" % base)

    api_account = {"mode": "api_key",
                   "apiKey": "bce-v3/ALTAK-SELFTEST/0000000000000",
                   "baseUrl": base + "/v2"}
    bad_account = {"mode": "api_key",
                   "apiKey": "bce-v3/ALTAK-SELFTEST-BAD/0000000000000",
                   "baseUrl": base + "/v2"}
    ak_account = {"mode": "ak_sk", "apiKey": "AKSELFTEST", "secretKey": "SKSELFTEST",
                  "baseUrl": base}
    plugin = Plugin({"acct-api": api_account, "acct-bad": bad_account,
                     "acct-ak": ak_account})

    try:
        host_env = {"data_dir": os.path.join(ROOT, "target", "selftest-data"),
                    "host_version": "3.4.0", "target": "x86_64-unknown-linux-gnu",
                    "proxy": "direct", "debug_traffic": False, "loopback_port": 0}

        # 1. 握手
        infos = plugin.init(host_env)
        info = next((item for item in infos if item.get("id") == "wenxin"), None)
        check("init 返回 wenxin ProviderInfo", info is not None, json.dumps(infos)[:200])
        if info:
            caps = info.get("capabilities") or {}
            check("能力位 chat/stateful/refresh/model_refresh 均为 true",
                  all(caps.get(key) for key in ("chat", "stateful", "refresh", "model_refresh")),
                  json.dumps(caps))
            check("能力位 usage/checkin 为 false（上游没有，见模块头排除过程）",
                  not caps.get("usage") and not caps.get("checkin"), json.dumps(caps))
            check("内置兜底清单非空（API Key 模式静态表）",
                  len(info.get("default_models") or []) > 0,
                  str(len(info.get("default_models") or [])))
            check("申报了 Token 登录方式", bool(info.get("login_methods")))

        # 2. 目录
        listed = plugin.call("models.list", {"account": "acct-api"})
        ids = [row.get("id") for row in listed.get("models", [])]
        check("models.list 非空且含 ernie-5.1", "ernie-5.1" in ids, str(ids[:6]))
        check("models.list 首刷前是静态兜底（meta.remote = false）",
              (listed.get("meta") or {}).get("remote") is False, json.dumps(listed.get("meta")))

        refreshed = plugin.call("models.refresh", {"account": "acct-api", "force": True})
        check("models.refresh 从 /v2/models 拉到清单", refreshed.get("refreshed") is True
              and refreshed.get("count") == 2, json.dumps(refreshed))
        listed = plugin.call("models.list", {"account": "acct-api"})
        ids = [row.get("id") for row in listed.get("models", [])]
        check("远程清单只保留对话模型（过滤 embeddings/offline/非 chat 端点）",
              ids == ["deepseek-v4-pro", "ernie-5.1"], str(ids))
        check("远程清单 meta.remote = true", (listed.get("meta") or {}).get("remote") is True)
        ernie = next((row for row in listed["models"] if row["id"] == "ernie-5.1"), {})
        check("features 含 function_call 才给 supportsToolCall",
              ernie.get("supportsToolCall") is True, json.dumps(ernie))
        check("远程条目的 maxInputTokens/maxOutputTokens 有值",
              ernie.get("maxInputTokens") == 121856 and ernie.get("maxOutputTokens") == 65536,
              json.dumps(ernie))

        # 3. 流式对话
        head, chunks, end = plugin.stream("conversation", {
            "provider": "wenxin", "account": "acct-api", "proxy": None,
            "headers": {}, "stream": True, "session": "selftest",
            "wire_model": "ernie-5.1",
            "body": {"model": "ernie-5.1",
                     "messages": [{"role": "user", "content": "回复两个字"}]},
        })
        check("conversation 先发 head 且状态 200", head is not None and head.get("status") == 200,
              json.dumps(head))
        check("head 声明 OpenAI chat SSE（upstream=chat）",
              (head or {}).get("upstream") == "chat", json.dumps(head))
        payloads = sse_payloads(chunks)
        done = payloads and payloads[-1] == "[DONE]"
        frames = [item for item in payloads if item != "[DONE]"]
        text = "".join(
            (frame.get("choices") or [{}])[0].get("delta", {}).get("content", "")
            for frame in frames if frame.get("choices")
        )
        check("流式帧可逐块消费且拼出正文「你好」", text == "你好", repr(text))
        check("流以 data: [DONE] 正常结束", bool(done), repr(payloads[-2:]))
        usage_frames = [frame for frame in frames if frame.get("usage")]
        check("末帧带非零 usage（include_usage 生效）",
              bool(usage_frames) and usage_frames[-1]["usage"].get("total_tokens") == 11,
              json.dumps(usage_frames[-1] if usage_frames else {}))
        finish = [frame for frame in frames if frame.get("choices")
                  and frame["choices"][0].get("finish_reason")]
        check("有 finish_reason = stop 的收尾帧", bool(finish), json.dumps(frames[-1:]))
        check("插件恒按流式推帧（网关据此聚合非流式）", end is None, repr(end))

        # 4. 上游忽略 stream、整包回 JSON
        head, chunks, end = plugin.stream("conversation", {
            "provider": "wenxin", "account": "acct-api", "proxy": None,
            "headers": {}, "stream": True, "session": "selftest",
            "wire_model": "whole-json",
            "body": {"model": "whole-json",
                     "messages": [{"role": "user", "content": "hi"}]},
        })
        payloads = sse_payloads(chunks)
        frames = [item for item in payloads if item != "[DONE]"]
        text = "".join((frame.get("choices") or [{}])[0].get("delta", {}).get("content", "")
                       for frame in frames if frame.get("choices"))
        check("非 SSE 整包被翻成「正文 + 收尾 + usage」帧流", text == "整包回答",
              repr(text))
        check("整包路径也带 usage", any(frame.get("usage") for frame in frames),
              json.dumps(frames))

        # 5. 上游 401：head 带状态码 + 原始错误体
        head, chunks, end = plugin.stream("conversation", {
            "provider": "wenxin", "account": "acct-bad", "proxy": None,
            "headers": {}, "stream": True, "session": "selftest",
            "wire_model": "ernie-5.1",
            "body": {"model": "ernie-5.1", "messages": [{"role": "user", "content": "hi"}]},
        })
        body = "".join(chunks)
        check("上游 401 时 head.status = 401", (head or {}).get("status") == 401,
              json.dumps(head))
        check("401 的错误体原样交给网关（含 invalid_iam_token）",
              "invalid_iam_token" in body, body[:200])

        # 6. 错误分类（判据取自官方错误码表）
        cases = [
            ("invalid_iam_token", 401,
             {"error": {"code": "invalid_iam_token", "message": "IAM Certification failed",
                        "type": "invalid_request_error"}}, "token_expired"),
            ("rpm_rate_limit_exceeded", 429,
             {"error": {"code": "rpm_rate_limit_exceeded", "message": "Rate limit reached for RPM",
                        "type": "rate_limit_exceeded"}}, "quota_limited"),
            ("system_unsafe", 403,
             {"error": {"code": "system_unsafe", "message": "the content of system field is invalid",
                        "type": "unsafe_request"}}, "content_blocked"),
            ("account_overdue", 403,
             {"error": {"code": "account_overdue", "message": "Access denied due to overdue account",
                        "type": "access_denied"}}, "quota_limited"),
            ("invalid_model(无权限)", 401,
             {"error": {"code": "invalid_model",
                        "message": "The model does not exist or you do not have access to it.",
                        "type": "invalid_request_error"}}, "fatal"),
            ("model_offline", 403,
             {"error": {"code": "model_offline", "message": "The model is offline",
                        "type": "access_denied"}}, "fatal"),
            ("malformed_json", 400,
             {"error": {"code": "malformed_json", "message": "Invalid Argument",
                        "type": "invalid_request_error"}}, "fatal"),
            ("internal_error", 500,
             {"error": {"code": "internal_error", "message": "Internal error",
                        "type": "Internal_error"}}, "fatal"),
            ("v1 error_code 110", 200,
             {"error_code": 110, "error_msg": "Access token invalid or no longer valid"},
             "token_expired"),
            ("v1 error_code 17(日限额)", 200,
             {"error_code": 17, "error_msg": "Open api daily request limit reached"},
             "quota_limited"),
            ("v1 error_code 100(参数)", 200,
             {"error_code": 100, "error_msg": "Invalid parameter"}, "fatal"),
        ]
        for label, status, body, expected in cases:
            got = plugin.call("classify", {"status": status, "body": body})
            kind = (got or {}).get("kind") or list(got.keys())[0]
            check("classify %s → %s" % (label, expected), kind == expected, json.dumps(got))

        # 7. AK/SK 模式：换 token / 临期 / 强制刷新 / V1 对话
        token = plugin.call("token.ensure", {"account": "acct-ak"})
        check("token.ensure 从 oauth 换到 access_token",
              token == "24.selftest.access-token", repr(token))
        check("换到的 token 被比较-再写回账号记录",
              any(write["account"] == "acct-ak"
                  and (write["secret"] or {}).get("accessToken") == "24.selftest.access-token"
                  for write in plugin.writes), json.dumps(plugin.writes[-1:])[:300])
        expiring = plugin.call("token.expiring", {"account": "acct-ak"})
        check("token.expiring 在 30 天有效期内返回 false", expiring is False, repr(expiring))
        # 把记录改成 1 小时后到期 → 应判临期
        plugin.accounts["acct-ak"]["expiresAt"] = 1
        expiring = plugin.call("token.expiring", {"account": "acct-ak"})
        check("token.expiring 在临期窗口内返回 true", expiring is True, repr(expiring))
        forced = plugin.call("token.refresh", {"account": "acct-ak"})
        check("token.refresh 能强制重取（忽略缓存）",
              forced == "24.selftest.access-token", repr(forced))
        api_key = plugin.call("token.ensure", {"account": "acct-api"})
        check("API Key 模式 token.ensure 直接给 Key",
              api_key == api_account["apiKey"], repr(api_key))
        api_expiring = plugin.call("token.expiring", {"account": "acct-api"})
        check("API Key 模式 token.expiring 恒 false（无续期接口）",
              api_expiring is False, repr(api_expiring))

        v1_head, v1_chunks, _ = plugin.stream("conversation", {
            "provider": "wenxin", "account": "acct-ak", "proxy": None,
            "headers": {}, "stream": True, "session": "selftest",
            "wire_model": "ernie-4.0-8k",
            "body": {"model": "ernie-4.0-8k",
                     "messages": [{"role": "user", "content": "回复两个字"}]},
        })
        v1_payloads = sse_payloads(v1_chunks)
        v1_frames = [item for item in v1_payloads if item != "[DONE]"]
        v1_text = "".join((frame.get("choices") or [{}])[0].get("delta", {}).get("content", "")
                          for frame in v1_frames if frame.get("choices"))
        check("V1 对话 result 帧被翻成 OpenAI 帧（正文「你好」）", v1_text == "你好", repr(v1_text))
        check("V1 流式也以 [DONE] 收尾（上游本来没有）",
              v1_payloads and v1_payloads[-1] == "[DONE]", repr(v1_payloads[-2:]))
        check("V1 流式带 usage 帧",
              any(frame.get("usage") for frame in v1_frames), json.dumps(v1_frames[-1:]))
        check("V1 head 200", (v1_head or {}).get("status") == 200, json.dumps(v1_head))

        # 8. 账号规范化
        normalized = plugin.call("account.normalize", {
            "source": {"kind": "manual",
                       "payload": {"apiKey": api_account["apiKey"],
                                   "baseUrl": base + "/v2", "name": "我的文心"}},
            "name": None, "proxy": None,
        })
        public = normalized.get("public") or {}
        secret = normalized.get("secret") or {}
        check("account.normalize 落库 secret 含 apiKey",
              secret.get("apiKey") == api_account["apiKey"], json.dumps(secret))
        check("公开形态不含凭证本体（无 apiKey/secretKey/accessToken）",
              not any(key in public for key in ("apiKey", "secretKey", "accessToken")),
              json.dumps(public))
        check("公开形态只给 tokenTail（尾 4 字符）",
              public.get("tokenTail") == api_account["apiKey"][-4:], json.dumps(public))
        check("identity 由模式 + 凭证指纹生成（wenxin- 前缀）",
              str(normalized.get("identity", "")).startswith("wenxin-"),
              json.dumps(normalized.get("identity")))
        try:
            plugin.call("account.normalize", {
                "source": {"kind": "manual",
                           "payload": {"apiKey": "bce-v3/ALTAK-SELFTEST-BAD/0000",
                                       "baseUrl": base + "/v2"}},
                "name": None, "proxy": None,
            })
            check("API Key 被上游 401 时拒绝落库", False, "居然成功了")
        except RuntimeError as error:
            check("API Key 被上游 401 时拒绝落库", "401" in str(error) or "拒绝" in str(error),
                  str(error))
        ak_norm = plugin.call("account.normalize", {
            "source": {"kind": "manual",
                       "payload": {"apiKey": "AKSELFTEST2", "secretKey": "SKSELFTEST2",
                                   "baseUrl": base}},
            "name": None, "proxy": None,
        })
        ak_secret = ak_norm.get("secret") or {}
        check("AK/SK 模式规范化时预热 access_token",
              ak_secret.get("accessToken") == "24.selftest.access-token",
              json.dumps(ak_secret))
        check("AK/SK 公开形态标明认证方式",
              (ak_norm.get("public") or {}).get("authMode") == "AK/SK",
              json.dumps(ak_norm.get("public")))

    finally:
        plugin.close()
        upstream.shutdown()

    print("\n结果：%d 项通过，%d 项失败" % (len(PASS), len(FAIL)))
    if FAIL:
        print("失败项：" + "；".join(FAIL))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
