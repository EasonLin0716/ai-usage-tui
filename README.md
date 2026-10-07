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
    "five_hours": { "used": 1, "resets_at": 1791283199962, "formatted_message": "*99%* remaining, resets 10-06 18:39" },
    "seven_days": { "used": 4, "resets_at": 1791845999962, "formatted_message": "*96%* remaining, resets 10-13 06:59" },
    "updated_at": 1791268244117
  }
}
```

- 最外層 key 是 provider，第二層 key 是 window，兩層都可自由增減。
- 第二層裡值是物件的 key 才算 window（必須含 `used`、`resets_at`、`formatted_message`，否則整筆回應視為無效）；
  值不是物件的 key（數字、字串、null、陣列）視為 provider 的 metadata：`updated_at` 會被讀出來顯示，其他一律忽略。
- `used`：已用百分比（0–100）。`resets_at`、`updated_at`：毫秒 epoch；`updated_at` 可省略。
- 顯示順序：provider 依字母序；window 先 `five_hours`、`seven_days`，其餘依字母序。
- `resets_at` 會轉成本地時區顯示，並附倒數（例：`resets 10-05 18:39 (in 2h 13m)`）；
  後端在該 window 沒有待重置時間時會送 `null`，此時顯示 `no reset`。
- `updated_at`（後端上次更新數字的時間）轉成本地時區顯示在 provider 標題（TUI）或 provider 行（`--once`）；
  沒有時 TUI 標題改顯示本機抓取時間。TUI 底部固定顯示最後一次成功抓取的本機時間（抓取失敗時也會保留）。

## 開發

```sh
cargo build
cargo clippy -- -D warnings
cargo test
```
