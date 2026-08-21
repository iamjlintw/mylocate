#!/bin/bash
# mylocate 安裝腳本：編譯、安裝執行檔、建立索引，並選擇性註冊 launchd agent。
set -euo pipefail

cd "$(dirname "$0")"

BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
LABEL="com.mylocate.daemon"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"

echo "==> 編譯（release）"
cargo build --release

echo "==> 安裝執行檔到 $BIN_DIR"
mkdir -p "$BIN_DIR"
install -m 755 target/release/ml "$BIN_DIR/ml"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *)
    echo
    echo "    注意：$BIN_DIR 不在 PATH 中，請加進你的 shell 設定："
    echo "    echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.zshrc"
    echo
    ;;
esac

if [ ! -f "$HOME/Library/Caches/mylocate/index.bin" ]; then
  echo "==> 建立第一份索引（約需 15-20 秒）"
  "$BIN_DIR/ml" index
else
  echo "==> 索引已存在，略過建立（要重建請執行：ml index）"
fi

echo
read -r -p "要註冊 launchd agent，讓 daemon 開機自動啟動嗎？[y/N] " ans
if [[ "$ans" =~ ^[Yy]$ ]]; then
  mkdir -p "$(dirname "$PLIST")"
  cat > "$PLIST" <<PLISTEOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$LABEL</string>
    <key>ProgramArguments</key>
    <array>
        <string>$BIN_DIR/ml</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/tmp/mylocate.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/mylocate.log</string>
</dict>
</plist>
PLISTEOF

  # 先卸載舊的（可能不存在，失敗不影響）
  launchctl unload "$PLIST" 2>/dev/null || true
  launchctl load -w "$PLIST"
  echo "    已註冊並啟動。日誌：/tmp/mylocate.log"
  echo "    要停用：launchctl unload -w $PLIST"
else
  echo "    略過。需要時可手動啟動：ml daemon &"
fi

echo
echo "完成。試試看："
echo "    ml <關鍵字>       搜尋"
echo "    ml -i             互動模式（需要 fzf）"
echo "    ml stats          查看狀態"
