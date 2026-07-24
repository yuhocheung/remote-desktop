#!/usr/bin/env bash
# 重导出 docs/架构图/*.svg 为同名 .png（供 Markdown 引用）。
#
# 用法:  bash scripts/export_diagrams.sh [svg ...]
#        不带参数时处理 docs/架构图/ 下全部 SVG。
#
# 实现要点（本机实测，勿随意改）:
#   1. 无 rsvg/inkscape/cairosvg，用 Edge headless --screenshot 渲染。
#   2. 必须带 --no-sandbox，否则 Edge 写不出文件。
#   3. 显示器 125% 缩放下 DPR=1.25 且 --force-device-scale-factor=1 无效:
#      截图像素 = --window-size，但 CSS 视口只有 window/1.25。
#      因此按 1.25 倍放大窗口截图，再用 Pillow 降采样回 SVG 原始尺寸。
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIR="$ROOT/docs/架构图"

EDGE=""
for p in "/c/Program Files (x86)/Microsoft/Edge/Application/msedge.exe" \
         "/c/Program Files/Microsoft/Edge/Application/msedge.exe"; do
  [ -f "$p" ] && EDGE="$p" && break
done
if [ -z "$EDGE" ]; then echo "ERROR: 找不到 msedge.exe"; exit 1; fi

if [ "$#" -gt 0 ]; then SVGS=("$@"); else SVGS=("$DIR"/*.svg); fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
i=0
for svg in "${SVGS[@]}"; do
  i=$((i+1))
  base="$(basename "$svg" .svg)"
  # 读 SVG 根元素 width/height
  # Git Bash 下「进程替换 + heredoc」喂 read 不稳定，改用命令替换 + set -- 拆词
  WH="$(python -c 'import re,sys; s=open(sys.argv[1],encoding="utf-8").read(); m=re.search(r"<svg[^>]*\bwidth=.(\d+).[^>]*\bheight=.(\d+).",s); print(m.group(1),m.group(2)) if m else sys.exit("根元素缺 width/height: "+sys.argv[1])' "$svg")" || exit 1
  set -- $WH; W=$1; H=$2
  WW=$(( (W*5+3)/4 ))   # ceil(W*1.25)
  HH=$(( (H*5+3)/4 ))
  RAW="$TMP/${base}.png"
  # Git Bash 下 ${var} 在反斜杠 Windows 路径里会被转义吞掉，截图输出一律走 /tmp 风格正斜杠路径
  "$EDGE" --headless --no-sandbox --disable-gpu --disable-extensions \
      --user-data-dir="$TMP/prof_$i" --hide-scrollbars \
      --window-size="$WW,$HH" --screenshot="$(cygpath -m "$RAW")" \
      "$(cygpath -m "$svg")" >/dev/null 2>&1
  if [ ! -s "$RAW" ]; then echo "ERROR: $base 截图失败"; exit 1; fi
done

python - "$TMP" "${SVGS[@]}" <<'PY'
import sys
from pathlib import Path
from PIL import Image
tmp = Path(sys.argv[1])
for svg in sys.argv[2:]:
    svg = Path(svg)
    raw = Image.open(tmp / (svg.stem + ".png"))
    import re
    m = re.search(r'<svg[^>]*\bwidth="(\d+)"[^>]*\bheight="(\d+)"',
                  svg.read_text(encoding='utf-8'))
    size = (int(m.group(1)), int(m.group(2)))
    out = svg.with_suffix(".png")
    raw.resize(size, Image.LANCZOS).save(out, optimize=True)
    print(f"{out.name}: {raw.size} -> {size}")
PY
echo "done."
