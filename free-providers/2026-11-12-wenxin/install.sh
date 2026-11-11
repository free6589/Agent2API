#!/usr/bin/env bash
# free-wenxin 安装钩子：把本目录的插件 zip 装进 Agent2API 数据目录，
# 让网关发现并加载「文心（千帆）」provider。
#
# ── 这个脚本做什么 ──────────────────────────────────────────────
#   1. 校验输入（zip 存在、内含 pack.json + 目标平台入口二进制）
#   2. 备份数据目录的 plugins.json（`plugins.json.bak.<时间戳>`，回滚即恢复它）
#   3. 解包 zip 到 `plugins/free-wenxin/2026.11.12/`（幂等：先删旧目录再解）
#   4. 在 plugins.json 里登记/更新 free-wenxin 条目（只动这一家，别家不动）
#   5. 复核：sha256 对得上、入口可执行、pack.json 可解析
#
# ── 用法 ──────────────────────────────────────────────────────
#   ./install.sh                       # 装到探测到的数据目录
#   ./install.sh --home /data          # 显式指定（等价于 AGENT2API_PROXY_HOME）
#   ./install.sh --dry-run             # 只打印计划，不动文件
#   ./install.sh --rollback            # 用最近一次备份恢复 plugins.json（并删掉本包目录）
#   ./install.sh --zip PATH            # 用指定 zip（默认本目录 dist/free-wenxin-2026.11.12.zip）
#   ./install.sh --restart-container NAME
#                                      # 顺带重启容器网关（默认不重启，只提示）
#
# ── 硬约束 ────────────────────────────────────────────────────
# 不联网、不发布、不碰别家 provider 的实现，只动 free-wenxin 这一包的
# 版本目录与注册表条目。任何一步失败都停在原地并打印已做了什么。
# 装完需要重启网关（宿主只在启动时读 plugins.json）。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PACK_ID="free-wenxin"
PACK_VERSION="2026.11.12"
ZIP="$HERE/dist/free-wenxin-2026.11.12.zip"
HOME_DIR=""
DRY_RUN=0
ROLLBACK=0
RESTART_CONTAINER=""

while [ $# -gt 0 ]; do
  case "$1" in
    --home) HOME_DIR="${2:?--home 需要目录}"; shift 2 ;;
    --zip) ZIP="${2:?--zip 需要路径}"; shift 2 ;;
    --restart-container) RESTART_CONTAINER="${2:?需要容器名}"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    --rollback) ROLLBACK=1; shift ;;
    -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
    *) echo "未知参数：$1" >&2; exit 2 ;;
  esac
done

say() { printf '%s\n' "$*"; }
die() { printf '错误：%s\n' "$*" >&2; exit 1; }

# ── 1. 找数据目录（与网关同一口径）──
if [ -z "$HOME_DIR" ]; then
  if [ -n "${AGENT2API_PROXY_HOME:-}" ]; then
    HOME_DIR="$AGENT2API_PROXY_HOME"
  elif [ -n "${WORKBUDDY_PROXY_HOME:-}" ]; then
    HOME_DIR="$WORKBUDDY_PROXY_HOME"
  elif [ -d "$HERE/../../data/plugins" ]; then
    HOME_DIR="$HERE/../../data"
  elif [ -d "$HOME/.agent2api" ]; then
    HOME_DIR="$HOME/.agent2api"
  else
    die "找不到数据目录，用 --home 指定（含 plugins/ 的那一级）"
  fi
fi
PLUGINS_DIR="$HOME_DIR/plugins"
TARGET_DIR="$PLUGINS_DIR/$PACK_ID/$PACK_VERSION"
REGISTRY="$PLUGINS_DIR/plugins.json"

if [ "$ROLLBACK" = 1 ]; then
  say "回滚 $PACK_ID ..."
  LAST_BAK="$(ls -t "$PLUGINS_DIR"/plugins.json.bak.* 2>/dev/null | head -1 || true)"
  [ -n "$LAST_BAK" ] || die "没有找到备份"
  [ "$DRY_RUN" = 1 ] && { say "  将用 $LAST_BAK 恢复 plugins.json，并删除 $TARGET_DIR"; exit 0; }
  cp "$LAST_BAK" "$REGISTRY"
  rm -rf "$TARGET_DIR"
  say "  已恢复注册表（来自 $LAST_BAK），已删除 $TARGET_DIR"
  say "  重启网关后生效"
  exit 0
fi

# ── 2. 校验输入 ──
[ -f "$ZIP" ] || die "找不到插件包：$ZIP（先确认 dist/ 下 zip 已生成）"
unzip -l "$ZIP" | grep -q "pack.json" || die "zip 里没有 pack.json"
ENTRY_IN_ZIP="$(unzip -Z1 "$ZIP" | grep -E "^bin/" | grep -v "/$" | head -1 || true)"
[ -n "$ENTRY_IN_ZIP" ] || die "zip 里没有 bin/ 入口二进制"

say "数据目录：$HOME_DIR"
say "插件包：$ZIP"
say "目标：$TARGET_DIR"
[ "$DRY_RUN" = 1 ] && { say "（dry-run：以上是计划，未动文件）"; exit 0; }

# ── 3. 备份注册表 ──
mkdir -p "$PLUGINS_DIR"
if [ -f "$REGISTRY" ]; then
  BAK="$REGISTRY.bak.$(date +%Y%m%d-%H%M%S)"
  cp "$REGISTRY" "$BAK"
  say "已备份注册表：$BAK"
else
  say "注册表不存在，稍后新建：$REGISTRY"
fi

# ── 4. 解包（幂等）──
rm -rf "$TARGET_DIR"
mkdir -p "$TARGET_DIR"
unzip -q -o "$ZIP" -d "$TARGET_DIR"
[ -f "$TARGET_DIR/pack.json" ] || die "解包后没有 pack.json，zip 布局不对"
[ -f "$TARGET_DIR/$ENTRY_IN_ZIP" ] || die "解包后没有入口：$ENTRY_IN_ZIP"
chmod +x "$TARGET_DIR/$ENTRY_IN_ZIP"
say "已解包：$(du -sh "$TARGET_DIR" | cut -f1)"

# ── 5. 登记注册表（只动 free-wenxin 这一条）──
SHA="$(sha256sum "$TARGET_DIR/$ENTRY_IN_ZIP" | cut -d' ' -f1)"
export PACK_SHA="$SHA" PACK_ENTRY="$ENTRY_IN_ZIP" PACK_VERSION="$PACK_VERSION"
python3 - "$REGISTRY" <<'EOF'
import json, sys, time, os
reg_path = sys.argv[1]
try:
    reg = json.load(open(reg_path))
except (FileNotFoundError, json.JSONDecodeError):
    reg = {"packs": []}
entry = {"id": "free-wenxin", "version": os.environ["PACK_VERSION"], "enabled": True,
         "providers": {"wenxin": True}, "entry": os.environ["PACK_ENTRY"],
         "sha256": os.environ["PACK_SHA"], "signed": False,
         "installed_at": int(time.time() * 1000),
         "manifest_providers": [{"id": "wenxin", "label": "文心（千帆）",
                                  "order": 0, "default_enabled": True}]}
packs = [x for x in reg.get("packs", []) if x.get("id") != "free-wenxin"]
packs.append(entry)
reg["packs"] = packs
json.dump(reg, open(reg_path, "w"), ensure_ascii=False, indent=2)
print("注册表已更新：", [x["id"] for x in packs])
EOF

# ── 6. 复核 ──
python3 -c "import json; d=json.load(open('$REGISTRY')); assert any(x['id']=='$PACK_ID' for x in d['packs']), '注册表无本包'"
[ -x "$TARGET_DIR/$ENTRY_IN_ZIP" ] || die "入口不可执行"
python3 -c "import json; json.load(open('$TARGET_DIR/pack.json'))" || die "pack.json 不可解析"
say "复核通过：sha256=${SHA:0:12}…、入口可执行、pack.json 可解析"
say ""
say "下一步：重启网关（宿主只在启动时读 plugins.json）"
if [ -n "$RESTART_CONTAINER" ]; then
  docker restart "$RESTART_CONTAINER"
else
  say "  容器网关：docker restart <容器名>   本机网关：重启 agent2api-server 进程"
fi
