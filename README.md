# ai-usage-tui

終端機裡看 AI 用量剩多少：打一個 HTTP GET 網址，把回傳的用量 JSON 畫成進度條。

## 用法

```sh
# TUI（預設每 60 秒重抓一次；q / Esc / Ctrl-C 離開）
ai-usage-tui --url "https://<host>/usage/frontend?token=<token>"

# 或用環境變數
export AI_USAGE_TUI_URL="https://<host>/usage/frontend?token=<token>"
ai-usage-tui --interval 30

# 只抓一次、印純文字進度條到 stdout 後離開
ai-usage-tui --once
```

| 參數 | 說明 |
|---|---|
| `--url <URL>` | 完整網址（token 放在 query string）；未給時讀 `AI_USAGE_TUI_URL` |
| `--interval <秒>` | 重抓間隔，預設 60 |
| `--once` | 抓一次、印文字版、離開（不進 TUI） |

兩者都沒給 URL 時印錯誤並以 exit code 2 離開；抓取失敗（連線錯誤、逾時 10 秒、HTTP 非 2xx）
在 `--once` 模式印錯誤並以 exit code 1 離開，在 TUI 模式保留上一次成功的資料並在底部顯示錯誤。

## 期望的回應格式

```json
{
  "claude": {
    "five_hours": { "used": 23, "resets_at": 1791196799681, "formatted_message": "*77%* remaining, resets 10-05 18:39" },
    "seven_days": { "used": 15, "resets_at": 1791241199681, "formatted_message": "*85%* remaining, resets 10-06 06:59" }
  }
}
```

- 最外層 key 是 provider，第二層 key 是 window，兩層都可自由增減。
- `used`：已用百分比（0–100）。`resets_at`：毫秒 epoch。
- 顯示順序：provider 依字母序；window 先 `five_hours`、`seven_days`，其餘依字母序。
- `resets_at` 會轉成本地時區顯示，並附倒數（例：`resets 10-05 18:39 (in 2h 13m)`）。

## 開發

```sh
cargo build
cargo clippy -- -D warnings
cargo test
```
