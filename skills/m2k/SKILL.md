---
name: m2k
description: Operate the m2k CLI for Mail2000 email, meeting room availability, synchronization, and its terminal UI when the user asks to use m2k.
---

# m2k

先讀目前安裝版本的 `m2k --help`，再讀與任務有關的子命令 `--help`，確認參數與功能後才執行。CLI help 是介面的來源；不要把 README 的命令清單複製成操作規格。若只有原始碼，於專案目錄用 `cargo run -- --help`，後續命令同樣透過 `cargo run --` 執行。

沿用使用者指定的 `--data-dir` 與既有帳號。代理程式讀結果時加 `--json`；密碼由既有登入設定取得。會議室查詢的 `unknown` 與查詢錯誤必須保留，包含 403 的未知結果不能算空閒。精確時段查詢只推薦 `free`；加 `--minutes` 找空檔時，推薦已確認的 `available_slots`，即使整個搜尋窗的狀態是 `busy`。寄送、邀請、預約或刪除依使用者已給的授權執行。

常見任務的入口與範例；以剛讀到的 help 校正參數，替換日期、收件人與檔案路徑：

- 查會議室：先讀 `m2k date-range --help`、`m2k meeting --help` 與 `m2k meeting rooms --help`。例如「找下週五下午兩點到三點、北區至少八人的會議室」：先執行 `m2k --json date-range '下週五'`，從回傳日期與時區推導該日 14:00–15:00 的精確 RFC3339 時間 `$START`、`$END`，再執行 `m2k --json meeting rooms --site 北區 --capacity 8 --start "$START" --end "$END"`。若需求是「下週五任一小時空檔」，以回傳日期範圍為搜尋窗加 `--minutes 60`，列出每間會議室的 `available_slots`。
- 讀信：`m2k --json mail list`／`mail search` 預設只回傳新回覆正文、`raw` 為空、附件只有中繼資料；需要單封全文用 `mail show ID --full`，附件用 `mail attachment`。除非使用者要求原始郵件，不要對 list／search 加 `--full`，它會帶回完整 RFC822 與 base64 附件。
- 寄信：先讀 `m2k mail --help` 與 `m2k mail send --help`。把已確認的正文寫入 UTF-8 檔，例如 `m2k mail send --to colleague@example.com --subject '會議安排' --body-file /tmp/m2k-body.txt`。
- 同步：先讀 `m2k sync --help`、`m2k watch --help` 與 `m2k directory sync --help`。預設只同步郵件與行事曆；郵件採 UID／旗標增量，例如 `m2k --json sync --kind mail`。行事曆以 ETag 重用正文，尚無伺服器 sync-token；`watch` 每個設定週期更新現在前後 30 天，首次與距上次成功完整同步至少 24 小時時更新完整 retention 範圍。聯絡人只在使用者要求時，用 `directory sync --scope company` 手動全量更新，或選 `--scope department`／指定分支。TUI 聯絡人頁只讀本機，`Shift-R` 才同步全公司；保留成功與錯誤結果。
- 使用 TUI：先讀 `m2k tui --help`，在互動終端執行 `m2k tui`，於畫面按 `?` 查看操作。無 TTY 時用 `m2k tui --snapshot` 檢視畫面。
- 新信 webhook：使用者要求把新郵件送到指定端點時，先讀 `m2k mail watch --help`，沿用帳號與資料目錄，執行 `mail watch --webhook-url URL`。首次只建立基準，相同網址重啟才接續；接收端依 `event_id` 去重，正文會傳至端點，Bearer token 由 `M2K_WEBHOOK_TOKEN` 提供。每輪 JSON 的 `delivery.error` 與 `status.mail_webhooks` 顯示送達／待送狀態，`--once` 結束仍可能待送；一般 `watch` 負責歷史與旗標同步及保存清理。fixture 成功與正式站台監看／實際接收端送達分開回報。

先讀 `m2k mail move --help`；批次預覽例如 `m2k --json mail move --folder Archive ID_1 ID_2 --dry-run`，確認計畫後依授權移除 `--dry-run` 執行。

`mail move` 支援同一來源資料夾、同一 UIDVALIDITY 的多個 ID。單封成功維持原有 JSON；批次成功回傳 `results` 的逐 ID 結果。`--dry-run` 連線後只用 EXAMINE、BODY.PEEK 與 SEARCH 檢查，回傳 `path`（`move`／`uidplus`／`guarded-copy-fallback`）、來源、目標、UID、Message-ID 及 `refusal`（無拒絕原因時為 null），不修改郵件或本機快取。所有能力路徑的 dry-run 都採相同保守檢查。

沒有 MOVE 與 UIDPLUS 時，移動會先確認每封信有唯一 Message-ID，且來源沒有集合以外的已刪除郵件，再一次複製整批、唯讀驗證目標、重新確認來源 UIDVALIDITY、標記刪除並重新比對完整 DELETED 集合，最後才執行一次 EXPUNGE。比對不符會撤回本次新增的刪除標記，保留原有標記；驗證失敗不標記或 EXPUNGE。失敗時目標可能保留副本，請先確認再重試。此流程不設定已讀狀態，成功才移除來源本機快取；不支援 MOVE／UIDPLUS 的伺服器仍無法提供跨用戶端原子性，其他用戶端若在最後檢查與 EXPUNGE 之間新增刪除標記，仍可能受影響。
