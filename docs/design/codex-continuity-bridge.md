# Codex Continuity Bridge 整合設計

## 決策摘要

以 CC Switch 作為長期產品底座，但不直接搬入現有 Node proxy。現有專案保留為可執行規格與相容性測試來源；其核心演算法、資料契約與失敗語義逐步改寫成 CC Switch 原生 Rust/SQLite 模組。

這樣可以直接沿用 CC Switch 已成熟的桌面 UI、provider 管理、真實串流、request logging、circuit breaker 與一般 failover，同時補上它目前欠缺、但對 Codex 長對話體驗關鍵的能力：

1. durable canonical compaction journal；
2. 官方與第三方 provider 間的 compaction migration；
3. 靜態加密與 OS 保護的 master key；
4. quota-window aware 的自動 fallback／恢復；
5. 重啟、切換 provider 與遷移失敗時不遺失上下文。

## 不直接移植 Node proxy 的原因

- CC Switch 的 proxy 主路徑已在 Rust 實作真實 SSE streaming；現有 `parity-proxy.mjs` 對部分第三方請求會先等待完整回應再合成 SSE，直接包進去會形成第二套 proxy 與生命週期。
- CC Switch 已有 provider router、circuit breaker、request logs 與 SQLite；重複建立設定、路由與日誌層會造成兩份狀態來源。
- Windows-only installer 與 deployment 邏輯不應帶入跨平台 Tauri app；只移植可跨平台的核心行為，Windows DPAPI 則做成平台 key protector。

## 現有程式到 CC Switch 的映射

| 現有來源 | 可重用內容 | CC Switch 落點 |
|---|---|---|
| `../compaction-store.mjs` | canonical snapshot、journal、加密 envelope、版本與校驗語義 | 新增 `src-tauri/src/proxy/compaction/{store,crypto,model}.rs`，資料落 SQLite |
| `../parity-proxy.mjs` | compact request 判定、materialize、migration、fail-closed、quota latch 演算法 | 拆成 `planner.rs`、`migration.rs`、`quota_policy.rs`，由 handlers/forwarder 呼叫 |
| `../provider-adapter.mjs` | provider capability 判定與 Responses/Chat 差異 | 擴充既有 provider adapter／Codex transform，不建立第二套路由器 |
| `../test_remote_compaction.py` | 官方↔第三方切換、resume、故障語義 | 移植為 Rust integration tests，使用 mock upstream |
| `../test_compaction_store.mjs` | journal、損壞資料、加解密測試案例 | 移植為 store/crypto unit tests |
| `../test_provider_adapter.mjs` | provider capability matrix | 移植為 provider transform table tests |
| `../src/CodexBridge.Deployment`、`../src/CodexBridge.Installer` | 安裝與設定經驗 | 不移植；由 Tauri installer、settings migration 與 UI 取代 |

## 目標模組

```text
src-tauri/src/proxy/compaction/
  mod.rs              對 proxy 暴露單一 CompactionService
  model.rs            CanonicalSnapshot、CompactionRef、MigrationRecord
  store.rs            SQLite transaction、版本、保留政策、重啟恢復
  crypto.rs           AES-256-GCM envelope 與 KeyProtector trait
  planner.rs          判斷 transparent/local/migrate/materialize 路徑
  migration.rs        官方↔第三方轉換、驗證、冪等與 fail-closed
  quota_policy.rs     quota window、provider/account latch、恢復時間
```

`CompactionService` 必須是唯一入口，避免 handler、transform 與 router 各自維護 compaction 狀態。服務由 proxy state 持有，透過 `Arc` 分享。

## 接入點

### 1. `/v1/responses/compact`

在 `src-tauri/src/proxy/handlers.rs` 現有 compact handler 中，forward 前先呼叫 planner：

- 上游原生支援 Responses Compact：透明轉送；成功後把官方 compaction ref 與 canonical snapshot 寫入 journal。
- Chat Completions／Anthropic 相容上游：在本地產生 canonical snapshot，回傳 Codex 接受的 `type: "compaction"` item，不再把 compact endpoint 單純改寫成一般 chat response。
- journal commit 必須與回應建立採「先持久化、再回應」；持久化失敗時不得回傳一個日後無法 resume 的 compaction item。

### 2. 一般 `/v1/responses`

在 `transform_codex_chat.rs`、`transform_codex_anthropic.rs` 前統一 materialize input：

- 同 realm 且上游可直接理解原始 compaction item：保持透明。
- 切到不同 realm 或不支援 compaction：從 journal 解密 canonical snapshot，重建可理解的上下文。
- 找不到、驗證失敗或 snapshot 損壞：fail closed，保留原始 request，回傳明確可恢復錯誤；不得靜默刪除 compaction item。

### 3. provider router / failover

一般網路或 HTTP 錯誤仍由現有 `provider_router.rs`、`forwarder.rs` 與 circuit breaker 處理。Quota policy 只補充語義，不取代一般 failover：

- 從 HTTP 429、Responses error、usage headers／body 擷取 quota signal。
- 對 `(app, provider, account, quota_kind)` 建立有到期時間的 latch。
- router 排序前排除仍被 latch 的候選；若所有候選被排除，回傳最早恢復時間與原因。
- 僅在已成功 materialize/migrate 上下文後才切換 provider，避免 fallback 成功但上下文遺失。
- 成功 probe 或 quota window 到期後解除 latch；不把短暫 rate limit 永久當成帳號耗盡。

### 4. UI 與可觀測性

先沿用 proxy request logs，增加結構化欄位／事件：

- compaction mode：`transparent`、`local`、`migrated`、`materialized`；
- source/target provider realm；
- snapshot id（不可記錄明文內容）；
- quota fallback 原因、被跳過的候選與恢復時間；
- migration 驗證結果。

Codex provider 設定最後增加「Context continuity」區塊，顯示 journal 狀態、最近 migration、加密 key 狀態及 quota fallback 開關。第一階段先做 backend 與 logs，不讓 UI 阻塞核心正確性。

## SQLite 資料模型

建議新增 schema migration 與 DAO，而不是另存散落 JSON：

```sql
CREATE TABLE compaction_snapshots (
  snapshot_id       TEXT PRIMARY KEY,
  task_key          TEXT NOT NULL,
  sequence_no       INTEGER NOT NULL,
  source_realm      TEXT NOT NULL,
  source_provider   TEXT,
  envelope_version  INTEGER NOT NULL,
  nonce             BLOB NOT NULL,
  ciphertext        BLOB NOT NULL,
  content_hash      BLOB NOT NULL,
  created_at        TEXT NOT NULL,
  UNIQUE(task_key, sequence_no)
);

CREATE TABLE compaction_refs (
  ref_fingerprint   BLOB PRIMARY KEY,
  snapshot_id       TEXT NOT NULL REFERENCES compaction_snapshots(snapshot_id),
  realm             TEXT NOT NULL,
  provider_id       TEXT,
  created_at        TEXT NOT NULL
);

CREATE TABLE compaction_migrations (
  migration_id      TEXT PRIMARY KEY,
  snapshot_id       TEXT NOT NULL REFERENCES compaction_snapshots(snapshot_id),
  source_realm      TEXT NOT NULL,
  target_realm      TEXT NOT NULL,
  target_ref_hash   BLOB,
  status            TEXT NOT NULL,
  error_code        TEXT,
  created_at        TEXT NOT NULL,
  completed_at      TEXT
);

CREATE TABLE quota_latches (
  app_type          TEXT NOT NULL,
  provider_id       TEXT NOT NULL,
  account_id        TEXT NOT NULL DEFAULT '',
  quota_kind        TEXT NOT NULL,
  blocked_until     TEXT,
  signal_hash       BLOB,
  updated_at        TEXT NOT NULL,
  PRIMARY KEY(app_type, provider_id, account_id, quota_kind)
);
```

不儲存官方 `encrypted_content` 或 API credentials 的可搜尋明文。外部 compaction ref 只存 keyed fingerprint；真正需要重放的值必須包含在加密 envelope 中。

## 加密與金鑰

- 每筆 snapshot 使用 AES-256-GCM、獨立隨機 nonce，AAD 綁定 schema version、snapshot id、task key 與 sequence。
- master key 不放 SQLite、不放 settings JSON、不寫 log。
- `KeyProtector` 平台實作：Windows DPAPI CurrentUser；macOS Keychain；Linux Secret Service。無安全 key store 時預設 fail closed，不能自動降級成明文。
- 啟動時只解封 master key 到記憶體；錯誤訊息不得包含 key、snapshot 明文、Authorization 或完整 provider URL。
- key rotation 採新版本 envelope 漸進重加密，不用一次性破壞性 migration。

## 關鍵不變量

1. Codex 看見 compaction item 前，對應 snapshot 已 durable commit。
2. migration 是冪等的；重送相同 source ref 不會建立多份互相衝突的狀態。
3. provider 切換失敗時，原始上下文與 journal 都保留。
4. 不能驗證的新 compaction ref 不覆蓋最後一份已知良好 snapshot。
5. quota fallback 不能繞過 provider/account 的使用者限制與安全設定。
6. streaming 路徑不因 journal 寫入退化為整包 buffer；只有 compact 結果需要原子持久化。

## 實作階段

### Phase 0 — Characterization

- 固定目前 Node 實作的 input/output fixtures 與錯誤碼。
- 在 CC Switch 建立 mock upstream 測試，先證明目前 transparent compact、Chat rewrite、429 failover 的行為。
- 建立 capability matrix：native Responses Compact、Responses-only、Chat Completions、Anthropic bridge。

### Phase 1 — Store 與 crypto

- 加 schema migration、DAO、`CompactionService` skeleton。
- 完成 AES-GCM、Windows DPAPI 與 restart recovery。
- 測試 tamper、錯 key、DB transaction rollback、並行 sequence 與 retention。

### Phase 2 — 第三方 local compaction

- 在 compact handler 產生 canonical snapshot 與合法 compaction item。
- 一般 responses 路徑 materialize snapshot，保留既有真實 streaming transform。
- 先支援 Chat Completions，再支援 Anthropic bridge。

### Phase 3 — 跨官方／第三方 migration

- 加 realm capability 判定與 ref fingerprint。
- 完成 official → third-party、third-party → official、third-party A → B。
- 加切換途中 crash/restart、重試、損壞 ref 與 rollback 測試。

### Phase 4 — Quota-aware fallback

- 把 quota signal parser 與 latch 接入現有 router。
- 加 multi-account 候選排序、恢復時間、request log 與 UI 狀態。
- 驗證短暫 429、hard quota、所有帳號耗盡與 window 自動恢復。

### Phase 5 — 跨平台與產品化

- 補 macOS/Linux key protector、key rotation、備份/同步排除規則。
- 加繁中／英文 UI、診斷匯出（只含 metadata）與升級/解除安裝策略。

## 驗收門檻

- 官方與至少兩種第三方協定互切後，可在同一 Codex task 繼續且語意上下文存在。
- app 或電腦重啟後仍可 resume；DB 或 envelope 被修改時安全失敗且不覆寫良好資料。
- compact 與一般 generation 的既有 SSE streaming、tool calls、reasoning items 不退化。
- quota fallback 能避開被 latch 的 provider/account，並在恢復時間後自動重新納入。
- log、UI、crash dump 與同步檔案中沒有 snapshot 明文、master key 或 credentials。

## 第一個程式碼 PR 的建議範圍

第一個 PR 只做 Phase 0 + Phase 1：schema、DAO、crypto abstraction、Windows DPAPI、service skeleton 與測試，不改實際 proxy 行為。這能先建立最難回頭的安全與持久化基礎，也讓後續 compact/migration PR 保持可審查。
