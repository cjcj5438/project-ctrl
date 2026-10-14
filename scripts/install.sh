#!/usr/bin/env bash
#
# 从 GitHub Release 安装 / 升级 project-ctrl 到 /Applications。
#
# 用法：
#   scripts/install.sh                 # 安装最新 Release
#   scripts/install.sh --tag v0.1.0    # 安装指定 tag
#   scripts/install.sh --dry-run       # 只打印将执行的步骤
#   scripts/install.sh --open          # 安装完启动
#
set -euo pipefail

REPO="cjcj5438/project-ctrl"
APP_NAME="project-ctrl.app"
INSTALL_DIR="/Applications"
TARGET="$INSTALL_DIR/$APP_NAME"
BINARY="$TARGET/Contents/MacOS/project-ctrl"

TAG=""
DRY_RUN=0
OPEN_AFTER=0

usage() {
  cat <<'EOF'
用法: install.sh [--tag vX.Y.Z] [--dry-run] [--open]

  --tag <tag>   安装指定 Release tag（默认：最新 Release）
  --dry-run     只打印将执行的步骤，不做任何改动
  --open        安装完成后启动应用
  -h, --help    显示帮助
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --tag)
      [ "$#" -ge 2 ] || { echo "错误：--tag 需要一个参数" >&2; exit 2; }
      TAG="$2"
      shift 2
      ;;
    --tag=*)
      TAG="${1#*=}"
      shift
      ;;
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --open)
      OPEN_AFTER=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "错误：未知参数 $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

log() { printf '%s\n' "$*"; }

# dry-run 时只打印，不执行
run() {
  if [ "$DRY_RUN" -eq 1 ]; then
    log "[dry-run] $*"
  else
    log "+ $*"
    "$@"
  fi
}

TMPDIR_INSTALL="$(mktemp -d "${TMPDIR:-/tmp}/project-ctrl-install.XXXXXX")"
cleanup() { rm -rf "$TMPDIR_INSTALL"; }
trap cleanup EXIT

ARCHIVE="$TMPDIR_INSTALL/$APP_NAME.tar.gz"
STAGED_APP="$TMPDIR_INSTALL/$APP_NAME"

log "仓库:      $REPO"
log "目标:      $TARGET"
log "临时目录:  $TMPDIR_INSTALL"
if [ -n "$TAG" ]; then
  log "Release:   $TAG"
else
  log "Release:   latest"
fi
log ""

# 1) 下载 .app.tar.gz
if [ -n "$TAG" ]; then
  run gh release download "$TAG" --repo "$REPO" --pattern '*.app.tar.gz' --dir "$TMPDIR_INSTALL"
else
  run gh release download --repo "$REPO" --pattern '*.app.tar.gz' --dir "$TMPDIR_INSTALL"
fi

if [ "$DRY_RUN" -eq 0 ]; then
  # 归一化文件名（避免 tag 前缀等造成的命名差异）
  found="$(find "$TMPDIR_INSTALL" -maxdepth 1 -name '*.app.tar.gz' -print -quit)"
  if [ -z "$found" ]; then
    echo "错误：Release 中没有找到 *.app.tar.gz" >&2
    exit 1
  fi
  [ "$found" = "$ARCHIVE" ] || mv "$found" "$ARCHIVE"
else
  run mv "<下载得到的 *.app.tar.gz>" "$ARCHIVE"
fi

# 2) 解压到临时目录
run tar -xzf "$ARCHIVE" -C "$TMPDIR_INSTALL"

if [ "$DRY_RUN" -eq 0 ]; then
  if [ ! -d "$STAGED_APP" ]; then
    echo "错误：压缩包中没有 $APP_NAME" >&2
    exit 1
  fi
fi

# 3) 应用正在运行则优雅退出（最多等约 10 秒，不强杀）
if pgrep -f "$BINARY" >/dev/null 2>&1; then
  log "应用正在运行，请求退出…"
  if [ "$DRY_RUN" -eq 1 ]; then
    log "[dry-run] osascript -e 'tell application \"project-ctrl\" to quit'"
    log "[dry-run] 等待最多 10 秒至进程退出"
  else
    osascript -e 'tell application "project-ctrl" to quit' || true
    for _ in $(seq 1 10); do
      pgrep -f "$BINARY" >/dev/null 2>&1 || break
      sleep 1
    done
    if pgrep -f "$BINARY" >/dev/null 2>&1; then
      echo "错误：project-ctrl 仍在运行，请手动退出后重试（不强制结束进程）" >&2
      exit 1
    fi
  fi
fi

# 4) 安装（ditto 覆盖）
run mkdir -p "$INSTALL_DIR"
run ditto "$STAGED_APP" "$TARGET"

# 5) 清除 quarantine（没有该属性也不报错）
if [ "$DRY_RUN" -eq 1 ]; then
  log "[dry-run] xattr -dr com.apple.quarantine $TARGET"
else
  xattr -dr com.apple.quarantine "$TARGET" 2>/dev/null || true
fi

# 6) 打印版本
if [ "$DRY_RUN" -eq 1 ]; then
  log "[dry-run] plutil -extract CFBundleShortVersionString raw -o - $TARGET/Contents/Info.plist"
  log ""
  log "（dry-run 结束，未做任何改动）"
  exit 0
fi

VERSION="$(plutil -extract CFBundleShortVersionString raw -o - "$TARGET/Contents/Info.plist")"
log ""
log "已安装 project-ctrl $VERSION 到 $TARGET"

# 7) 可选启动
if [ "$OPEN_AFTER" -eq 1 ]; then
  run open "$TARGET"
fi
