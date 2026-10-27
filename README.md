# m2k

Mail2000 的 Rust 原生 CLI 與 TUI。參考既有 Mail2000 連接器的工作流程與 tokscale 的終端版面，使用同一組帳號密碼控制 IMAP、SMTP、CalDAV、CardDAV。

此階段是可執行的產品骨架：SQLite 持久化、離線查詢、同步與保存週期，以及郵件／人員／會議核心指令。真實 Mail2000 帳號的登入、權限與外部寫入仍需實際驗收；本機協定測試不能替代這項驗收。

## 安裝

從 [Releases](https://github.com/dnplus/m2k-cli/releases) 下載對應平台的壓縮檔（Linux x86_64、macOS Apple Silicon／Intel、Windows x86_64），核對同名 `.sha256` 後解壓，把 `m2k`（Windows 為 `m2k.exe`）放到 PATH 裡：

```sh
shasum -a 256 -c m2k-v0.3.0-aarch64-apple-darwin.tar.gz.sha256   # Linux 可用 sha256sum -c
tar -xzf m2k-v0.3.0-aarch64-apple-darwin.tar.gz
```

Windows 用 PowerShell 的 `Get-FileHash <檔名> -Algorithm SHA256` 比對。Linux 版在 Ubuntu 22.04 建置，執行時需要 `libdbus-1-3` 與 `libssl3`（大多數桌面發行版已內建）。macOS 版沒有經過 Apple 公證，用瀏覽器下載時若被 Gatekeeper 擋下，對解壓後的執行檔執行 `xattr -d com.apple.quarantine m2k-*/m2k` 後再開；用 `curl` 下載則不會被標記。推送 `vX.Y.Z` tag 時由 `.github/workflows/release.yml` 自動建置並發佈，tag 必須和 `Cargo.toml` 的版本一致。

## 建置與啟動

```sh
cargo build --release
./target/release/m2k --help
cargo install --path . --locked
```

密碼存在系統密碼庫：macOS 透過 Apple 簽章的 `/usr/bin/security` 讀寫登入鑰匙圈，Windows 用認證管理員，Linux 用 Secret Service。macOS 項目的存取權限信任的是 `security` 而不是 m2k 執行檔，所以重新編譯、`cargo install` 或下載的 release binary 都不會再跳授權視窗。安全性取捨和 `gh` 相同：同一個使用者的任何程式只要執行 `security find-generic-password -s m2k -g` 就能讀到密碼；需要更嚴格的隔離時，請改用 `M2K_PASSWORD` 由外部密碼管理工具注入。寫入時密碼經由標準輸入交給 `security`，不會出現在程序參數列。

從舊版升級：舊版建立的鑰匙圈項目只信任當初的 m2k 執行檔，升級後第一次讀取時 macOS 會跳出授權視窗，可能要求輸入登入鑰匙圈密碼，請選「永遠允許」。讀到密碼後 m2k 會刪除舊項目、由 `security` 重新建立（`add-generic-password -U` 不會更換既有項目的存取權限，所以必須重建），之後就不會再問；完成的帳號記錄在資料目錄的 `keychain-acl-migrated`。重新寫入失敗會再試一次；仍失敗時 stderr 會說明舊項目已刪除，請重新執行 `m2k auth login`。`M2K_PASSWORD` 提供的密碼不會寫進鑰匙圈，`m2k doctor` 是唯讀檢查，兩者都不做這個遷移。每次呼叫 `security`（讀取、刪除、重新寫入）最多等待 15 秒，逾時會結束該 `security` 程序，不留下背景程序。若先前為 m2k 建立過「m2k local codesign」自簽憑證，現在可以自行在「鑰匙圈存取」刪除，m2k 不會替你刪。

`M2K_KEYCHAIN` 可指定一個鑰匙圈檔路徑，macOS 上所有 `security` 指令都改用該檔而不碰登入鑰匙圈；測試以不存在的路徑讓 m2k 視為沒有存放密碼。其他平台忽略這個變數。

啟動獨立展示資料，資料都是虛構的，線上操作會拒絕執行：

```sh
m2k --data-dir .tmp/demo demo
m2k --data-dir .tmp/demo demo --no-tui
m2k --data-dir .tmp/demo tui --snapshot
```

正式帳號的預設資料目錄由作業系統決定：macOS 是 `~/Library/Application Support/m2k`。可全域使用 `--data-dir` 切換獨立帳號資料目錄；每個資料庫會綁定帳號及 DAV／IMAP 端點。

## 單一帳號設定

```sh
m2k init --username your_account --server mail.example.com
m2k auth login
m2k auth check
m2k doctor
m2k sync
m2k tui
```

`auth login` 隱藏輸入密碼，檢查四種服務後存入系統密碼庫。`auth logout` 移除密碼，保留離線資料。自動化環境可提供 `M2K_PASSWORD`，程式不會把它寫進設定或 SQLite。設定與資料檔權限為 `0600`，資料目錄為 `0700`；這是本機檔案存取保護，SQLite 內容沒有另行加密。

`m2k doctor`（可加 `--json`）是唯讀的驗收前檢查，一次回答「這台機器、這個帳號能不能開始正式驗收」。逐項回報 `pass`／`warn`／`fail`／`skip` 與原因（`warn` 不算失敗）：`build`（版本與 build commit）、`data_dir`（0700）、`config`（存在、有效、0600）、`site_profile`（實際生效的 profile；通用 profile 且沒有任何使用者 profile 時為 `warn` 並附遷移步驟）、`credentials`（系統密碼庫或 `M2K_PASSWORD`）、`imap`、`smtp`、`caldav`、`carddav`（登入並探索 principal／collection）、`database`（可開啟且 schema 為最新）、`sync_lock`（沒有被其他程序占用）。展示資料目錄會把所有線上與憑證檢查標為 `skip` 並註明原因。任何一項 `fail` 即以狀態 1 結束；`--json` 不論成功或失敗，都從同一個頂層路徑讀結果：`checks`、`version`、`build_commit`、`data_dir`、`demo`。失敗時另外沿用錯誤契約（`state:"error"`、`error.code:"doctor_failed"`）；舊的 `report`（內容與頂層相同）保留給既有腳本，新腳本請讀頂層。doctor 不寄信、不改旗標、不寫行事曆、不建立或遷移任何本機檔案，錯誤訊息也不會出現密碼；SMTP 只做連線與認證測試。

若網路擋住 SMTP 587，可執行 `m2k config set --smtp-port 465`，改用 implicit TLS。

預設 IMAP 為 TLS 993、SMTP 為 STARTTLS 587。帳號前綴會依伺服器網域推導 email（`mail.example.com` → `your_account@example.com`）。CalDAV 使用個人行事曆；CardDAV 由 `mPA.000@<帳號網域>` principal 探索 addressbook home。實際同步網址可在初始化時覆寫：

```sh
m2k init --username your_account --email your_account@example.com \
  --caldav-url 'https://mail.example.com/cgi-bin/cal/caldav/calendars/your_account%40example.com/default/' \
  --carddav-url 'https://mail.example.com/cgi-bin/carddav/principals/mPA.000@example.com'
```

### Site profile

站台差異（CalDAV／CardDAV 路徑、公司通訊錄根、會議室 provider、據點清單、房名解析規則、內建 fallback 會議室）放在 site profile，不寫在程式裡。`init` 與 `auth login` 可用 `--site-profile` 指定 profile 名稱或 JSON 檔路徑，存進 `config.json` 的 `site_profile`；未指定時，若伺服器符合某個 profile 的 `server` 就自動套用，否則使用只含 Mail2000 通用路徑的 `mail2000` profile，選到的名稱同樣寫進 `site_profile`。舊版建立、沒有 `site_profile` 的設定在每次執行時依伺服器自動選用。

程式只內建 `mail2000`（通用）與 `demo`（展示用、虛構）兩個 profile，在 `src/site/*.json`。站台專屬的 profile（含真實會議室 access 名稱）不放進程式，由使用者放在資料目錄的 `site-profiles/<name>.json`（例如 macOS 為 `~/Library/Application Support/m2k/site-profiles/`，`--data-dir` 指定時為該目錄下的 `site-profiles/`），之後以 `--site-profile <name>` 或伺服器自動選用；也可直接給任意路徑（含 `/` 或副檔名才視為路徑，單純名稱只找 `site-profiles/`）。環境變數 `M2K_SITE_PROFILE_DIR` 可改用其他目錄。使用者 profile 與內建 profile 同名時使用內建版本並顯示警告。範本見 [`examples/site-profile.example.json`](examples/site-profile.example.json)，內容全部虛構。

```json
{
  "name": "example",
  "server": "mail.example.com",
  "caldav_calendars_path": "/cgi-bin/cal/caldav/calendars",
  "carddav_principal_path": "/cgi-bin/carddav/principals/mPA.000@{domain}",
  "company_directory_root": "EXAMPLE_ALL",
  "room_provider": "@{domain}",
  "sites": [{ "name": "北區", "prefixes": ["北區"] }],
  "default_site": "北區",
  "room_capacity_from_name_suffix": true,
  "rooms": [{ "name": "北區甲室_10", "access": "ROOM_ACCESS_NAME" }]
}
```

除了 `name` 以外都可省略；`{domain}` 代入帳號 email 的網域。profile 有 `server` 時，`init --site-profile NAME|FILE` 可省略 `--server`。`site-profiles/` 中格式錯誤的檔案在依伺服器自動選用時會被略過，以名稱指定時才報錯。

#### 從內建站台 profile 遷移

舊版曾把某個站台的 profile 編進程式。升級到不內建該 profile 的版本前，先把舊檔複製到資料目錄，`config.json` 不需修改：

```sh
DATA_DIR="$HOME/Library/Application Support/m2k"   # Linux：~/.local/share/m2k；或你的 --data-dir
mkdir -p "$DATA_DIR/site-profiles"
git show <舊版 commit>:src/site/<name>.json > "$DATA_DIR/site-profiles/<name>.json"
m2k --json meeting resources | head   # 確認會議室清單與升級前相同
```

`config.json` 的 `site_profile` 若是該名稱，會改從這個檔案載入；若沒有 `site_profile`、原本靠伺服器自動選用，也會依檔案中的 `server` 選到同一份 profile。檔案不存在時，以名稱指定的設定會明確報錯並指出預期路徑；靠伺服器自動選用的設定則退回通用 `mail2000` profile（沒有會議室清單）；這時 `m2k doctor` 的 `site_profile` 項目回報 `warn`，`meeting resources`／`meeting rooms` 找不到任何會議室時在 stderr 與 JSON `warnings` 附上本段步驟，`directory self` 無法判定根通訊錄時也會提示。

## 同步與循環保存

```sh
m2k config show
m2k config set --sync-interval-minutes 30 \
  --mail-retention-days 90 --calendar-retention-days 365 --contacts-retention-days 0
m2k sync --kind mail
m2k watch
m2k status
m2k prune
```

`watch` 是前景常駐程序，依設定週期同步並清理，Ctrl-C 停止。預設同步間隔 360 分鐘，郵件保存 180 天、過去事件保存 365 天，聯絡人永久保存；天數 `0` 表示永久。郵件同步窗口跟隨郵件保存天數。未來與重複事件不因基礎 retention 被刪掉；聯絡人依最後成功看見的時間清理。清理只刪本機資料，不刪伺服器郵件、事件或聯絡人。

預設 `sync`／`watch` 只處理郵件與行事曆。郵件以 UID／旗標增量更新，重用已有的原始郵件，下載新增或缺少正文的郵件。行事曆以 ETag 比對資源，只取得新增或變更的正文。`watch` 首次與距離上次成功完整同步至少 24 小時時抓取完整 retention 範圍，其餘每個設定週期只更新現在前後 30 天，區間外快取保留；`sync --kind calendar` 仍手動更新完整 retention 範圍。

完整同步時會對每個行事曆 collection 多送一個 Depth 0 PROPFIND，探測 RFC 6578 `DAV:sync-token` 與 `supported-report-set`；所有 collection 都宣告支援 `sync-collection` 才保存 token（存在既有 `metadata` 表，不改 schema）。有 token 且一小時內做過近區間 ETag 比對時，`watch` 的週期同步改用 `REPORT sync-collection` 只取變更與刪除（404），涵蓋整個 collection 而不限前後 30 天；否則仍跑近區間 ETag 比對，所以伺服器即使悄悄回空的變更，近區間最慢約一小時加一個週期內會更新。token 被拒（`valid-sync-token`）就清掉並在同一輪改做一次完整同步；sync-collection 其他異常回應（非 207、缺 token、不標準的 multistatus 等）清掉 token 並在同一輪改跑近區間，認證失敗與連線錯誤照常回報。伺服器不支援、或有 collection 沒有 token 時，沿用上面的 ETag 近區間策略；每 24 小時的完整同步仍保留作為安全網。真實 Mail2000 是否支援尚未驗證：`m2k doctor` 的 caldav 檢查會顯示 `sync-token：N 個 collection 都宣告支援 sync-collection` 或宣告支援的比例。

聯絡人僅手動更新：`directory sync --scope company`（或 `sync --kind contacts`）做全公司全量同步，也可用 `--scope department`／`--addressbook` 更新部門或分支；`watch --kind contacts` 會拒絕啟動。同步採各資料域獨立提交：失敗的資料域保留原快取，CLI 回報錯誤並以非零狀態結束。SQLite 完整快照、指定集合快照與部分更新有各自的交易邊界；局部分支更新保留其他分支。

`status --json` 包含 `version`、`build_commit`、最近同步結果與 `progress`。同步進度記錄準備／探索／取得／保存階段、目前資料種類與已保存筆數；取得資料時不提供猜測的百分比。程序中斷後鎖已釋放，未完成紀錄會顯示 `interrupted`，不會繼續宣稱同步中。

鄰近日曆同步中，原本在區間內、此次未回傳的已快取單次事件，會依原 href 再 GET：移到遠處的事件更新到新日期，只有 404／410 才移除；權限或解析錯誤保留原快取。既有資料庫自動升級至 schema 3，保留 schema 2 的聯絡人聚合與 memberships，加入郵件監看進度與 webhook 待送事件；TUI 翻頁只解碼當頁資料。

`--json` 的執行及參數錯誤會以非零退出碼與單一 JSON 回傳，例如：

```json
{"state":"error","error":{"code":"not_found","message":"找不到項目：missing-id","retryable":false}}
```

`account_busy` 表示操作尚未開始，可稍後再試。其他錯誤不建議自動重試。部分同步失敗回傳 `state: partial`、`error.code: partial_sync` 與各資料域 `results`；已有 DAV 遠端寫入 receipt、訂房 `partial`／`unknown` 契約保留。`watch --json` 每個週期輸出一行 JSON，成功為 `state: complete`，部分失敗沿用 `partial_sync` 契約；每行皆附 `time`。`--help`／`--version` 及明確要求的 raw／snapshot 仍是文字輸出。

## 郵件

```sh
m2k mail folders
m2k mail list --collection INBOX
m2k mail search '專案'
m2k --json mail list --query '會議'
m2k mail show MESSAGE_ID
m2k mail show MESSAGE_ID --full
m2k mail show MESSAGE_ID --raw
m2k mail thread --id MESSAGE_ID
m2k mail thread --id MESSAGE_ID --full
m2k mail reply MESSAGE_ID --body-file reply.txt
m2k mail forward MESSAGE_ID --to colleague@example.com --body-file note.txt
m2k mail send --to colleague@example.com --subject '進度' --body-file body.txt --attach report.pdf
m2k mail flag MESSAGE_ID --flag seen
m2k mail flag MESSAGE_ID --flag flagged --clear
m2k mail move MESSAGE_ID --folder Archive
m2k mail move --folder Archive MESSAGE_ID_1 MESSAGE_ID_2 --dry-run
m2k mail move --folder Archive MESSAGE_ID_1 MESSAGE_ID_2
m2k mail attachment MESSAGE_ID --index 0 --output attachment.pdf
```

IMAP 同步使用 `BODY.PEEK`，不改已讀狀態。快取保存完整 RFC822，包含非 UTF-8 與二進位 MIME 的位元組；附件可離線取出。寄信不自動重試，轉寄保留原郵件與附件。線上旗標、搬移與刪除操作先核對 UIDVALIDITY，拒絕過期郵件識別碼。

`mail move` 支援同一來源資料夾、同一 UIDVALIDITY 的多個 ID。單封成功維持原有 JSON；批次成功回傳 `results` 的逐 ID 結果。`--dry-run` 連線後只用 EXAMINE、BODY.PEEK 與 SEARCH 檢查，回傳 `path`（`move`／`uidplus`／`guarded-copy-fallback`）、來源、目標、UID、Message-ID 及 `refusal`（無拒絕原因時為 null），不修改郵件或本機快取。所有能力路徑的 dry-run 都採相同保守檢查。

沒有 MOVE 與 UIDPLUS 時，移動會先確認每封信有唯一 Message-ID，且來源沒有集合以外的已刪除郵件，再一次複製整批、唯讀驗證目標、重新確認來源 UIDVALIDITY、標記刪除並重新比對完整 DELETED 集合，最後才執行一次 EXPUNGE。比對不符會撤回本次新增的刪除標記，保留原有標記；驗證失敗不標記或 EXPUNGE。失敗時目標可能保留副本，請先確認再重試。此流程不設定已讀狀態，成功才移除來源本機快取；不支援 MOVE／UIDPLUS 的伺服器仍無法提供跨用戶端原子性，其他用戶端若在最後檢查與 EXPUNGE 之間新增刪除標記，仍可能受影響。

CLI 的郵件／行事曆／聯絡人列表採固定欄寬單行，長欄位以 `…` 截斷，不畫框線、不折行；內嵌換行與控制字元轉為單行文字。完整 ID／主旨等欄位請使用 `--json ... list`。`meeting resources`／`meeting rooms` 文字列表也固定欄寬，精確模式每間房一列，分鐘模式每個連續空檔一列；加 `--json` 保留完整結果。

`mail show MESSAGE_ID` 顯示指定郵件的新回覆正文，去除 `>` 引用行，以及 `On … wrote:`、`-----Original Message-----` 等明確歷史回覆區塊。`mail thread --id MESSAGE_ID`／`--subject '主旨'` 預設只顯示同主旨最新一封的新正文；`--full` 才顯示歷史郵件與引用，`thread --full --limit N` 限制最新 N 封。過濾只影響顯示，不修改 SQLite、raw 或 TUI；`show --raw` 仍輸出完整 RFC822。JSON 讀信也套用同一預設，`text` 為新正文且 `raw` 為空；加 `--full` 恢復完整 text／raw。引用辨識採文字規則，特殊格式可用 `--full` 查看原文。

郵件保存日期優先採伺服器的 `INTERNALDATE`，避免新收到但 `Date` header 很舊的郵件被立即清除。

郵件串與批次清理請用 `m2k mail thread --help`、`m2k mail cleanup-preview --help`、`m2k mail cleanup-execute --help`。清理先建立綁定帳號、條件及精確 UID 集合的簽章預覽；執行會重新確認伺服器狀態及預覽效期，變動範圍漂移會拒絕執行。

## 新郵件 webhook

```sh
m2k mail watch --webhook-url https://your-receiver.example/hook
m2k mail watch --webhook-url https://your-receiver.example/hook --folder INBOX --poll-seconds 60
m2k mail watch --webhook-url https://your-receiver.example/hook --once
m2k --json status
```

沿用目前帳號與資料目錄，預設監看 INBOX，前景常駐、Ctrl-C 停止；可用 shell 背景執行。伺服器支援 IDLE 時保留同一條 IMAP 連線等待通知，最遲每 `--poll-seconds` 秒查詢新 UID；不支援 IDLE 時依相同間隔查詢。每批最多抓取 100 封新信，使用 `BODY.PEEK`，保存完整正文與 RFC822，保留已讀狀態。每個資料目錄同時只允許一個郵件 webhook 程序；一般 `sync`／`watch` 可並行。

首次啟動以當下 UID 建立基準，既有郵件不推送。相同帳號／資料夾／網址再次啟動會接續 SQLite 進度，補抓停止期間仍留在該資料夾的新 UID。更換資料夾或網址建立新的基準，原目的地的待送事件仍保留，使用原網址啟動才會送出；UIDVALIDITY 改變時也重新建立基準。這是新 UID 通知，搬入資料夾的郵件也會通知；已讀旗標、刪信不產生事件，已被刪除或移走而未曾抓取的郵件無法復原。

每封郵件以 HTTP POST 傳送 JSON：`schema_version: 1`、`event: "mail.received"`、`event_id`、`account`、`observed_at`、`folder`、`uid_validity`、數字 `uid` 及完整解析的 `mail`。`mail.text` 保留歷史引用；`mail.raw` 為空字串，附件提供中繼資料，二進位內容可由本機 `mail attachment` 取得。若 MIME 解碼失敗，仍保存原始郵件與可解析的標頭，`mail.text` 留空、`parse_error` 明確回報 `mail_parse_failed`，後續正常郵件繼續處理；正常事件的 `parse_error` 為 null。可透過環境變數 `M2K_WEBHOOK_TOKEN` 提供 Bearer token，憑證不寫入設定或事件。

2xx 才算送達；每次請求最多等 10 秒，不跟隨重新導向。失敗以 5 秒起、倍增至最多 300 秒重試，包含非 2xx HTTP 狀態；單筆失敗不延遲其他已到期的事件。正文、UID 進度與事件在同一筆 SQLite 交易保存；成功後移除待送事件，程序中斷後可接續。送達採至少一次語意，接收端應以 `event_id` 去重；HTTP 的 `Idempotency-Key` 與 `X-M2K-Event-ID` 都提供相同值。不同目的地的待送事件分開保存。

### Webhook 簽章（HMAC-SHA256）

設定環境變數 `M2K_WEBHOOK_SECRET` 後，每次 POST 都附上 `X-M2K-Timestamp`（Unix 秒）與 `X-M2K-Signature: sha256=<小寫十六進位>`；簽章是以 secret 對「`<timestamp>.`＋原始 body 位元組」計算的 HMAC-SHA256。重送（含重試）會以當下時間重新簽章，body 與 `event_id` 不變。secret 只從環境變數讀取，不寫入設定、SQLite、log 或 `status`；`status` 只以 `webhook_signing` 顯示目前環境是否啟用。未設定（或空字串）時完全不送這兩個 header，行為與先前相同。Bearer token 與簽章可同時使用。

接收端驗證：用**原始 body 位元組**（不要重新序列化 JSON）重算簽章，以常數時間比較，並拒絕時間窗口外（建議 ±5 分鐘）的時間戳以防重放；窗口內的重放再以 `event_id` 去重。

```python
import hmac, hashlib, time

def verify(secret: bytes, headers: dict, raw_body: bytes, tolerance=300) -> bool:
    try:
        ts = int(headers["X-M2K-Timestamp"])
        sent = headers["X-M2K-Signature"].removeprefix("sha256=")
    except (KeyError, ValueError):
        return False
    if abs(time.time() - ts) > tolerance:
        return False
    expected = hmac.new(secret, f"{ts}.".encode() + raw_body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, sent)
```

程序每輪輸出一行 JSON，包含 `state`、`transport`、`baseline`、`cursor`、`new_mail`、`parse_errors` 與 `delivery`，結束時輸出 `stopped`。`--once` 執行一輪查詢與送達嘗試後結束，可能仍有待送事件；以 `delivery.error` 與 `status.mail_webhooks` 的 `pending`／`last_error`／`next_attempt_at` 確認。此命令專注新信；歷史同步、旗標更新與保存週期由 `sync`／一般 `watch`／`prune` 執行，清理快取不移除待送 payload。

## 行事曆與聯絡人

```sh
m2k calendar collections
m2k calendar list
m2k calendar search '會議'
m2k calendar show EVENT_ID
m2k calendar create --file examples/event.ics --collection CALENDAR_URL
m2k calendar create --title '專案同步' --start '明天 14:00' --end '15:00' --location '北區 5F'
m2k calendar create --title '員工旅遊' --start 2026-11-12 --end 2026-11-13 --all-day
m2k calendar update EVENT_ID --file examples/event.ics
m2k calendar update EVENT_ID --start '下週一 下午兩點' --location ''
m2k calendar delete EVENT_ID
m2k calendar export EVENT_ID --output exported.ics
m2k contacts list
m2k contacts search '王小明'
m2k contacts create --file examples/contact.vcf --collection ADDRESSBOOK_URL
m2k contacts update CONTACT_ID --file examples/contact.vcf
m2k contacts export CONTACT_ID --output exported.vcf
```

`calendar create` 可用 `--file` 傳入完整 ICS，或用 `--title --start --end [--location --description --all-day]` 欄位建立個人事件（不帶 METHOD／ORGANIZER）。`--start`／`--end` 以台北時間解讀，與找會議室相同：`明天 14:00`、`下週一 下午兩點`、`2026-10-22 09:30` 或 RFC3339；`--end` 只寫時刻時沿用開始日期。全天事件的 `--end` 是最後一天（含），省略則為單日。`calendar update ID` 給欄位時只改指定部分，保留原 UID 與其他屬性（ATTENDEE、VALARM、X- 屬性等），SEQUENCE 有就加一，沒有而改了時間則補 `SEQUENCE:1`；只改 `--start` 時保留原長度（原本用 DURATION 的事件也一樣，DURATION 無法解析時拒絕改時間），`--location ''`／`--description ''` 移除該欄位，`--all-day=false` 搭配 `--start` 改回定時事件。欄位更新會拒絕重複事件（RRULE／RDATE／EXDATE／RECURRENCE-ID 或多個 VEVENT）、別人邀請的事件（ORGANIZER 不是自己）、有自己以外 ATTENDEE 的會議（例如 `meeting book` 建立、同時在會議室行事曆的那份，只改自己這份會留下會議室預約且與會者收不到通知，請改用 `meeting` 流程），以及會議室或他人的行事曆；欄位建立的 `--collection` 也只接受自己的行事曆。這些情況仍可用 `--file` 明確處理整份 ICS。`--file` 的錯誤順序與以前相同：先檢查 `sync.lock` 與帳號（展示資料、密碼），再讀檔。

原始 ICS／VCF 完整保存。建立使用 `If-None-Match: *`，更新與刪除使用 ETag `If-Match`，避免蓋掉伺服器上的新版本。`calendar delete ID` 與 `contacts delete ID` 會刪除遠端資料。檔案輸出不覆寫既有檔案。

若 PUT 已接受，但無法取得新 ETag 或保存本機快取，結果會包含遠端資源網址並要求重新同步；`--json` 回傳 `partial` receipt，程序以非零狀態結束。此時先確認遠端現況，不要直接重新建立事件。

## 人員與部門目錄

```sh
m2k directory tree
m2k directory self --online
m2k directory sync --scope company
m2k directory sync --scope department
m2k directory sync --scope department --addressbook /EXAMPLE_ALL/Engineering/Platform
m2k directory search '王小明' --online --addressbook /EXAMPLE_ALL/Engineering/Platform
m2k directory resolve '王小明'
```

目錄保留通訊錄階層與聯絡人所屬分支，姓名解析必須得到唯一 email。自己身份以遠端或已同步的 contact 為依據；沒有資料時明確回報缺資料。跨部門按需查找與全公司同步由使用者明確啟動；TUI 聯絡人頁只讀本機快取。

首次可明確執行 `--scope company` 初始化企業目錄，再使用部門／分支 refresh；若自己所屬部門尚未確認，使用 `--addressbook` 指定實際分支。

## 會議與會議室

```sh
m2k date-range 明天
m2k meeting resources --site 北區 --capacity 8
m2k meeting events --attendee colleague@example.com \
  --start 2026-10-07T09:30:00+08:00 --end 2026-10-07T17:30:00+08:00
m2k meeting free-busy --attendee colleague@example.com \
  --start 2026-10-07T09:30:00+08:00 --end 2026-10-07T17:30:00+08:00
m2k meeting slots --attendee colleague@example.com --minutes 60 \
  --start 2026-10-07T09:30:00+08:00 --end 2026-10-07T17:30:00+08:00
m2k meeting rooms --site 北區 --capacity 8 \
  --start 2026-10-07T14:00:00+08:00 --end 2026-10-07T15:00:00+08:00
m2k meeting prepare --title '專案同步' --attendee colleague@example.com \
  --start 2026-10-07T14:00:00+08:00 --end 2026-10-07T15:00:00+08:00 \
  --location '北區辦公室' --output invite.ics
m2k meeting invite --file invite.ics --dry-run
m2k meeting review --attendee colleague@example.com --room ROOM_NAME \
  --start 2026-10-07T14:00:00+08:00 --end 2026-10-07T15:00:00+08:00
m2k meeting book --file invite.ics --room ROOM_NAME --dry-run
m2k meeting book --file invite.ics --room ROOM_NAME
m2k meeting invite --file invite.ics --send
m2k meeting rsvp MAIL_ID --response accept --dry-run
m2k meeting rsvp MAIL_ID --response decline --comment '那天出差'
```

房間資源結合 DAV 回覆與 site profile 的據點與會議室清單。空檔判定使用有時區的時間範圍；權限不足、事件無法解析或重複事件無法可靠展開時，結果是未知，不能視為空閒。預約前 live recheck 房間衝突，個人衝突回報警告。

個人與房間的兩次 DAV PUT 無法形成跨伺服器交易，因此會回報逐一成功、失敗及補救資訊。預約行事曆與寄送邀請是兩個明確動作；SMTP 使用 `text/calendar; method=REQUEST`。收到的邀請用 `meeting rsvp MAIL_ID --response accept|decline|tentative` 回覆：從郵件的 `text/calendar`（`METHOD:REQUEST`，ICS 沒寫時看 Content-Type 的 `method=`）產生 RFC 5546 `METHOD:REPLY`，保留原 UID、SEQUENCE、ORGANIZER 與 RECURRENCE-ID，ATTENDEE 只放自己並帶 PARTSTAT，以 `text/calendar; method=REPLY` 寄給主辦人。`--dry-run` 只輸出 ICS 與收件者；自己不在 ATTENDEE、自己是主辦人、或 METHOD 不是 REQUEST 時明確報錯且不寄。寄信不自動重試；展示資料拒絕實際寄送。RSVP 不修改自己行事曆裡的 PARTSTAT（輸出 `calendar_updated: false`），需要時另用 `calendar update`。

若個人行事曆已成功、房間寫入失敗，先依結果確認現況，再使用同一份 ICS 與 `meeting book --file invite.ics --room ROOM_NAME --target room` 補寫；不會重新建立個人事件。

`--target self` 可單獨處理個人行事曆。預約輸入限單次事件；重複規則或 `RECURRENCE-ID` 需另外處理，避免用展開實例覆蓋整個重複事件。取消及透明事件不佔用忙碌時段。

寄信邀請與 DAV 儲存的 ICS 各自遵守用途：邀請保留 `METHOD:REQUEST`，行事曆資源不帶 `METHOD`，預約保留原始 UID。[CalDAV RFC 4791](https://www.rfc-editor.org/rfc/rfc4791.html#section-4.1)

`meeting rooms --json` 的 `calendars` 列出每間房的 `source`（cache／remote／demo／unknown）、`fetched_at` 與 coverage 起迄。加 `--refresh` 同時略過會議室目錄與行事曆快取，重新讀取並更新快取。會議室目錄與已忙碌的精確時段可沿用 10 分鐘內且涵蓋查詢範圍的快取；可能有空的房間及 `--minutes` 空檔搜尋每次重新查詢，避免其他人新增預約後仍顯示有空。重新查詢失敗顯示未知，不以舊快取宣告有空。訂房的 live recheck 仍每次執行。

`meeting book --dry-run` 會即時檢查個人／房間行事曆，因此仍需認證，但不寫入行事曆。系統密碼庫讀取最多等待 15 秒；macOS 授權逾時時（通常是升級後第一次讀取舊項目），m2k 會結束 `security` 程序；請重新執行並在授權視窗允許 `security` 存取（可能需要輸入登入密碼），或重新執行 `m2k auth login`。DAV 每次 HTTP 請求有 20 秒逾時；多個 discovery／查詢請求的總時間可能更長。

DAV collection 身分比對接受路徑中的 `%40`／`@`、安全字元的 percent-encoding 與結尾斜線差異，舊資料庫帳號資訊也可沿用。密碼庫項目識別仍維持原有規則；若改過設定 URL 而找不到密碼項目，請重新執行 `m2k auth login`。

## TUI

採 tokscale 的 header／tab、分欄與 footer 結構，使用 cyan 選取提示與可讀對比。寬度至少 112 欄時並列資料夾、列表、詳情；較窄時切換詳情畫面。最低 40 欄 × 14 列，行事曆需 40 × 20；建議 120 × 32，可完整顯示月格。邊框使用 ASCII，避免 CJK 終端的模糊字寬造成畫面錯位。

| 按鍵 / 滑鼠 | 操作 |
| --- | --- |
| `1` / `2` / `3` / `4`、Tab | 郵件／行事曆／聯絡人／找會議室 |
| ↑ / ↓、j / k | 選取資料；詳情內捲動；行事曆持續選取活動 |
| ← / →、h / l | 切換資料夾；行事曆 ← / → 選取活動，h / l 移動日期視窗 |
| 行事曆 `v`、`d` / `w` / `m` | 循環切換 3-day → week → month → 3-day；直接切換三天／週／月視圖 |
| Enter、Esc | 讀取詳情／返回列表 |
| `/` 或點搜尋框 | 輸入搜尋；Enter 立即套用 |
| `[` / `]`、PageUp / PageDown | 上一頁／下一頁；行事曆依視圖移一天／一週／一月；PageUp / PageDown 在閱讀詳情時捲動 |
| Home / End、g / G | 第一頁／最後一頁；行事曆選取第一個／最後一個活動；閱讀詳情時移到正文開頭／結尾 |
| 郵件詳情 `R` | 回覆草稿：預設帶 `Re:` 主旨、收件人與引用原文；有 `$EDITOR` 就暫時離開 TUI 編輯，否則用內建輸入（只能在文末追加，Ctrl-D 完成）。編輯後進確認畫面：`y` 寄出（與 `mail reply` 同一路徑，不重試）、`d` 只存草稿、`e` 再編輯、Esc 存草稿離開；草稿放在 `<data-dir>/drafts/reply-*.txt`，寄送失敗顯示原因並保留，展示資料只能存草稿，Ctrl-C 會先存草稿再離開 |
| `r` | 郵件增量更新；行事曆更新選取日期前後 30 天；聯絡人重新讀取本機資料 |
| 行事曆 `n` / `e` / `x` | 新增事件（預填選取活動當天或今天的第一個空白整點）／編輯選取事件／刪除選取事件。表單用 Tab、↑ / ↓ 換欄、Space 切換全天、Enter 預覽；確認畫面 `y` 才寫入（與 `calendar create/update/delete` 同一路徑、ETag 條件寫入、不重試），`e` 回表單、Esc 返回。重複事件、別人邀請的事件、有其他與會者或會議室的會議，以及會議室行事曆會拒絕並說明原因；展示資料不寫入；表單開著時 Ctrl-C 先把輸入存成 `drafts/event-*.txt` 再離開 |
| 聯絡人 `Shift-R` | 手動背景同步全公司目錄，完成後更新列表 |
| 點擊 | 頁籤、資料夾、選列／詳情、搜尋框、上一頁／下一頁 |
| 滾輪 | 捲動游標所在的資料夾、列表或詳情 |
| `?` | 按鍵說明 |
| `q` / Ctrl-C | 離開並還原終端；會議室表單中使用 Ctrl-C |

列表每頁最多 100 筆摘要，SQLite 負責篩選、計數與穩定排序；正文只在開啟詳情時載入，關閉或改選時釋放正文記憶體。搜尋仍涵蓋本機解析的正文與附件檔名。輸入暫停 200 ms 後套用本機搜尋、400 ms 後背景搜尋 IMAP；Enter 可立即套用。行事曆與聯絡人使用本機搜尋。

聯絡人頁只讀本機快取，即使完全沒有聯絡人也不會自動掃描公司目錄。需要時按 `Shift-R`，在背景執行與 `directory sync --scope company` 相同的手動全量同步；也可透過 CLI 手動更新公司、部門或指定分支。載入中仍可切換頁籤或搜尋，成功後更新本機列表，失敗在狀態列顯示原因，按 `Shift-R` 可重試。其他程序持有 `sync.lock` 時拒絕啟動新同步並顯示原因；展示模式只讀展示資料。郵件頁的 `r` 不會同步聯絡人。

行事曆預設為三天時段表（3-day），保留重疊活動的分欄顯示。按 `v` 循環切換三天、週、月視圖，或按 `d`／`w`／`m` 直接切換；面板標題顯示目前視圖，所有日期與時刻都採台北時間。週視圖（week）與三天視圖共用時段格：左側時間軸，週一到週日並列七天，重疊活動分欄顯示；七天欄位較窄，日期與活動標題可能截短，完整內容仍可在下方詳情查看。月視圖（month）以週一到週日排出當月日格，顯示活動數，空間足夠時也顯示截短標題。`[`／`]`、`h`／`l` 在三天視圖移一天、週視圖移一週、月視圖移一月。切換視圖保留搜尋、行事曆篩選與選取活動；若縮小視窗會排除選取活動，則將視窗移到該活動。

月視圖高度不足時只顯示部分週，面板標題標示可見範圍；選取活動會將所在週帶入畫面，`j`／`k` 仍可逐一選取整月活動。

各視圖均可用 ↑／↓、←／→、`j`／`k` 選取活動，`Home`／`End`、`g`／`G` 選取第一個／最後一個活動。月視圖可點活動標題或有活動的日格標頭（保留當天已選活動，否則選第一個）；空白日格沒有活動可選。選取後，下方詳情顯示完整標題、台北起迄時間、地點與正文，方便閱讀格內截短的標題。`Enter` 進入閱讀，`PageUp`／`PageDown` 捲動正文；`Esc` 收起閱讀，再按一次清除選取。

行事曆頁按 `n` 新增、`e` 編輯、`x` 刪除。新增預填選取活動當天（沒有選取時為視窗內的今天或第一天）09:00–18:00 第一個不與現有活動重疊的整點一小時，今天則從下一個整點起算；時間欄位接受與找會議室相同的人話寫法。編輯只送出改動的欄位，保留原 UID 與其他屬性，並用開啟表單時的 ETag 做 `If-Match`；伺服器版本已被其他人修改時回 412，畫面顯示原因並保留輸入；Esc 關閉表單時會暫存自己改過的欄位，按 `r` 更新後再按 `e`（新增則按 `n`）帶回，沒改的欄位使用伺服器最新值，不會蓋掉別人的修改。寫入失敗不自動重試，按 `y` 才再送一次；若遠端已接受但本機快取或 ETag 未確認，表單鎖住、只能關閉，請按 `r` 更新確認，不要重新建立。寫入期間 Ctrl-C 會等結果。

行事曆頁的 `r` 以目前選取事件的日期為中心更新前後 30 天，沒有可用日期時以目前視窗中心為準。更新使用 ETag 重用未變更的事件，保留查詢區間外快取；TUI 自己不會啟動定時同步。

郵件混合搜尋依資料夾、UIDVALIDITY 與 UID 的穩定 ID 合併去重；保留本機完整快取，遠端摘要只存於目前搜尋工作階段。IMAP 使用唯讀 `EXAMINE` 與 `BODY.PEEK`，中文搜尋使用 UTF-8 literal；讀信不改已讀狀態。網路或伺服器搜尋失敗會顯示錯誤並保留本機結果。

主旨與寄件人的編碼、IMAP 的中文資料夾名稱會轉為可讀文字，遠端資料夾識別與已讀狀態保持相同。舊快取若有原始郵件但正文空白，開啟詳情時會從本機原始郵件重新解析正文；優先顯示純文字，只有 HTML 時轉為文字，不需重新下載郵件。

遠端搜尋最多 500 筆摘要、整體 30 秒，掃描最多 32 個各 2048 封的窗口，從每個資料夾的新郵件向前搜尋；全部資料夾依名稱排序。尚有未掃描窗口或資料夾時顯示截斷提示，請縮小搜尋條件或選定資料夾。遠端正文按需載入並再次核對 UIDVALIDITY；未同步的遠端郵件在離線時沒有正文。本機搜尋 3 個字元以上走 SQLite FTS5 trigram 索引，1–2 個字元退回逐筆掃描；兩者都是不分大小寫的子字串比對，結果、排序與分頁相同。

### 找會議室

按 `4` 或點「找會議室」，以 Tab 或滑鼠選欄位。開始與結束可輸入 `明天 14:00`、`下週一 下午兩點` 等日期與時刻，自然語言以 `Asia/Taipei` 解讀；也接受完整 RFC3339，例如 `2026-10-12T14:00:00+08:00`。站點選項來自 site profile 的 `sites`，留空查全部站點。

首次開啟預設最少 6 人、會議 120 分鐘、工作時段 `09:30-17:30`；開始自動帶入下一個足以容納會議的工作日時段。結束留空且有會議分鐘時，從開始時間搜尋到當日工作時段結束，必須放得下指定長度。清空會議分鐘則改查精確起迄，這時必須明確填入結束時間；填入分鐘及結束時間時，列出兩者之間長度足夠的最長連續空檔。

按 Enter 或點「查詢會議室」後在背景執行共用的 `meeting rooms` 查詢；F5 略過快取立即重新查詢，顯示載入狀態、可用房間的名稱／容納／站點／可用空檔（或來源）及錯誤原因。分鐘模式每一列是一段最長連續空檔；在結果列再按 Enter，可於該段內填實際開始／結束（結束留空則用會議分鐘），並填入主旨與與會者。確認後保存 `drafts/<內容識別碼>/invite.ics` 與 `draft.json`，顯示可捲動的訂房與寄送指令。指令固定使用原本 `--data-dir`、實際會議室 `--url` 及同一份 ICS；重複保存相同內容沿用相同 UID，已有手動修改的 ICS 會保留並回報錯誤。只有草稿保存，不會自動寫入遠端或寄信。表單驗證成功送出時，站點、人數、會議分鐘與工作時段保存於帳號資料目錄的 `room_finder_prefs.json`，下次啟動沿用；開始與結束日期不保存，避免沿用過期的日期。未知或 403 的房間排除於可用結果；PageUp／PageDown 或狀態區滾輪可查看錯誤。段內起迄表單按 Esc 回空檔列表；再 Esc 返回郵件。

房間行事曆與成功探索的目錄保存於帳號資料目錄的 `room_cache.sqlite3`（0600）。TTL 為 10 分鐘，初次查詢至少抓取開始日起 30 天；快取綁定帳號、端點、房間 URL 與覆蓋區間。只保存成功回覆的原始行事曆，重新判定指定區間的忙碌／未知；403 或傳輸失敗不保存為空白行事曆。未命中時最多四路並行查詢，CLI 與 TUI 共用快取與查詢函式。

`meeting rooms --minutes N` 列出查詢窗／工作時段內每間房的最長連續空檔，`N` 是最低長度，不再每 15 分鐘滑出重疊切片。全日 09:30–17:30 空閒、`--minutes 120` 時只回一段 8 小時空檔；有忙碌事件隔開時可回多段（一空檔一筆），各段保留真正的事件邊界。例如找兩小時、某房上午 09:30–12:00 與下午 13:00–17:30 都空 → 兩筆整段；若中間再有半小時 booking，連續空檔會再切開，夠長的段各列一筆。可查一天或最多 31 天，預設工作時間 `09:30-17:30`，`--working-hours` 可覆寫。只有完整可讀且有足夠長空檔的房間會列入 `rooms`／`available_slots`；`availability` 保留整個查詢窗的狀態，因此 busy 仍可能有可用空檔。CLI 與 TUI 共用此路徑。沒有 `--minutes` 時保留精確起迄查詢；工作時段依 `--start` 的固定 offset 每天各自裁切，不跨夜。獨立的 `meeting slots` 共同排程候選維持原本 `--step` 行為。

選定某一筆連續空檔後，再在該段範圍內挑實際要訂的起迄（長度可為查詢的會議分鐘，或自訂但仍須完全落在空檔內）。TUI 分鐘模式：↑↓ 選列後 Enter 進入段內起迄表單，確認後顯示 `meeting prepare`／`meeting book --dry-run` CLI（不自動寫入正式信箱）。CLI 對齊方式：先用 `meeting rooms --minutes` 取得整段空檔，再把 `meeting prepare --start/--end`（與後續 `meeting book`）設在選定段內，例如空檔 `13:00–17:30` 可訂 `14:00–16:00`。

```sh
m2k --json meeting rooms --site 北區 --capacity 6 \
  --start 2026-10-12T00:00:00+08:00 --end 2026-10-13T00:00:00+08:00 \
  --minutes 120 --working-hours 09:30-17:30
```

自然語言日期可先用 `m2k date-range '下週五'` 取得當地日期範圍，再將實際會議的時刻及時區組成精確的 `--start`／`--end`。日期解析沿用 chrono 與明確的英中規則，保留整週、整月的既有邊界；不依賴 `rosetta-date`。`next tuesday` 表示下一次週二（今天為週二則是七天後），`tuesday`／`週二` 允許今天。`下週五` 表示下一個曆週的週五，繁體 `週／兩／後` 會正規化。以 Asia/Taipei 2026-10-07 為基準：`next tuesday` 是 10/13、`下週五` 是 10/16、`週六` 是 10/10、`兩天前` 是 10/05。支援星期名稱／常見英文縮寫、數字或中文數字的 N 天前後；不接受未定義的句子。

### 本機操作驗證

```sh
cargo run -- --data-dir /tmp/m2k-demo demo --no-tui
cargo run -- --data-dir /tmp/m2k-demo tui
```

1. 點選郵件列表查看正文；Esc 返回。以滑鼠選資料夾，點行事曆／聯絡人頁籤。
2. 點搜尋框輸入「歡迎」，Enter，確認展示資料的本機搜尋結果。展示模式不連線。
3. 郵件／聯絡人大量資料時點上一頁／下一頁，或按 `[`／`]`；每頁 100 筆。行事曆按 `2` 確認預設三天時段表（台北時間），活動詳情固定在下方。按 `v` 循環切換三天／週／月，再試 `d`／`w`／`m`；確認標題顯示目前視圖、週視圖與三天視圖同樣有左側時間軸和時段格，週一到週日並列且重疊活動分欄，月視圖有日格與活動標記。縮窄終端機時，確認週視圖欄位與標題截短後仍可選取活動。各視圖用方向鍵、`j`／`k`、`Home`／`End` 或滑鼠選活動，確認下方標題、時間、地點、正文隨選取更新；月視圖也可點有活動的日格。`[`／`]`、`h`／`l` 分別移一天／一週／一月。`Enter` 閱讀，`PageUp`／`PageDown` 捲動，`Esc` 收起閱讀／清除選取；搜尋不存在的文字或移到空視窗，確認顯示無活動且詳情清空。`r` 更新選取日期或視窗中心前後 30 天。按 `n` 確認表單預填空檔，輸入標題後 Enter 看確認畫面，`y` 會提示展示資料不寫入；選會議室活動按 `e`／`x`，狀態列說明拒絕原因。
4. 點「找會議室」，確認預設 6 人／120 分鐘／`09:30-17:30`，開始輸入 `明天 14:00`，結束留空，Enter；也可試 `下週一 下午兩點`。展示模式只使用已有快取，缺資料顯示未知。清空分鐘時填入結束，例如 `明天 15:00`，改查精確起迄。
5. 已登入的正式帳號可在同樣畫面驗證本機與 IMAP 的合併結果，以及真正的會議室可用性。

無 TTY 可執行 `cargo run -- --data-dir /tmp/m2k-demo tui --snapshot --width 120 --height 32`。進階目錄／會議預約與郵件、聯絡人的寫入操作仍透過 CLI 執行。先前的本機查詢、DAV 探索與合成資料量測見 [EFFICIENCY.md](EFFICIENCY.md)。

## Agent skill 安裝

薄 skill 原始檔在 [skills/m2k/SKILL.md](skills/m2k/SKILL.md)，只列使用情境與少量入口範例；agent 必須執行 `m2k --help` 與相關子命令 `--help` 查目前介面。

```sh
m2k skills --help
m2k skills install
# Cursor 專用位置，或指定獨立 skills 根目錄：
m2k skills install --path ~/.cursor/skills
```

預設安裝 `~/.agents/skills/m2k/SKILL.md`，相同內容重跑不改寫，並印出實際路徑。`--path`（別名 `--dest`）指向 skills 根目錄，程式在其下建立 `m2k/SKILL.md`；也支援 `--json` 的 `path`／`changed` 結果。Skill 內嵌於 binary，安裝時不需原始碼目錄。

## 核心工作流程覆蓋

| 原情境 | m2k 對應 |
| --- | --- |
| Mail2000 單一帳號與連線測試 | `init`、`auth` |
| 收件匣同步、本機搜尋、郵件串 | `sync --kind mail`、`mail search`、`mail thread` |
| 清理預覽、精確範圍確認與執行 | `mail cleanup-preview`、`mail cleanup-execute` |
| 全公司／部門／分支目錄、自己身份 | `directory sync/tree/search/resolve/self` |
| 個人與同事行事曆、日期範圍 | `calendar`、`meeting events`、`date-range` |
| 衝突、共同空檔、方案檢查 | `meeting free-busy/slots/review` |
| 房間候選與預約 | `meeting resources/rooms/book` |
| 邀請草稿與寄送 | `meeting prepare/invite` |
| 回覆會議邀請（RSVP） | `meeting rsvp` |
| 本機資料、保存週期與重跑 | SQLite、`config set`、`watch/prune/status` |

此階段聚焦 Mail2000；其他系統整合不在範圍內。

終端版面參考 tokscale 的 `crates/tokscale-cli/src/tui`。功能以實際程式行為與測試為依據。

## 驗證與模組

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

線上路徑（TLS IMAP/SMTP、DAV 同步、會議室預約 PUT、webhook）可用本機假 Mail2000 驗證，流程見 [examples/m2k_fixture/README.md](examples/m2k_fixture/README.md)；`cargo test --test fixture_e2e` 會自動跑一次。

`config` 管理單一帳號及密碼庫，`provider` 管理真實郵件與 DAV 協定，`store` 管理 SQLite 與保留政策，`sync` 管理提交與錯誤狀態，`directory`／`meeting`／`mail_workflow` 組合原核心情境，`tui` 讀取相同本機資料。
