# certs：換發端點每個硬體 ID 的限流改在簽章之後（2026-10-31）

Laysi/trustiot-v2#801：知道閘道器硬體 ID 的人原本能以捏造的請求用光那個硬體 ID 的換發限流，讓真正的閘道器一直收到 429；未經驗證的流量也能寫出全平台事件 `certs.gateway_renewal_refused`。這份報告是兩點決定的證據；決定與取捨在 [services/certs/README.md](../../services/certs/README.md) 的「換發」（「限流與資源上限」與「規則與取捨」）。

## 環境

- **程式碼**：分支 `fix/801-renewal-abuse-limits`，基於 main `1da444a0`。
- **機器**：建置主機，以 `CI=1` 執行。最後的樹（`54abec88`）在建置主機的 arm64 Linux VM 上跑過下面全部的指令；改退還規則之前的樹也在建置主機（x86_64 Linux）上跑過同一組指令，結果相同。需要容器的測試不略過：podman 起 `testkit::product` 的拋棄式 TLS Postgres，以 migration Job 的程式套用整個產品的 schema，再以 `certs_app` 與 `certs_platform` 啟動真的 `certs` 執行檔。閘道器的 TPM 以軟體 P-256 金鑰代替。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust。
- **設定**（`services/certs/tests/renewal.rs` 的 `TIGHT_LIMITS`）：每個硬體 ID 一次 2 個簽章驗過的兌換、之後每小時 1 個；每一步全域一次 12 個、之後每分鐘 1 個。指標以 OTLP/HTTP 送到測試裡的 collector 替身。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo run --locked -p openapi -- --check
CI=1 cargo test --locked --no-fail-fast -p certs -- --nocapture
CI=1 cargo test --locked --no-fail-fast --workspace --exclude agent
```

只重跑這次的證據：`CI=1 cargo test --locked -p certs --test renewal -- --nocapture rate_limits`；限流的單元測試：`cargo test --locked -p certs --lib renewal::`。

## 結果

`rate_limits_answer_429_and_bound_the_challenges` 的證據行（`-- --nocapture`）：

```text
evidence: 8 requests named rl-ok without its key (4 challenges, 2 redemptions signed by another key: 401, 2 made-up nonces: 409); rl-ok then renewed 2 times: 201 each
evidence: rl-ok's own limit: 429, Retry-After 3600; the challenge was spent: 409
evidence: refusal line {"hardware_id":"rl-ok","reason":"gateway_rate_limited","step":"renew"}
evidence: global: 46 challenge requests of new hardware IDs refused with 429; 4 of the flood stored a challenge
evidence: challenge limit used up by the flood; rl-two still renewed: 201
evidence: renew limit: 3 made-up redemptions answered 409, then 17 429
```

- 以 `rl-ok` 為名、沒有它金鑰的 8 個請求（4 個取挑戰、2 個以別的金鑰簽的兌換回 401、2 個捏造的 nonce 回 409），是它自己限流（2 個）的 4 倍；之後真正的 `rl-ok` 仍換發成功 2 次（201，簽發紀錄多 2 列）。改之前，前 2 個捏造的請求就會用完它的額度，換發得到 429。
- 第 3 次簽章驗過、超過它自己的限流：429、`Retry-After: 3600`、`code` 是 `rate_limited`，挑戰已作廢（同一個請求再送是 409），簽發紀錄不變。拒絕的日誌是 INFO、`reason` 是 `gateway_rate_limited`、`step` 是 `renew`；`certs.renewal.refused{reason="gateway_rate_limited"}` 是 1；`rl-ok` 被拒的計數是 `challenge_unknown` 3、`signature_invalid` 2、`gateway_rate_limited` 1。
- 全域限流照舊：取挑戰超過後新硬體 ID 都 429（`Retry-After` 1–60 秒），資料庫只存下限流放行的挑戰，`certs.renewal.refused{reason="rate_limited"}` 等於被拒的次數；洪水之前拿到挑戰的 `rl-two` 仍換發成功；捏造的兌換用完兌換那一道之後 429。
- 其他換發測試（情境、lag 不明、租戶狀態的 lag、broker 權限同步）照舊通過：

```text
evidence: unknown lag: 60 requests of 30 new hardware IDs: 24 answered 503 with an INFO line each, 36 answered 429 without one
evidence: refusal event {"hardware_id":"rn-ok","reasons":{"challenge_expired":1,"challenge_mismatch":2,"challenge_unknown":3,"signature_invalid":1},"refusals":7,"window_end":"2026-10-30T00:00:00Z","window_start":"2026-10-29T00:00:00Z"}
evidence: unknown lag: 503 bindings_stale, Retry-After 30; the challenge was still valid
```

- 單元測試（`src/renewal/limiter.rs`）：100 個取挑戰與 100 個兌換通過全域限流之後，硬體 ID 自己的那一道仍是完整的額度，記憶體裡沒有任何硬體 ID 的項目；硬體 ID 那一道的拒絕不扣全域限流；簽發時服務自己失敗（503）退還的額度可以再用一次，只有一次；`gateway_rate_limited` 計入時間窗、`rate_limited` 不計，兩種 429 的本文相同、都帶 `Retry-After`（`src/renewal.rs`）。
- `cargo test -p certs`：每個測試執行檔都是 `test result: ok`（函式庫 54 個）；整個 workspace（不含 agent）260 組 `test result` 都是 `ok`（含 doc test）、0 組失敗；`fmt`、`clippy`、`openapi --check` 都通過。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 第 1 點有決定：實作了緩解並有測試證明「捏造請求用光額度後真正的閘道器仍能換發」 | 通過：每個硬體 ID 的限流改在簽章驗過之後才檢查、才扣；上面的證據行。取捨、比較過的做法與還剩下的風險見 README「規則與取捨」 |
| 第 2 點有決定，並寫進 `services/certs/README.md`「規則與取捨」 | 通過：維持未經驗證的拒絕計入時間窗，理由、寫入量上限與調整方式寫在「未經驗證的流量也會寫出被拒的事件」 |
| `CI=1 cargo test -p certs` 通過 | 通過 |

告警（`rate_limited` 與 `gateway_rate_limited` 的速率、事件的速率）在 Laysi/trustiot-v2#183；兩個 `reason` 的意義寫在 README「紀錄與指標」。
