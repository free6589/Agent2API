#!/usr/bin/env bash
# Agent2API 本地 Docker 部署辅助脚本（Arch 系：Omarchy / EndeavourOS / Manjaro，其它发行版通用）
#
# 适配 3.4.x 的仓库结构：**服务名、发布端口、数据卷路径全部从 `docker compose config`
# 读出来**，不假设服务一定叫 agent2api、宿主端口一定是 3065、数据一定在 ./data。
# 默认用 scripts/omarchy-compose.yml（端口默认只绑回环的加固版）；换宿主端口、
# 预置网关 Key 等都在仓库根 .env 里改（见 docs/install.md 的 Docker 一节）。
#
# 安全边界（脚本自己的约束）：
#   · 不删除容器以外的任何东西；down/stop 保留 ./data
#   · 不读取、不回显 .env 与环境变量的**值**（只看是否存在、权限、变量名）
#   · 不访问数据库内容（备份是整目录 tar，不打开库文件）
#   · 任何需要停服务的动作（backup / restore）都在 trap 里保证服务被重新拉起
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

readonly HEALTH_TIMEOUT="${AGENT2API_HEALTH_TIMEOUT:-60}"

usage() {
  cat <<'USAGE'
用法：scripts/omarchy-agent2api.sh <命令> [参数]

  up [--build]     启动服务（--build 先重建镜像）；启动后等待 /health 通过
  build [--bridge] 构建镜像。默认走宿主网络（--network host，绕开 Docker bridge
                   访问 GitHub 超时）；--bridge 用普通 bridge 构建。构建后用 up 启动
  down             停止并删除容器/网络（数据卷保留）
  restart          重启服务，并等待 /health 通过
  status           容器状态、宿主端口与绑定地址、/health、数据目录、镜像
  logs             跟踪日志（--tail=200 起）
  health           只打 /health
  backup [目录]     停服务 → 打包数据目录 → 起服务；默认写到 ./backups
  restore <归档>    停服务 → 现有数据改名留档 → 解包 → 起服务（会二次确认）
  doctor           环境自检：依赖、守护进程、端口占用、数据目录权限、磁盘、出网
  config           打印部署摘要（服务名 / 镜像 / 端口 / 数据目录 / 环境变量名）
  help             本帮助

环境变量（都可选）：
  AGENT2API_COMPOSE_FILE   compose 文件（默认 scripts/omarchy-compose.yml；
                           相对路径按仓库根解析，项目目录取文件所在目录）
  AGENT2API_DATA_DIR       覆盖数据目录（默认从 compose 的 /data 挂载点读出）
  AGENT2API_HEALTH_TIMEOUT /health 等待秒数（默认 60）
USAGE
}

note() { printf '%s\n' "$*"; }
warn() { printf '警告：%s\n' "$*" >&2; }
die()  { printf '错误：%s\n' "$*" >&2; exit 1; }

require() {
  command -v "$1" >/dev/null 2>&1 || die "缺少命令：$1（Arch：sudo pacman -S $2）"
}

have() { command -v "$1" >/dev/null 2>&1; }

# 项目目录固定在仓库根：.env 插值与 compose 里的相对路径（./data）都从这里解析。
compose() {
  if [ -n "${AGENT2API_COMPOSE_FILE:-}" ]; then
    local file="$AGENT2API_COMPOSE_FILE"
    case "$file" in /*) ;; *) file="$ROOT/$file" ;; esac
    docker compose --project-directory "$(cd "$(dirname "$file")" && pwd)" -f "$file" "$@"
  else
    docker compose --project-directory "$ROOT" -f "$ROOT/scripts/omarchy-compose.yml" "$@"
  fi
}

# ── 从 compose 读事实（不猜）──────────────────────────────────────────────

service_name() {
  local name
  name="$(compose config --services 2>/dev/null | head -n 1 || true)"
  [ -n "$name" ] || die "读不出 compose 服务名（先确认 docker-compose.yml 存在且可解析）"
  printf '%s' "$name"
}

# 网关端口：容器内监听端口，取 compose ports 映射的 target（默认 3065）
gateway_port() {
  local target=""
  if have jq; then
    target="$(compose config --format json 2>/dev/null \
      | jq -r '[ .services[]?.ports[]? | select((.protocol // "tcp") == "tcp") | .target ]
               | map(select(. != null)) | .[0] // empty' 2>/dev/null || true)"
  fi
  printf '%s' "${target:-3065}"
}

# 宿主发布端口：优先取 target 与网关端口一致的那条映射
published_port() {
  local want port=""
  want="$(gateway_port)"
  if have jq; then
    port="$(compose config --format json 2>/dev/null \
      | jq -r --arg want "$want" '
          [ .services[]?.ports[]?
            | select((.protocol // "tcp") == "tcp")
            | select((.target | tostring) == $want) | .published ]
          + [ .services[]?.ports[]? | select((.protocol // "tcp") == "tcp") | .published ]
          | map(select(. != null and . != "")) | .[0] // empty' 2>/dev/null || true)"
  fi
  if [ -z "$port" ]; then
    port="$(compose config 2>/dev/null \
      | awk '/^[[:space:]]*published:/{gsub(/["'\'']/,"",$2); print $2; exit}' || true)"
  fi
  [ -n "$port" ] || die "读不出宿主发布端口（compose 里没有 ports 映射？）"
  printf '%s' "$port"
}

# 数据目录：compose 里挂到容器 /data 的 bind source；读不到就回落 ./data
data_dir() {
  if [ -n "${AGENT2API_DATA_DIR:-}" ]; then
    printf '%s' "$AGENT2API_DATA_DIR"
    return
  fi
  local dir=""
  if have jq; then
    dir="$(compose config --format json 2>/dev/null \
      | jq -r '[ .services[]?.volumes[]? | select(.target == "/data") | .source ] | .[0] // empty' 2>/dev/null || true)"
  fi
  [ -n "$dir" ] || dir="$ROOT/data"
  printf '%s' "$dir"
}

# 容器内用户（compose 没写 user: 就是镜像默认的 root）
container_user() {
  local user=""
  if have jq; then
    user="$(compose config --format json 2>/dev/null \
      | jq -r '[ .services[]?.user ] | map(select(. != null and . != "")) | .[0] // empty' 2>/dev/null || true)"
  fi
  printf '%s' "${user:-root}"
}

image_ref() {
  local image=""
  if have jq; then
    image="$(compose config --format json 2>/dev/null \
      | jq -r '[ .services[]?.image ] | map(select(. != null and . != "")) | .[0] // empty' 2>/dev/null || true)"
  fi
  printf '%s' "${image:-unknown}"
}

# ── 运行态 ────────────────────────────────────────────────────────────────

running() {
  [ -n "$(compose ps -q 2>/dev/null | head -n 1)" ]
}

wait_health() {
  local port deadline
  port="$(published_port)"
  deadline=$((SECONDS + HEALTH_TIMEOUT))
  note "等待 http://127.0.0.1:${port}/health …"
  while [ "$SECONDS" -lt "$deadline" ]; do
    if curl -fsS --max-time 3 "http://127.0.0.1:${port}/health" >/dev/null 2>&1; then
      note "健康检查通过（${SECONDS}s）：$(curl -fsS --max-time 3 "http://127.0.0.1:${port}/health")"
      return 0
    fi
    sleep 2
  done
  warn "等待 ${HEALTH_TIMEOUT}s 仍未通过 /health，看日志：scripts/omarchy-agent2api.sh logs"
  return 1
}

# 备份前把服务停掉、无论成败都拉起来
start_after() {
  compose start >/dev/null 2>&1 || compose up -d >/dev/null 2>&1 || true
}

# ── 命令 ─────────────────────────────────────────────────────────────────

cmd_build() {
  local mode="${1:-}"
  local image="${AGENT2API_IMAGE:-agent2api:local}"
  case "$mode" in
    --bridge) docker build -t "$image" "$ROOT" ;;
    "")       docker build --network host -t "$image" "$ROOT" ;;
    *)        die "build 只接受 --bridge" ;;
  esac
  note "镜像已构建：$image；用 up 启动。"
}

cmd_up() {
  local build="${1:-}"
  require docker docker
  case "$build" in
    --build) compose up -d --build ;;
    "")      compose up -d ;;
    *)       die "up 只接受 --build" ;;
  esac
  compose ps
  wait_health || true
}

cmd_down() {
  require docker docker
  compose down
  note "容器与网络已删除；数据目录 $(data_dir) 保留。"
}

cmd_restart() {
  require docker docker
  compose restart
  compose ps
  wait_health || true
}

cmd_status() {
  require docker docker
  local port svc dir user
  svc="$(service_name)"
  port="$(published_port)"
  dir="$(data_dir)"
  user="$(container_user)"
  note "项目目录 : $ROOT"
  note "服务名   : $svc"
  note "镜像     : $(image_ref)"
  note "宿主端口 : $port（容器内 $(gateway_port)）"
  note "数据目录 : $dir"
  note "容器用户 : $user"
  note ""
  compose ps || true
  note ""
  if running; then
    note "宿主监听："
    ss -ltnp 2>/dev/null | grep -E ":${port}\b" || note "  （ss 没看到 :$port，可能被其它方式占用或未就绪）"
    note ""
    note "运行版本（/health）："
    if body="$(curl -fsS --max-time 5 "http://127.0.0.1:${port}/health")"; then
      note "$body"
    else
      note "  /health 不通"
    fi
    note ""
    note "镜像标签 vs 实际运行版本：面板「设置 → 软件更新」里的容器内更新会让两者漂移，"
    note "执行 docker compose pull && docker compose up -d 或本脚本 up --build 可回到镜像版本。"
  else
    note "容器未运行（scripts/omarchy-agent2api.sh up 启动）"
  fi
}

cmd_logs() {
  require docker docker
  compose logs -f --tail=200
}

cmd_health() {
  local port
  port="$(published_port)"
  curl -fsS --max-time 5 "http://127.0.0.1:${port}/health" && note ""
}

cmd_backup() {
  require docker docker
  require tar tar
  local dir out stamp size
  dir="$(data_dir)"
  [ -d "$dir" ] || die "数据目录不存在：$dir（先 up 起一次服务）"
  out="${1:-$ROOT/backups}"
  mkdir -p "$out"
  chmod 700 "$out" 2>/dev/null || warn "无法把 $out 权限改成 700（属主不是你？用 sudo 重跑）"
  stamp="$(date +%Y%m%d-%H%M%S)"
  local archive="$out/agent2api-data-${stamp}.tar.gz"
  note "停服务 → 打包 → 起服务（保证 SQLite/WAL 一致）"
  compose stop
  trap 'start_after' EXIT INT TERM
  tar -C "$(dirname "$dir")" -czf "$archive" "$(basename "$dir")"
  trap - EXIT INT TERM
  start_after
  chmod 600 "$archive" 2>/dev/null || true
  size="$(du -h "$archive" | cut -f1)"
  note "备份完成：$archive（$size）"
  note "包内关键文件："
  local listing
  listing="$(tar -tzf "$archive")" || warn "归档清单读取失败"
  printf '%s\n' "$listing" | grep -E '/(agent2api\.db|account\.key)$' || warn "归档里没看到 agent2api.db / account.key，确认数据目录是否正确"
  wait_health || true
}

cmd_restore() {
  require docker docker
  require tar tar
  local archive="${1:-}"
  [ -n "$archive" ] || die "用法：restore <归档路径>"
  [ -f "$archive" ] || die "归档不存在：$archive"
  local dir stamp keep
  dir="$(data_dir)"
  stamp="$(date +%Y%m%d-%H%M%S)"
  keep="${dir}.before-restore-${stamp}"
  note "将把 $archive 解到 $dir"
  note "现有数据先改名保留为 $keep（不删除）"
  if [ -t 0 ]; then
    printf '继续？输入 yes 确认：'
    local answer
    read -r answer
    [ "$answer" = "yes" ] || die "已取消"
  else
    note "非交互环境，跳过确认（调用方已明确传入归档）"
  fi
  compose stop
  trap 'start_after' EXIT INT TERM
  if [ -e "$dir" ]; then
    mv "$dir" "$keep"
  fi
  mkdir -p "$(dirname "$dir")"
  tar -C "$(dirname "$dir")" -xzf "$archive"
  [ -d "$dir" ] || die "解包后没有 $dir，检查归档结构"
  trap - EXIT INT TERM
  start_after
  note "恢复完成；旧数据保留在 $keep"
  wait_health || true
}

cmd_doctor() {
  local fail=0
  note "== 依赖 =="
  local cmd
  for cmd in docker tar curl; do
    if have "$cmd"; then
      note "  [PASS] $cmd → $(command -v "$cmd")"
    else
      note "  [FAIL] 缺少 $cmd"
      fail=1
    fi
  done
  for cmd in jq ss git; do
    if have "$cmd"; then
      note "  [PASS] $cmd → $(command -v "$cmd")"
    else
      note "  [WARN] 缺少 $cmd（$cmd 仅用于更好的解析/排查，可缺）"
    fi
  done
  note ""
  note "== Docker =="
  if have docker && docker info >/dev/null 2>&1; then
    note "  [PASS] docker 守护进程可达（$(docker version --format '{{.Server.Version}}' 2>/dev/null || echo '版本未知')）"
    note "  [INFO] compose 版本：$(docker compose version --short 2>/dev/null || echo '未知')"
    if docker info --format '{{json .SecurityOptions}}' 2>/dev/null | grep -q rootless; then
      note "  [INFO] rootless Docker：数据目录归属会随 dockerd 用户变化"
    fi
  else
    note "  [FAIL] docker 守护进程不可达（systemctl enable --now docker）"
    fail=1
  fi
  if [ -f "$ROOT/docker-compose.yml" ]; then
    note "  [PASS] docker-compose.yml 存在"
  else
    note "  [FAIL] 当前目录没有 docker-compose.yml"
    fail=1
  fi
  note ""
  note "== 部署事实（从 compose 读）=="
  local svc port dir user
  svc="$(service_name 2>/dev/null || echo '?')"
  port="$(published_port 2>/dev/null || echo '?')"
  dir="$(data_dir)"
  user="$(container_user)"
  note "  服务名=$svc 宿主端口=$port 数据目录=$dir 容器用户=$user"
  note ""
  note "== 端口占用 =="
  if [ "$port" != "?" ] && ss -ltn 2>/dev/null | grep -qE ":${port}\b"; then
    if running; then
      note "  [PASS] :$port 由本项目的容器占用"
    else
      note "  [FAIL] :$port 被别的进程占用（ss -ltnp | grep :$port）；在 .env 里改 AGENT2API_HOST_PORT 换宿主端口"
      fail=1
    fi
  else
    note "  [PASS] :$port 当前空闲"
  fi
  note ""
  note "== 数据目录 =="
  if [ -d "$dir" ]; then
    note "  [PASS] 存在：$dir（$(stat -c '%U:%G %a' "$dir" 2>/dev/null || echo '权限未知')）"
    local owner
    owner="$(stat -c '%U' "$dir" 2>/dev/null || echo '')"
    local is_root=0
    case "$user" in root | "0" | "0:"*) is_root=1 ;; esac
    if [ "$is_root" -eq 1 ] && [ -n "$owner" ] && [ "$owner" != "root" ] && [ "$owner" != "0" ]; then
      note "  [WARN] 容器以 root 运行，目录属主是 $owner —— 容器会以 root 写文件；"
      note "         之后手工 tar/删文件可能需要 sudo。要避免就在 compose 里加 user: \"\$(id -u):\$(id -g)\" 并先 chown。"
    fi
    note "  [INFO] 占用：$(du -sh "$dir" 2>/dev/null | cut -f1 || echo '未知')"
  else
    note "  [INFO] 还不存在（首次 up 会创建）：$dir"
  fi
  note "  [INFO] 磁盘剩余：$(df -h "$ROOT" | awk 'NR==2{print $4}')"
  note ""
  note "== .env（只看存在与权限，不读内容）=="
  if [ -f "$ROOT/.env" ]; then
    note "  [PASS] 存在，权限 $(stat -c '%a' "$ROOT/.env" 2>/dev/null || echo '未知')（建议 600）"
  else
    note "  [INFO] 没有 .env —— 面板注册管理员；要预置就照 docs/install.md 建一个"
  fi
  note ""
  note "== 出网（源码构建与插件安装都要 GitHub）=="
  if have git; then
    if git ls-remote --exit-code https://github.com/ProvidersPlugin/Agent2Providers HEAD >/dev/null 2>&1; then
      note "  [PASS] 宿主机可访问 GitHub（源码构建 / Bun 运行时下载可用）"
    else
      note "  [WARN] 宿主机访问 GitHub 失败：构建用本脚本 build（走宿主网络），"
      note "         或 docker build --build-arg HTTPS_PROXY=…；脚本插件宿主首次启用要下 Bun（github.com/oven-sh/bun），见 install.md 排错节"
    fi
  fi
  note ""
  if [ "$fail" -ne 0 ]; then
    note "结论：有 FAIL 项，先修再 up。"
    return 1
  fi
  note "结论：检查通过。"
}

cmd_config() {
  require docker docker
  note "服务名   : $(service_name)"
  note "镜像     : $(image_ref)"
  note "宿主端口 : $(published_port) → 容器 $(gateway_port)"
  note "数据目录 : $(data_dir)"
  note "容器用户 : $(container_user)"
  note "环境变量名（值不打印）："
  if have jq; then
    compose config --format json 2>/dev/null \
      | jq -r '.services[]?.environment // {} | keys[] | "  - " + .' || note "  （解析失败）"
  else
    note "  （装 jq 后可列出：sudo pacman -S jq）"
  fi
}

main() {
  local cmd="${1:-}"
  shift || true
  case "$cmd" in
    up)      cmd_up "${1:-}" ;;
    build)   cmd_build "${1:-}" ;;
    down)    cmd_down ;;
    restart) cmd_restart ;;
    status)  cmd_status ;;
    logs)    cmd_logs ;;
    health)  cmd_health ;;
    backup)  cmd_backup "${1:-}" ;;
    restore) cmd_restore "${1:-}" ;;
    doctor)  cmd_doctor ;;
    config)  cmd_config ;;
    ""|-h|--help|help) usage ;;
    *) usage >&2; exit 2 ;;
  esac
}

main "$@"
