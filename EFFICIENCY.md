# Mail2000 效率驗證

日期：2026-10-07。範圍：參考先前 Mail2000 連接器（TypeScript）的既有修正，驗證 m2k 本機查詢及 DAV 流程。參考的程式沒有修改。

## 歷史來源

- 先前的連接器修正了上游資料完整下載後才套用 `limit` 的問題，加入 IMAP 搜尋預算及 DAV 內容抓取前的 URL 篩選與容量檢查。
- 另一版連接器改用 home tree 與部門範圍同步，避免逐資料夾探索。
- 連接器的工具 rediscovery、設定重啟與 CardDAV 命中保留，屬於不同問題；不能把它當成本次效率修正的效能證據。

m2k 是持久化離線 CLI，會保存保留期間內的完整郵件與附件。因此不直接套用 connector 的 UID 搜尋預算或只在 read 時下載正文：那會改變完整同步與離線閱讀的需求。IMAP 保留日期範圍、分批 FETCH 及既有正文重用。

## 已修正的工作

| 問題 | 修正 |
| --- | --- |
| 清單先解碼所有 Item／RFC822，最後才排序及限制筆數 | SQLite 執行篩選、Unicode 搜尋、排序及 LIMIT；以符合排序的索引取得結果 |
| TUI 保留每封郵件的完整正文、RFC822 與附件資料 | 清單使用輕量摘要；Enter 才讀取選取項目的完整資料 |
| 每輪同步為了檢查快取而配置整封 decoded RFC822 | 使用既有 base64 library 與固定大小 buffer 驗證；仍掃描編碼資料，但不配置整封解碼 buffer |
| 郵件保存清理讀取不需要的 raw／flags | 各資料域只讀取清理所需欄位 |
| 同一個會議操作反覆建立 HTTP client、探索同一集合 | 一個 command 共用 DavSession 與已驗證的集合，使用 canonical URL HashMap 查找 |
| 個人與房間 PUT 重新探索個人端點 | 使用 live recheck 已確認的實際目標集合；新 session 也探索指定集合 |
| 目錄探索與父／子集合資料重複抓取 | 重用探索結果，每個選取集合只 REPORT 一次；不以回覆中的路徑深度猜測完整性 |

Mail 搜尋使用已解析的主旨、內文、地址、日期及附件檔名，不搜尋 base64 編碼內容。行事曆與聯絡人的 ICS／VCF 原文仍可搜尋。Unicode 不分大小寫、`limit=0` 的無上限語意與既有排序都保留。

目錄只在完整解析且接受 `Depth: infinity` 回覆後，才把 home tree 視為完整。伺服器不支援時，退回逐層 `Depth: 1` 探索，包含回覆未列出的深層分支。[RFC 4918](https://www.rfc-editor.org/rfc/rfc4918.html#section-9.1) 規定伺服器必須支援 0／1，但 infinity 可以停用；測試覆蓋兩條路徑。

## 本機量測方式

使用 macOS arm64 的原生 release binary、同一個 SQLite 合成 fixture：4,000 封郵件，每封 65,536 個 base64 字元，INBOX 100 封。每個案例先暖機一次，再取五次的中位數。時間是 Python 呼叫 `/usr/bin/time -l m2k` 的 wall time，包含程序建立、查詢與輸出至 `/dev/null`；記憶體是 time 回報的 m2k process peak RSS。fixture 沒有真實郵件或企業帳密。

基準為原骨架版本的已安裝 release binary，保留後再比較本次修正版。原始樣本與 binary SHA-256 見 [修正前](benchmarks/queries-before.json)／[修正後](benchmarks/queries-after.json)。

| 案例 | 修正前時間 | 修正後時間 | 修正前 RSS | 修正後 RSS |
| --- | ---: | ---: | ---: | ---: |
| 全部郵件取 10 筆 | 80.00 ms | 8.41 ms | 331.80 MiB | 11.89 MiB |
| INBOX 取 10 筆 | 61.47 ms | 6.49 ms | 21.02 MiB | 11.91 MiB |
| 搜尋無符合結果 | 72.08 ms | 54.56 ms | 13.02 MiB | 12.81 MiB |
| TUI 載入與首畫面 snapshot | 74.30 ms | 44.23 ms | 339.86 MiB | 22.77 MiB |

可重跑：

```sh
python3 scripts/benchmark_queries.py \
  --binary /path/to/m2k \
  --fixture-dir .tmp/efficiency-fixture \
  --output .tmp/efficiency-results.json
```

這是暖快取的本機量測，不代表正式站台的網路延遲、第一次完整同步或正式帳號的效能。

## 流程驗證

`cargo test --offline --locked`：99 passed、0 failed、0 ignored。Clippy（warnings 視為錯誤）及 release build 通過。測試包含原生 CLI process 與 loopback HTTP fixture。

| HTTP fixture 情境 | 實際請求數與結果 |
| --- | --- |
| 完整 home tree 的四個目錄集合 | 6：principal、home tree、四個集合各一次 REPORT |
| 不支援 infinity 的隱藏分支 | 12；逐層探索找到 PLATFORM，不遺漏分支 |
| 部門範圍 | 4；只抓選定範圍，保留跨集合 memberships |
| 自己身份 | 3：principal、home、root EMAIL REPORT |
| 個人與房間預約 | 6：兩個目標各一次 PROPFIND／REPORT／PUT；原流程 8 |
| 只寫個人行事曆 | 3：PROPFIND／REPORT／PUT；原流程 4 |

測試同時驗證中文搜尋、摘要開啟完整正文、重載詳情、base64 損毀回抓與權限不足處理。獨立複查未發現可重現的資料遺漏或協定保護退步。

## 保留的邊界

UIDVALIDITY、ETag、同來源與集合範圍檢查、權限不足的 unknown、訂房 live recheck、遠端部分成功 receipt 都保留。會議衝突與預約依本次即時查詢判定。

DAV ETag-only／sync-token 增量同步、完整快照的差異寫入，以及第一次大量附件下載，尚未做效能承諾。這些需要另外驗證伺服器契約與資料恢復情境。真實站台的認證、權限及網路效能仍為 NOT RUN。

## 2026-10-09 companion 更新

聯絡人聚合移到資料寫入交易內，schema 2 以 `contact_people` 保存合併內容、`contact_members` 保存來源關係；畫面透過 SQL count／排序／LIMIT／OFFSET 只解碼選定頁，不再每次解析整份 VCARD。同步及單筆聯絡人寫入仍需重建完整聚合，此次沒有承諾全公司同步寫入延遲。

`tests/directory.rs` 以 2,000 人及跨分支重複資料驗證：第一頁 100 人正常讀取；原始 items 的非查詢 JSON 與未選到的人員 payload 損毀，均不影響該頁；選到損毀 payload 才回報錯誤。中文搜尋與部門篩選仍保留全部 memberships，局部分支更新、刪除、重開與 schema 1 遷移也通過。這是處理範圍與正確性的 fixture 證據，沒有轉換為正式站台 latency 數字。

SQLite 初次開庫與 schema 初始化改為相同路徑的檔案鎖，並發開庫重跑 40 次全部成功。HTTP fixture 的 accepted socket 明確切回 blocking，避免 macOS 繼承 listener 的 nonblocking 狀態而使 response queue 位移；partial booking 情境重跑 20 次全部成功。

會議室 metadata 提供 fetched_at／coverage，`--refresh` 略過兩層快取；8 間房 fixture 驗證快取命中時不發 HTTP，強制更新時重新探索與 REPORT。保留最多四路查詢、403 unknown 與訂房 live recheck。

## 2026-10-09 新郵件 webhook

`mail watch` 每次連線只認證及查 CAPABILITY 一次，重用 IMAP 連線與 IDLE；每輪只查所選資料夾的下一個 UID 起的範圍，每批抓取最多 100 封新增正文。首次基準與 UIDVALIDITY 變更只保存水位，不下載歷史郵件；UIDNEXT 缺失時才用 UID SEARCH ALL 建立基準。一般週期不掃描所有資料夾、既有正文或旗標。

協定 fixture 驗證連續 IDLE／DONE／查詢與重啟後的空增量不再 FETCH 已保存郵件；101 封積壓只抓首批 100 封後接續。SQLite schema 2→3 保留聯絡人聚合，不重新解析全公司 VCARD。這些是處理範圍與正確性驗證，尚未量測正式站台的 IDLE 支援、延遲或長時間連線穩定度。

## 2026-10-19 本機全文搜尋 FTS5

schema 4 新增 `items_search`：FTS5 contentless 表（`content=''`、`contentless_delete=1`），tokenizer 用 `trigram`，rowid 對應 `items.rowid`。bundled SQLite 為 3.46.0，trigram（3.34+）與 contentless delete（3.43+）都可用。

- **為何 trigram**：預設 unicode61 以空白與標點斷詞，連續中文會被當成一個長詞，「預算審核」搜不到「第四季預算審核會議」。trigram 把文字切成每 3 個字元一組，查詢 3 字元以上即可做任意位置的子字串比對，中英混合也不需斷詞字典。
- **1–2 字元**：trigram 無法產生 token，這類查詢退回原本逐筆掃描的 `m2k_contains`（Rust `to_lowercase` 子字串），不是 SQLite `LIKE`，因為 `LIKE` 只對 ASCII 不分大小寫，會讓中英結果和以前不同。
- **結果一致**：索引只負責挑候選，候選仍經過原本的逐欄精確判斷，因此欄位範圍（主旨、寄件人、正文、附件檔名、地址、電話；行事曆與聯絡人另含 ICS／VCF 原文；郵件 base64 原文仍不搜尋）、排序與分頁都和掃描版相同。查詢包成單一 FTS5 字串（雙引號、內含引號加倍），`AND`、`OR`、`NEAR`、`*`、`^`、`:`、括號都當一般文字。
- **同步**：`items` 的 INSERT／DELETE／UPDATE trigger 維護索引，prune、刪除、快照取代都經由 DELETE trigger 清索引。UPDATE trigger 只在搜尋欄位變動時重寫，每次同步更新的 `last_seen`、`etag`、旗標不會觸發。trigger 只用內建 SQL 與 JSON 函式，sqlite3 CLI 或 benchmark 腳本直接寫 `items` 也會同步。
- **快照差異式寫入**：郵件同步走 `apply_snapshot`、行事曆走 `apply_collection_snapshot`。原本兩者都先刪掉整個範圍再逐筆 INSERT，每次同步會讓全部郵件經過 DELETE 與 INSERT trigger、重新切 trigram，上面的 `WHEN` 完全用不到。現在只刪除快照裡已經不存在的 id，其餘在原 rowid 上 upsert，所以沒變的郵件不會重寫索引。最終資料和原本相同：範圍內的項目與快照一致、`last_seen` 都更新成這次的時間、集合照舊取代；如果 id 已被範圍外的資料使用（別的類型或別的集合），一樣拒絕整筆快照，只是錯誤訊息改成指出是哪個 id。
- **升級**：舊庫第一次以寫入模式開啟時，在既有 migration 交易裡建立索引並一次回填；失敗或中斷（例如 Ctrl-C）就整筆 rollback，留在 schema 3，下次開啟會重新回填。開始回填前在 stderr 印出「正在建立搜尋索引（第一次升級，可能需要數十秒）…」；TUI 在進入全螢幕前開庫，所以 TUI 使用者也會在終端看到這行。commit 後執行 `PRAGMA wal_checkpoint(TRUNCATE)`，把回填寫進 WAL 的內容併回主檔。回填期間持有初始化檔案鎖與寫入交易，其他 m2k 程序等 5 秒後會回報「SQLite 初始化正在由其他程序執行」，稍後重試即可。
- **升級時間，以每 MiB 可搜尋正文計算**（可搜尋正文指主旨、寄件人、正文等會進索引的文字）：

  | 資料庫 | 可搜尋正文 | 升級時間 | user／sys CPU | 每 MiB（wall／CPU） |
  | --- | ---: | ---: | ---: | ---: |
  | 合成庫（約 7,500 封隨機中文，檔案 62 MB） | 53.6 MiB | 10.6–10.9 s | 9.0 s／1.3 s | 0.20 s／0.19 s |
  | 合成 benchmark fixture（4,000 封） | 2.8 MiB | 0.39–0.46 s | 0.06 s／0.09–0.11 s | — |

  合成庫的 CPU 約每 MiB 0.19 秒，切 trigram 的成本和正文量成正比。資料庫檔案很大時（郵件 base64 原文和正文存在同一列），回填要把整個檔案讀過一遍，wall time 會明顯高於 CPU time。4,000 封 fixture 太小，wall 和 CPU 的差距主要是程序啟動等固定成本，不適合換算每 MiB。
- **限制**：rowid 對應依賴 SQLite 不重編 rowid；m2k 不執行 `VACUUM`，手動 `VACUUM` 可能打亂對應，屆時把 `user_version` 設回 3 再開啟即可重建。SQLite 的大小寫折疊與 Rust `to_lowercase` 在少數字元（例如土耳其文 `İ`）可能不同，這類查詢可能漏掉候選。

### 量測

同上一節的方法與 fixture（4,000 封、每封 65,536 個 base64 字元、warmup 1 次後取 5 次中位數，macOS arm64 release binary）。「修正前」使用修正前 main 的 release binary。原始樣本見 [修正前](benchmarks/fts-before.json)／[修正後](benchmarks/fts-after.json)。`scripts/benchmark_queries.py` 補上 site profile 改版後 `init` 需要的 `--server`，並新增三個搜尋案例。

| 案例 | 查詢 | 修正前時間 | 修正後時間 | 修正前 RSS | 修正後 RSS |
| --- | --- | ---: | ---: | ---: | ---: |
| 全部郵件取 10 筆 | — | 9.57 ms | 10.16 ms | 12.27 MiB | 12.22 MiB |
| INBOX 取 10 筆 | — | 9.91 ms | 10.33 ms | 12.28 MiB | 12.25 MiB |
| 搜尋無符合結果 | `沒有符合的合成關鍵字` | 78.50 ms | 10.98 ms | 13.03 MiB | 11.14 MiB |
| 中文命中 1 封 | `合成效能資料 3999` | 73.37 ms | 11.79 ms | 13.44 MiB | 12.09 MiB |
| ASCII 命中 1 封 | `benchmark-03999` | 68.85 ms | 13.38 ms | 13.38 MiB | 12.53 MiB |
| 2 字元無符合（掃描） | `無關` | 73.46 ms | 74.75 ms | 13.09 MiB | 13.00 MiB |
| TUI 載入與首畫面 | — | 12.43 ms | 13.68 ms | 14.78 MiB | 14.72 MiB |

數字的意思：

- 3 字元以上的搜尋從約 70–80 ms 降到 11–13 ms，已接近不搜尋的清單（約 10 ms，主要是程序啟動）。查詢時間不再隨郵件數線性成長；掃描版要對 4,000 封各欄位做 Rust 小寫轉換與比對。
- RSS 下降 1–2 MiB，因為不再把每列欄位讀進 Rust 函式。
- 2 字元查詢沒有變快，和預期一致：仍是掃描。清單與 TUI 的 ±1 ms 是量測雜訊。
- 空間：索引大小約為可搜尋正文的 0.8–3.2 倍，依內容重複程度而定。benchmark fixture 每封內容幾乎相同，2.8 MiB 正文的索引是 2.2 MiB（0.8 倍，下限）；隨機中文合成庫 53.6 MiB 正文的索引是 173 MiB（3.2 倍，接近上限，因為幾乎每個 trigram 都不一樣）。規劃磁碟空間時建議抓正文的 3 倍。郵件 base64 原文不進索引。
- 這仍是暖快取本機量測，不含網路、IMAP 混合搜尋或第一次同步。

### 同步路徑（差異式寫入前後）

用一次性的 harness（未提交）直接呼叫 `Store::apply_snapshot`：數千封隨機中文郵件、可搜尋正文 53.6 MiB。比較修正前的 main（沒有 FTS）、第一版實作（FTS＋先刪再插）與差異式寫入版，各跑兩輪，表中列出兩輪的範圍。

| 情境 | 修正前（無 FTS） | 第一版實作（先刪再插） | 差異式寫入 |
| --- | ---: | ---: | ---: |
| 空庫第一次寫入數千封 | 0.57 s | 13.1–13.4 s | 12.8 s |
| 同一份快照重新同步（每輪 3 次） | 0.40–0.52 s | 14.0–18.5 s | 0.10–1.03 s |
| 新增 20 封、刪除 5 封 | 0.45 s | 14.9–18.2 s | 0.18–0.85 s |

數字的意思：

- 先刪再插的版本，每次郵件同步都會把整個信箱重新切一次 trigram，在寫鎖內多花 14–18 秒；因此改成差異式寫入。
- 差異式寫入之後，沒有變動的郵件只更新 `last_seen`，不碰索引；重新同步回到 0.1–1 秒，和沒有 FTS 的 `main` 同一個量級。多出的時間主要是每封多一次 id 查詢與 upsert。
- 只有新信或改過的信需要切 trigram，所以日常同步的成本和新信數量成正比，不是和信箱大小成正比。
- 空庫第一次寫入（或第一次完整同步）仍要把全部正文切一次，約 13 秒，和升級回填同一個量級；這是建索引本身的成本，不是同步路徑的問題。
- `tests/search_index.rs` 的 `resyncing_an_unchanged_snapshot_does_not_rewrite_the_index` 走真實的 `apply_snapshot`／`apply_collection_snapshot`，比對 FTS5 segment 的位元組，確認重新同步沒有改寫索引；這個測試在第一版實作上會失敗。
