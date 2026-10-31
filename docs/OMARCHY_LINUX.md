# 在 Omarchy / Arch Linux 上从源码部署 Agent2API

本文记录在本机 **Omarchy 4.0.6（Arch 系，x86_64，kernel 7.2.11-arch1-1）** 上，
从 GitHub fork 源码构建并以 Docker 运行 Agent2API 的实测步骤。

> 结论先说：
>
> - **已验证**：`agent2api-server` 的 Docker 单容器部署（面板 + OpenAI 兼容网关）可以在这台机器上从源码构建、启动、登录、持久化和调用真实上游模型。
> - **未在本次验收中构建**：Tauri 原生桌面窗口（`deb` / `AppImage`）。仓库已有 Linux 打包配置，系统也具备依赖，但“原生桌面已适配并验证”这句话不能由容器部署推导出来；两者是不同的运行形态。原生桌面的依赖与构建命令见文末。
> - 默认部署把端口发布到 **127.0.0.1**，不会把面板或 API 暴露到局域网/公网；需要远程访问时必须显式改绑定地址并自行加防火墙。

## 1. 这台机器的实测环境

| 项目 | 值 |
| --- | --- |
| 发行版 | Omarchy 4.0.6，`ID_LIKE=arch` |
| 架构 | x86_64 |
| 内核 | 7.2.11-arch1-1 |
| Docker Engine | 29.9.2 |
| Docker Compose | 5.6.1 |
| Git | 2.56.0 |
| GitHub CLI | 2.102.0 |
| 当前用户 | `fish`（uid/gid 1000，在 `docker` 组） |
| 本机已占用端口 | `3065` 有旧的 `agent2api` v3.1.0 容器；本 fork 使用 `3066` |

检查命令：

```bash
cat /etc/os-release
uname -m
uname -r
docker version
docker compose version
git --version
gh --version
ss -ltnp | grep -E ':3065|:3066' || true
```

## 2. 前置依赖

Docker 部署只需要：

```bash
# Arch / Omarchy
sudo pacman -S --needed git docker docker-compose curl jq sqlite
sudo systemctl enable --now docker   # 若使用 systemd 的 docker.service
sudo usermod -aG docker "$USER"      # 首次加入后需重新登录
```

验证：

```bash
docker run --rm hello-world
docker compose version
```

> 本机的 `docker.service` 当前显示为 `disabled` 但 Docker 守护进程在运行；这说明它是被 socket/其他方式拉起的。若希望重启后自动恢复，执行上面的 `systemctl enable --now docker`。不要在没有确认的情况下停止已有的 Docker 容器。

## 3. 取得源码与分支

Fork 地址（本机实际使用）：

```bash
git clone https://github.com/free6589/Agent2API.git
cd Agent2API
git remote -v
# origin   https://github.com/free6589/Agent2API.git
# upstream https://github.com/aimod-cc/Agent2API.git
git switch feat/omarchy-local-deploy
```

如果从零开始复现本分支：

```bash
git fetch upstream beta
git switch -c feat/omarchy-local-deploy upstream/beta
```

本分支基线是 upstream `beta` 的 `bd36938d7cc32f2a372a63e57f3aec873cf53f11`（v3.3.0-beta.6 开发线）。
`main` 是旧稳定线，不能拿 `main` 的 Docker 行为推断 beta 的插件架构。

## 4. 从源码构建镜像

### 4.1 正常网络

```bash
docker compose build
# 或
docker build -t agent2api-fork:local .
```

### 4.2 本机实测：Docker bridge 访问 GitHub 超时

这台机器的 Docker bridge 访问 GitHub 会超时，而宿主网络正常。构建阶段 Cargo 需要拉取
`ProvidersPlugin/Agent2Providers` 的 git 依赖，所以直接 `docker build` 会失败：

```text
spurious network error ... Timeout ... github.com/ProvidersPlugin/Agent2Providers
```

已验证的解决方式是让**构建阶段**走宿主网络：

```bash
docker build --network host -t agent2api-fork:local .
```

镜像构建成功后应看到类似：

```bash
docker images agent2api-fork:local
# REPOSITORY             TAG       IMAGE ID       ...
# agent2api-fork:local   latest    c26255f950b2   ...
```

> 运行时不一定需要 `--network host`。本机最终容器使用普通 bridge 网络，真实上游 API 可正常访问；只有 GitHub 在容器内不可达。若要在容器内安装/更新插件且 GitHub 不通，见第 9 节。

## 5. 首次启动

仓库根目录的 `docker-compose.yml` 已改为：

- 默认只发布到 `127.0.0.1`；
- 支持通过 `.env` 设置端口、容器名、uid/gid；
- 支持预置管理员和第一把网关 Key；
- 数据固定落在 `./data`。

创建本地 `.env`（**不要提交**；`.gitignore` 已覆盖）：

```bash
umask 077
cat > .env <<'ENV'
COMPOSE_PROJECT_NAME=agent2api-fork
AGENT2API_IMAGE=agent2api-fork:local
AGENT2API_CONTAINER_NAME=agent2api-fork
AGENT2API_BIND_ADDRESS=127.0.0.1
AGENT2API_HOST_PORT=3066
AGENT2API_UID=1000
AGENT2API_GID=1000
AGENT2API_ADMIN_USER=admin
AGENT2API_ADMIN_PASSWORD=请替换为至少8位的随机密码
AGENT2API_PROXY_API_KEY=sk-请替换为随机Key
AGENT2API_SELF_UPDATE=0
AGENT2API_CAPTCHA_ENABLED=1
ENV
chmod 600 .env
```

说明：

- `AGENT2API_HOST_PORT=3066` 是为了避开本机已有的 3065 旧容器。正常新机器直接用
  `AGENT2API_HOST_PORT=3065` 即可。
- `AGENT2API_UID/GID=1000` 让容器以当前用户运行，`./data` 里的文件宿主可读、可备份；
  同时因为 `AGENT2API_SELF_UPDATE=0`，面板的“容器内自更新”不会因非 root 权限失败。
- 不设置 `AGENT2API_ADMIN_USER/PASSWORD` 时，第一次打开面板会进入“创建管理员账号”流程；
  两种方式二选一，不要同时使用。
- 不设置 `AGENT2API_PROXY_API_KEY` 时，`/v1/chat/completions` 等模型调用在创建第一把
  Key 前会 fail-closed（401），这是预期行为，不要用 `AGENT2API_ALLOW_NO_KEY=1` 绕过。

启动：

```bash
docker compose up -d
docker compose ps
```

本机实际结果：

```text
NAME             IMAGE                  STATUS                   PORTS
agent2api-fork   agent2api-fork:local   Up ... (healthy)         127.0.0.1:3066->3065/tcp
```

容器内监听 `0.0.0.0:3065`，宿主只发布到 `127.0.0.1:3066`；这是“容器内监听”和
“宿主暴露面”两件事，不要混淆。

## 6. 面板、管理员与登录

打开：

```text
http://127.0.0.1:3066/
```

- 首次使用（没有预置管理员）：页面提示“首次使用：创建一个管理员账号”，设置用户名和至少 8 位密码。
- 已预置管理员：直接输入 `.env` 中的账号密码。
- 登录页的 ALTCHA 人机验证默认开启；如果页面显示 “Verified”，直接提交即可。
- 账号密码只在 `.env` 和 SQLite 的 bcrypt 哈希中；`.env` 权限为 600，不要提交。

本机实际验证：

```bash
curl -s http://127.0.0.1:3066/api/panel/status
# {"data":{"registered":true},"success":true}

curl -s http://127.0.0.1:3066/health
# {"status":"ok"}
```

浏览器自动化（Playwright）也验证过：登录页能加载、ALTCHA 完成、提交后进入概览页，
且能够读取账号列表（8 个账号、74 个模型由旧数据迁移而来）。

## 7. 调用 OpenAI 兼容 API

先从 `.env` 读取 Key（不要把 Key 写进 shell 历史或提交）：

```bash
export A2A_KEY="$(grep '^AGENT2API_PROXY_API_KEY=' .env | cut -d= -f2-)"
export A2A_BASE="http://127.0.0.1:3066"
```

健康检查与模型列表：

```bash
curl -s "$A2A_BASE/health"
curl -s -H "Authorization: Bearer $A2A_KEY" "$A2A_BASE/v1/models" | jq '.meta, (.data|length)'
```

非流式对话：

```bash
curl -s -X POST "$A2A_BASE/v1/chat/completions" \
  -H "Authorization: Bearer $A2A_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "glm-5.3",
    "messages": [{"role":"user","content":"Reply with exactly: pong"}],
    "max_tokens": 32,
    "stream": false
  }' | jq .
```

流式对话：

```bash
curl -N -X POST "$A2A_BASE/v1/chat/completions" \
  -H "Authorization: Bearer $A2A_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "glm-5.3",
    "messages": [{"role":"user","content":"Say pong"}],
    "stream": true,
    "max_tokens": 32
  }'
```

本机实际验收结果：

```text
POST /v1/chat/completions  model=glm-5.3  HTTP 200
content="pong"  prompt_tokens=50  completion_tokens=3  total_tokens=53
```

这证明真实上游调用（不是 mock）通过本 fork 的容器成功完成。测试 mock 上游也验证过
非流式与 SSE 流式路径；真实调用会比 mock 多消耗少量额度。

无 Key 调用模型必须被拒绝：

```bash
curl -s -o /dev/null -w '%{http_code}\n' -X POST "$A2A_BASE/v1/chat/completions" \
  -H 'Content-Type: application/json' \
  -d '{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}]}'
# 401
```

`/v1/models` 在没有任何 Key 的空白实例上会返回空列表（200），这是目录接口的既有行为；
真正需要鉴权的是模型调用接口。

## 8. 数据、备份与恢复

所有状态都在 `./data`：

```text
data/
├── agent2api.db        # SQLite：账号、Key、设置、请求记录
├── account.key         # 加密账号字段的密钥（32 字节，600）
├── plugins/            # 已安装的提供商插件与模型缓存
└── debug/              # 可选调试产物
```

权限建议：

```bash
chmod 700 data
chmod 600 data/account.key data/agent2api.db
```

**一致性备份（推荐，先停服务）：**

```bash
docker compose stop
tar -C . -czf "agent2api-backup-$(date +%F-%H%M%S).tar.gz" data
docker compose start
```

**在线备份（不中断服务，但必须同时备份 account.key）：**

```bash
sqlite3 data/agent2api.db ".backup 'agent2api-live.db'"
cp -p data/account.key "agent2api-live.account.key"
```

只复制 `agent2api.db` 而漏掉 `account.key`，恢复后账号密文无法解密。

**恢复：**

```bash
docker compose stop
mv data "data.before-restore-$(date +%s)"   # 不删除旧数据
mkdir -p data
tar -C . -xzf agent2api-backup-YYYY-MM-DD-HHMMSS.tar.gz
chown -R 1000:1000 data
chmod 700 data
chmod 600 data/account.key data/agent2api.db
docker compose start
```

不要把本机的 `./data`、`.env`、`account.key` 提交到 Git 或上传到公开网盘。

## 9. 日志、停止、重启、升级

```bash
docker compose logs -f --tail=200     # 跟踪日志
docker compose ps                     # 状态
docker compose restart                # 重启（数据保留）
docker compose stop                   # 停止（数据保留）
docker compose down                   # 删除容器和网络，数据保留
docker compose up -d                  # 再启动
```

升级（源码方式，推荐）：

```bash
git fetch upstream beta
git switch feat/omarchy-local-deploy
git rebase upstream/beta        # 或 merge，按团队约定
docker build --network host -t agent2api-fork:local .
docker compose up -d
```

升级前先备份 `./data`。`AGENT2API_SELF_UPDATE=0` 时，面板“软件更新”不会在容器内
替换二进制；要更新就重新构建镜像。若确实要使用面板内更新，把该变量设为 1 并以 root
运行容器，同时接受“镜像标签不变、容器可写层被替换”的既有语义。

## 10. 常见问题

### 10.1 3065 已被占用

```bash
ss -ltnp | grep 3065
docker ps --filter publish=3065
```

不要为了启动 fork 去停止或覆盖正在工作的旧容器。把 `.env` 的
`AGENT2API_HOST_PORT` 改成 3066（或其它空闲端口），只影响宿主发布端口。

### 10.2 构建时 Cargo 拉 GitHub 超时

使用：

```bash
docker build --network host -t agent2api-fork:local .
```

如果仍失败，检查宿主：

```bash
git ls-remote https://github.com/ProvidersPlugin/Agent2Providers
```

### 10.3 容器内 GitHub 不通，插件安装/更新失败

本机运行容器使用 bridge 网络，GitHub 不可达；但已安装的插件会从 `data/plugins`
加载，不影响已迁移账号与模型。需要在线安装/更新插件时，可以：

1. 在宿主机下载插件 zip，通过面板“插件 → 上传安装”安装；或
2. 临时用 `--network host` 启动容器完成安装，再恢复普通 bridge 运行。

不要为了插件更新把面板端口发布到 `0.0.0.0`。

### 10.4 日志反复出现 `Clash 出口不可用`

这是出站代理探测的降级提示：本机没有 Clash Verge 配置，网关自动回退直连。它不影响
真实上游调用（本机已验证 `glm-5.3` 返回 200）。如果不需要 Clash，可在设置里关闭相关
探测；不要为此改认证或关闭网络策略。

### 10.5 登录后页面空白 / 401

- 确认访问的是 `http://127.0.0.1:3066/`，不是 `https` 或旧容器端口；
- 清掉该站点的旧 cookie 后重试；
- 查看 `docker compose logs --tail=100`，确认没有 `panelAdmin` 初始化失败；
- 确认 `.env` 的 `AGENT2API_UID/GID` 与 `data/` 所有者一致。

### 10.6 数据目录权限

容器以 `.env` 的 uid/gid 运行。若从旧机器复制数据后出现 `unable to open database file`：

```bash
chown -R 1000:1000 data
chmod 700 data
chmod 600 data/account.key data/agent2api.db
docker compose restart
```

### 10.7 从旧 v3.1.0 数据迁移

本机做过一次**复制迁移**（原 `/home/fish/data` 与旧 3065 容器保持不动）：

1. 停止新容器；
2. 从旧容器复制 `/data` 到临时目录；
3. 在新副本上删除旧的 `panelAdmin` 会话键，让新 `.env` 重新播种管理员；
4. 启动新容器，确认 8 个账号、插件、74 个模型被加载；
5. 用新管理员登录并调用真实模型。

迁移成功的关键是 `account.key` 与 `agent2api.db` 必须成对复制；插件目录也要一起复制，
否则 beta 的插件家账号会显示为不可用。不要把旧数据目录直接挂给新容器做原地升级，先复制
并验证，确认后再考虑切换。

## 11. 原生 Tauri 桌面：差异与未验证项

仓库 `desktop-tauri/` 是 Tauri 2 桌面壳，`desktop-tauri/src-tauri/tauri.linux.conf.json`
已声明 Linux 打包目标：

```json
{"bundle": {"targets": ["deb", "appimage"]}}
```

Arch/Omarchy 上构建原生桌面通常需要：

```bash
sudo pacman -S --needed base-devel webkit2gtk-4.1 gtk3 libayatana-appindicator \
  librsvg patchelf openssl file
```

构建（需要 Node/npm 与 Rust；本机 Rust 在 `~/.cargo/bin`，默认 PATH 可能没有）：

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cd desktop-tauri
npm install
npm run build
# 或显式使用 Linux 配置：
# npx tauri build --config src-tauri/tauri.linux.conf.json
```

本次**没有**把原生桌面构建作为验收项，原因是：容器形态已经完整验证，而原生桌面还要额外
处理 WebKitGTK/托盘/打包与桌面窗口的跨源登录（`tauri://localhost` 与
`127.0.0.1:3065` 不同源）。因此：

- **容器部署**：已验证可用，推荐长期部署方式；
- **原生桌面**：代码与依赖具备，但未在本次验收中构建/运行，不能声称已适配完成。

如果后续要验证桌面形态，先单独构建 `deb`/`AppImage`，再验证登录页在 `tauri://` 源下
能通过 `window.workbuddyDesktop` 访问网关，而不是把浏览器的 `http://127.0.0.1` 结果
当成桌面结果。

### 11.1 原生 headless server（systemd）说明

仓库的 `packaging/linux/install.sh` 与 systemd unit 走的是 `agent2api-server`，它不链接
Tauri/WebKitGTK，理论上比桌面壳依赖更少。但该 unit 使用独立的 `agent2api` 系统用户并设置
`ProtectHome=true`，因此**无法读取当前桌面用户的 `~/.config` 登录态**；依赖“导入本机客户端
登录态”的 provider 在这条路径上会失败。需要这类账号时，优先用 Docker（可挂载所需目录并
显式控制权限）或在面板里粘贴凭证，不要直接放宽 systemd 的 `ProtectHome`。

本次验收没有安装 systemd 服务；Docker 是已验证的长期运行方式。若要用 systemd，先在一台
测试机确认 unit 的数据目录、端口、`ProtectHome` 与账号导入需求，再切换生产。

## 12. 本次验收的可复现命令清单

```bash
# 1. 构建
docker build --network host -t agent2api-fork:local .

# 2. 启动
docker compose up -d
docker compose ps

# 3. 健康与面板
curl -s http://127.0.0.1:3066/health
curl -s http://127.0.0.1:3066/api/panel/status

# 4. Key 与模型
export A2A_KEY="$(grep '^AGENT2API_PROXY_API_KEY=' .env | cut -d= -f2-)"
curl -s -H "Authorization: Bearer $A2A_KEY" http://127.0.0.1:3066/v1/models | jq '.meta'

# 5. 真实对话
curl -s -X POST http://127.0.0.1:3066/v1/chat/completions \
  -H "Authorization: Bearer $A2A_KEY" -H 'Content-Type: application/json' \
  -d '{"model":"glm-5.3","messages":[{"role":"user","content":"Reply with exactly: pong"}],"max_tokens":32}' | jq '{model,choices,usage}'

# 6. 无 Key 必须 401
curl -s -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:3066/v1/chat/completions \
  -H 'Content-Type: application/json' -d '{"model":"glm-5.3","messages":[{"role":"user","content":"hi"}]}'

# 7. 重启持久化
docker compose restart
curl -s -H "Authorization: Bearer $A2A_KEY" http://127.0.0.1:3066/v1/models | jq '.meta.count'
```

## 13. 安全边界

- 默认 `127.0.0.1` 绑定；不要为了远程访问直接改成 `0.0.0.0` 而不加认证/防火墙。
- 面板管理员和网关 Key 分离：面板登录不等于模型调用授权。
- `.env`、`data/account.key`、`data/agent2api.db` 都是敏感文件，权限 600/700。
- 不在 Issue、PR、日志、截图中提交真实 Key、Cookie、账号密码。
- 旧 3065 容器与 `/home/fish/data` 在本次部署中保持原样；新部署使用独立容器、独立数据副本和 3066 端口。

---

最后更新：2026-11-01。环境与结果以本机实际命令输出为准；上游行为可能随版本变化。
