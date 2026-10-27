# m2k 本機假 Mail2000 伺服器

`examples/m2k_fixture` 在 127.0.0.1 上同時提供 IMAP、SMTP、CalDAV、CardDAV，共用一組帳密，讓**實際的 m2k binary** 跑 `auth check`、`sync`、`mail send|flag|move`、`meeting book`、`mail watch` 這些線上路徑。它是 cargo example，不會編進 release binary，也沒有新增任何依賴。

| 服務 | 傳輸 | 說明 |
| --- | --- | --- |
| IMAP | TLS | LOGIN、LIST、SELECT/EXAMINE、UID SEARCH/FETCH/STORE/MOVE/COPY/EXPUNGE、APPEND、IDLE |
| SMTP | implicit TLS | AUTH PLAIN/LOGIN；寄給 fixture 帳號的信同時投遞到 INBOX |
| CalDAV / CardDAV | HTTP（m2k 只允許 loopback 用 `http://`） | PROPFIND、REPORT（query/multiget、time-range、EMAIL filter；加 `--sync-collection` 時另有 RFC 6578 sync-collection）、GET、PUT（If-Match/If-None-Match）、DELETE |
| Webhook sink | HTTP `POST /webhook` | 給 `m2k mail watch` 推送用 |

## TLS 怎麼信任

啟動時用系統的 `openssl` CLI 產生一張 30 天的測試 CA 與 loopback 伺服器憑證（`<data-dir>/tls/ca.pem`）。m2k 只在 IMAP/SMTP 主機是 `localhost`、`127.0.0.1` 或 `::1` 時讀 `M2K_TEST_ROOT_CERT`，把它加為**額外**的信任根；憑證與主機名稱驗證照常進行。若主機不是 loopback，設了這個變數會直接拒絕連線。沒設這個變數時，m2k 會拒絕 fixture 的憑證（`tests/fixture_e2e.rs` 有驗證這一點）。

SMTP 若使用 465／587 以外的 port，只有 loopback 主機允許，並一律當成 implicit TLS。

## 初始資料與副作用

每次啟動都從空狀態重新 seed（UIDVALIDITY 會跟著變）：

- 郵件：INBOX 4 封（其中 1 封有附件、1 封是 Carol 寄來的 `METHOD:REQUEST` 會議邀請）、Archive 1 封、Sent 1 封、Trash 空
- 行事曆：個人 `fixture@example.test` 2 個事件；會議室 `room-a@example.test` 明天 06:00–07:00Z 已有預約
- 通訊錄：principal `mPA.000@<使用者網域>`（預設 `mPA.000@example.test`） → `company/directory/`，3 張 vCard
- `samples/booking.ics`（明天 08:00–09:00Z）與 `samples/body.txt`

每個 DAV HTTP 請求另外記在 `<data-dir>/http.jsonl`（method、path、depth、status、回應的 `<D:response>` 筆數、REPORT 種類），用來比對同步實際花了幾個請求。

所有副作用都寫進 `<data-dir>/events.jsonl`，一行一個 JSON（`kind` 為 `smtp.message`、`dav.put`、`dav.delete`、`imap.store`、`imap.move`、`imap.copy`、`imap.expunge`、`imap.append`、`webhook`）；原始內容另存在 `smtp/`、`dav/`、`imap/`、`webhook/`，事件的 `file` 欄位指向它。端點資訊寫在 `<data-dir>/fixture.json`。

`--data-dir` 必須是新目錄、空目錄，或之前 fixture 用過的目錄（有 `.m2k-fixture` 標記），否則拒絕啟動。

## 端到端流程

終端 1：啟動 fixture。

```bash
cargo run --example m2k_fixture -- --data-dir /tmp/m2k-fx --imap-port 19993 --smtp-port 19465 --http-port 18080
```

終端 2：用實際 binary 走一遍。

```bash
cargo build
export M2K_PASSWORD=fixture-password M2K_TEST_ROOT_CERT=/tmp/m2k-fx/tls/ca.pem
alias m2k='./target/debug/m2k --data-dir /tmp/m2k-fx-client'

m2k init --username fixture@example.test --server http://127.0.0.1:18080
m2k config set --imap-port 19993 --smtp-port 19465
m2k auth check                 # verified: imap, smtp, caldav, carddav
m2k sync                       # mail 6、calendar 2
m2k mail list
m2k mail send --to bob@example.test --subject 'Fixture 寄送測試' --body-file /tmp/m2k-fx/samples/body.txt
grep smtp.message /tmp/m2k-fx/events.jsonl     # fixture 收到的信，原文在 smtp/

ROOM='http://127.0.0.1:18080/cgi-bin/cal/caldav/calendars/room-a@example.test/default/'
m2k meeting book --file /tmp/m2k-fx/samples/booking.ics --url "$ROOM" --dry-run   # 不會 PUT
m2k meeting book --file /tmp/m2k-fx/samples/booking.ics --url "$ROOM"             # 個人＋會議室各 PUT 一次
grep dav.put /tmp/m2k-fx/events.jsonl          # 兩筆 PUT，ICS 原文在 dav/
```

其他可驗的線上路徑：

- `m2k mail flag <id> --flag flagged`、`m2k mail move <id> --folder Archive` → `imap.store`／`imap.move`
- 把 `booking.ics` 的時間改成 06:00–07:00Z 再 `meeting book`，會因會議室衝突拒絕，且沒有 PUT
- `m2k mail watch --webhook-url http://127.0.0.1:18080/webhook --once` 建立基準；寄一封給 `fixture@example.test` 後再跑一次，`events.jsonl` 會出現 `webhook`

`cargo test --test fixture_e2e` 會在程序內啟動 fixture（隨機 port），自動跑上面的 init → auth check → sync → mail send → meeting book 流程。

## sync-token（RFC 6578）

加 `--sync-collection` 啟動時，行事曆 collection 的 PROPFIND 會回 `DAV:sync-token` 與含 `sync-collection` 的 `supported-report-set`，REPORT sync-collection 回傳 token 之後變更的事件（刪除為 404）。token 帶每次啟動隨機的 epoch，fixture 重啟後舊 token 會得到 403 `valid-sync-token`，模擬伺服器重設。不加這個旗標時行為與以前相同。

```bash
cargo run --example m2k_fixture -- --data-dir /tmp/m2k-fx --http-port 18080 --sync-collection
m2k sync --kind calendar       # 完整同步，順便保存 token
C='http://127.0.0.1:18080/cgi-bin/cal/caldav/calendars/fixture%40example.test/default'
curl -u fixture@example.test:fixture-password -X DELETE "$C/fixture-customer.ics"
m2k --json watch --kind calendar   # 第一輪：PROPFIND + sync-collection REPORT，Ctrl-C 停止
tail -n 3 /tmp/m2k-fx/http.jsonl
```

`cargo test --test fixture_sync_token` 用實際 binary 比較有無 sync-collection 時同一組變更的請求數與 REPORT 回應筆數。

## 已知限制

- 這不是通用 IMAP/DAV 伺服器，只實作 m2k 會送的指令；不支援的指令回 `NO`／`BAD`，m2k 端會顯示錯誤。
- DAV href 使用 `%40`（跟 m2k 預設設定的 URL 一樣），用來覆蓋 m2k 對 `%40`／`@` 等價的處理。
- 需要 PATH 上有 `openssl`（macOS 內建 LibreSSL 或 Homebrew OpenSSL 都可）。
