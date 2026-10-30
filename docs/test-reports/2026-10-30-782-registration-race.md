# certs：同時到達的出廠登錄排隊（2026-10-30）

Laysi/trustiot-v2#782：同一台閘道器的兩筆相同出廠登錄同時到達時，後到的一筆可能回 409 `gateway_registration.public_key_in_use`，而不是冪等的 200。修正是在 `register()` 以硬體 ID 為鍵、取到交易結束的 advisory lock（`registrations::LOCK_REGISTRATION`），規則見 [services/certs/README.md](../../services/certs/README.md) 的「規則與取捨」。這份報告是驗收條件的證據。所有測試都以 `CI=1` 實際執行，需要容器的測試不會略過。

## 環境

- **程式碼**：分支 `fix/782-registration-race`，基於 main `fff5500e`。功能 commit 是 `708ff9c7`，本報告的 commit 只加文件。
- **機器**：建置主機（arm64 Linux VM），經檢查佇列執行。
  - 容器測試由 podman 起 `testkit::product` 的拋棄式 TLS Postgres，以 `certs_app` 與 `certs_platform` 啟動真的 `certs` 執行檔。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust。
- **設定**：`CI=1`。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=1 cargo test --locked -p certs --test registration -- --nocapture
for i in $(seq 1 10); do CI=1 cargo test --locked -p certs || echo "run $i failed"; done
CI=1 cargo test --locked --workspace --exclude agent --no-fail-fast
```

反向對照：暫時刪掉 `register()` 裡執行 `LOCK_REGISTRATION` 的那一句，只跑一次 `registration_is_idempotent_and_needs_a_workstation_and_an_administrator`，跑完以 `git restore` 復原，沒有 commit。

## 結果

| 指令 | 在哪裡、哪個 commit | 結果 |
| --- | --- | --- |
| 「指令」一節全部 | 建置主機，`708ff9c7` | 全部通過（[final-checks.txt](data/2026-10-30-782-registration-race/final-checks.txt)）：<br>- fmt、clippy 通過<br>- `tests/registration.rs` 5 個測試通過<br>- `cargo test -p certs` 連續 10 次都通過，每次 19 個測試執行檔、93 個測試、0 失敗<br>- workspace 250 個測試執行檔、1991 個測試、0 失敗 |
| 反向對照（沒有鎖） | 建置主機，`708ff9c7` 減去取鎖那一句 | 如預期失敗：`both registrations of gw-0004 waiting for its registration lock: not within 120s`（[negative-control.txt](data/2026-10-30-782-registration-race/negative-control.txt)） |

### 證據行（`--nocapture`，`708ff9c7`）

- **同一個硬體 ID**：`two at once, held together before the lookup: [200, 201], the same certificate, one certificate and one event`。
  - 測試以 superuser 拿住 gw-0004 的登錄鎖，等到 `pg_locks` 顯示兩個請求都在等這把鎖才放開。這時兩者都已讀完工作站的簽發紀錄，也都還沒寫 nonce、還沒以硬體 ID 查詢。
  - 結果一個 201、一個 200，`serial` 與 `certificate` 相同，`gw-*` 的登錄、憑證、事件各只多一筆。
- **不同硬體 ID、同一把公鑰**：`["dup-0001", "dup-0002"] at once, held together before the insert: 201 and 409 gateway_registration.public_key_in_use; one registration, certificate and event`。
  - 測試以 superuser 拿住登錄表的 SHARE 鎖，等到兩個請求都持有各自的登錄鎖、並停在寫入登錄表之前，才放開。
  - 後寫入的一個撞到唯一鍵（23505），交易整個回滾，重試後回 409。
  - 它簽過的憑證沒有留下。
- **不同硬體 ID、同一個 EK**：`["dup-0003", "dup-0004"] at once, ...: 201 and 409 gateway_registration.endorsement_key_in_use`，同上。
- **反向對照**：沒有鎖時，兩個請求不會停在查詢之前，測試在等待條件上失敗，失敗訊息寫出它在等什麼。
  - 這證明測試釘住的是鎖造成的排隊。
  - 這個測試不重現原本的 409。原本的 409 需要一筆交易停在兩個查詢之間，從資料庫外面做不到穩定重現；要做到就得在產品程式碼加測試注入點，這次沒有加。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 同一筆登錄以穩定的交錯同時送兩次，一筆 201、一筆 200，兩筆是同一張憑證（測試） | 通過。交錯由鎖的等待者計數決定，不靠時間。 |
| 另一台閘道器真的用了同一把公鑰或 EK 時，仍回 409（既有測試維持通過） | 通過。既有的依序案例維持通過。另外新增同時到達、在唯一鍵衝突的案例：公鑰回 409 `public_key_in_use`，EK 回 409 `endorsement_key_in_use`。 |
| `CI=1 cargo test -p certs` 連續跑 10 次都通過 | 通過（10/10，每次 93 個測試） |
| `cargo clippy --workspace --all-targets -- -D warnings` 通過 | 通過 |

### 待測

以下是延後的進階驗證，不在這個 issue 的範圍：

- 在叢集上以多個 certs 副本實測同時登錄。
- 滾動更新期間，新舊副本混合時的行為。README 已記載這段期間仍可能出現原本的 409。
