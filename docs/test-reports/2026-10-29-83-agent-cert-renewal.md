# 閘道器憑證自動換發用戶端（2026-10-29）

對應 issue：Laysi/trustiot-v2#83（檢查清單 D2）。範圍是 agent 的 `src/renewal.rs`、`src/renewal/https.rs`，以及連線在換上新憑證時的輪替（`src/mqtt/connection.rs`、`src/link.rs`）。

## 環境

- 機器：建置主機（x86_64 Linux）。容器測試（Mosquitto、swtpm）以 podman 執行，並設了 `CI`，所以缺少容器執行環境時測試會失敗，不會略過。
- Rust 1.99.0（`.prototools` 與 `rust-toolchain.toml`），aarch64 以 `.prototools` 鎖定的 zig 0.16.0 與 cargo-zigbuild 0.23.4 交叉建置。
- Mosquitto：`testkit::mqtt::mosquitto` 的映像（broker 的 mTLS listener 與 dynamic security），前面接 testkit 的替代 WebSocket 入口。
- 換發端點：`tests/support/renewal.rs` 的替代端點（HTTPS，使用替代入口的伺服器憑證；challenge 的 nonce、以登記的公鑰驗證簽章、為同一把金鑰簽發新憑證，與憑證服務相同），可以指定下一個回應為 429 或 503 與 `Retry-After`，也可以先扣住回應。
- 金鑰：檔案金鑰（P-256，測試時產生），代替 TPM。agent 以同一個 `SigningKey` 介面簽 challenge 與 TLS；TPM 的簽章由既有的 `tests/tpm_swtpm.rs` 驗證。
- 分支 `feat/83-agent-cert-renewal`，第一輪基於 main `10b43af2`；安全審查後的修正（見「安全審查後的修正」）基於 main `bd36aace`。
- 沒有使用實體硬體、lab 的閘道器與實體 TPM。

## 指令

從 repository 根目錄執行：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=true cargo test --locked -p agent
cargo test --locked -p protocol
CI=true cargo test --locked -p testkit
cargo test --locked -p repo-check
cargo deny --locked check
CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" \
  PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" \
  cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent
bun install --cwd scripts --frozen-lockfile
bun test scripts
bun scripts/ci/check-secret-handling.ts

# 只跑換發相關的測試
CI=true cargo test --locked -p agent --lib
CI=true cargo test --locked -p agent --test link -- renew rotat refus
CI=true cargo test --locked -p agent --test renewal -- --nocapture
```

## 結果

所有指令都通過。原始輸出見 [checks.txt](data/2026-10-29-83-agent-cert-renewal/checks.txt)（完整批次，`exit 0 at 13:40:51`）與 [renewal-tests.txt](data/2026-10-29-83-agent-cert-renewal/renewal-tests.txt)（換發相關的測試名稱，`exit 0 at 13:43:03`）。

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過，沒有警告 |
| `CI=true cargo test --locked -p agent` | 全部通過：lib 202、main 1、binary 7、broker 4、dependencies 1、desired_config 7、link 56、liveness 4、modbus 系列、polling_recovery 3、renewal 4、systemd_unit 6、tpm_provision 6、tpm_swtpm 1、transport 19 |
| `cargo test --locked -p protocol` | 全部通過 |
| `CI=true cargo test --locked -p testkit` | 全部通過：lib 42 與各整合測試 |
| `cargo test --locked -p repo-check` | 28 通過 |
| `cargo deny --locked check` | advisories、bans、licenses、sources 都 ok |
| `cargo zigbuild ... --target aarch64-unknown-linux-gnu -p agent` | 通過（連結器只有一則 `ignoring deprecated linker optimization setting '1'` 警告） |
| `bun test scripts` | 1704 通過、0 失敗（2 個需要另外開關的 e2e 略過） |
| `bun scripts/ci/check-secret-handling.ts` | 沒有違規 |

`tests/dependencies.rs` 在加入 hyper、hyper-util、http-body-util 之後仍通過：閘道器建置裡沒有公開根憑證庫、OpenSSL、aws-lc 與 TPM 的 C 函式庫，只有 `ring` 編譯原生程式碼。

### 端到端（`tests/renewal.rs`）

| 測試 | 證明的事 |
| --- | --- |
| `a_certificate_the_broker_refuses_is_renewed_over_https_and_the_agent_reconnects_with_it` | broker（行程內，mTLS 只信任目前的簽發 CA）以 `UnknownIssuer` 拒絕閘道器的憑證，等同憑證被撤銷。agent 馬上換發，對端點的請求依序是 challenge（200）與 renew（201），`Host` 都是 `localhost:<port>`（`cloud.url` 的主機與 port）。下一條成功的內層 mTLS 送的是端點簽發的憑證與它的簽發 CA，MQTT 連上；`gateway.pem` 換成同一條鏈，權限維持 0640；心跳用的到期時間是新憑證的（晚於 80 天後） |
| `the_endpoint_s_429_and_503_are_waited_out_and_each_retry_starts_with_a_challenge` | 過期的憑證，端點先回 503（`Retry-After: 2`）再回 429（`Retry-After: 3`）。請求依序是 challenge 503、challenge 429、challenge 200、renew 201：每次重試都從 challenge 開始；兩次等待分別至少 2 秒與 3 秒。換發後 broker 接受新憑證 |
| `a_short_lived_certificate_is_renewed_before_it_expires_without_losing_messages` | Mosquitto 容器。憑證的效期不到 11 分鐘（`remaining_percent = 50`，20 秒後到換發時間），端點先扣住回應，等閘道器穩定 publish 之後才放行，所以第一次連線一定是用舊憑證。換發期間一個發布者不停送 QoS 1 的遙測（序號），收到 `NotConnected` 或 `Busy` 就重送同一個序號，如同離線緩衝。結果：933 則都拿到 PUBACK，沒有任何一則 `ConnectionLost`；雲端帳號收到的序號正好是 0–932 各一次；舊 session 以 `offline`／`shutdown` 結束（沒有發布 will），新 session 發布 `online`；新 session 的心跳帶新憑證的到期時間；`gateway.pem` 的第一張是端點簽發的憑證 |
| `an_expired_certificate_is_renewed_and_the_broker_takes_the_new_one` | Mosquitto 容器。已過期一小時的憑證：換發一次，之後 Mosquitto 接受新憑證並收到 `online`，檔案換成新憑證 |

### 單元與 link 測試

`cargo test -p agent --lib`（以停住的時鐘與腳本化的端點）：

| 測試 | 證明的事 |
| --- | --- |
| `renewal_is_due_with_the_configured_share_of_the_validity_left` | 到期前剩下設定比例時到換發時間 |
| `renewal_waits_for_the_schedule_and_then_for_the_new_certificate_s_schedule` | 第一次在到期時換發；第二次依新憑證的效期與單調時鐘的下限排程 |
| `a_clock_ahead_renews_once_and_not_again_after_a_restart` | 時鐘快 2000 秒時只換一次；重開 agent 後同樣快的時鐘也不會馬上再換，要等約 2010 秒（排程的下限從憑證檔的修改時間算） |
| `failed_attempts_back_off_and_obey_retry_after`、`retries_wait_for_retry_after_and_then_the_backoff` | 固定抽最長延遲時，重試間隔依序為 60、65、120、2040 秒：退避加倍、`Retry-After` 加在退避之上、上限封頂；只看先後與範圍，不靠真實時間 |
| `a_refused_certificate_is_renewed_at_once_and_a_refused_renewal_waits` | broker 明確拒絕未證明的憑證時馬上換發；新憑證被證明可用前也被拒時等退避（30 秒）；不到 60 秒的連線不算證明，下一次被拒等退避的第二階（60 秒）；維持 60 秒的連線證明可用後再被拒，換發時間正好是上一次因拒絕而換發後 15 分鐘；因拒絕而換發時不留 `.prev` |
| `an_inferred_refusal_of_a_certificate_valid_by_this_clock_waits_for_the_backoff` | 推斷的拒絕（內層 mTLS 一直失敗）而憑證依本機時鐘仍有效時，等退避（30 秒）才換 |
| `a_renewed_certificate_the_broker_refuses_before_it_is_proven_is_rolled_back` | 證明可用的憑證照排程換發時，舊憑證存成 `gateway.pem.prev`（權限 0640）；換上新憑證時已連著的連線不算證明；新憑證被 broker 明確拒絕時不送請求就退回舊憑證，`.prev` 刪除，檔案修改時間回到舊憑證的 notBefore；退回的憑證照排程到期，等退避（30 秒）換發且不留 `.prev`；之後再被拒也不會退回未證明的憑證 |
| `a_connection_already_up_at_the_start_proves_the_certificate`、`an_inferred_refusal_of_a_renewed_certificate_does_not_roll_back` | 換發開始時已經連著的連線（用的就是使用中的憑證）從那時起算，維持 60 秒就證明可用，照排程換掉時會留下 `.prev`；推斷的拒絕不退回，等退避（30 秒）再換發，`.prev` 仍是上一張證明可用的憑證 |
| `only_a_connection_whose_attempt_presented_the_renewed_certificate_proves_it` | 換上新憑證後，嘗試記下舊版本的新連線維持 180 秒也不算證明（`.prev` 還在）；用新版本重連的連線維持 60 秒就證明可用（`.prev` 刪除），之後被明確拒絕時馬上換發、不退回 |
| `a_restart_keeps_the_previous_certificate_until_the_renewed_one_is_proven`、`a_rollback_that_cannot_be_saved_keeps_the_previous_certificate_for_a_restart`、`an_unusable_previous_certificate_is_removed_at_startup` | 重開後 `.prev` 仍可退回；退回時憑證檔寫不進去則保留 `.prev`，重開後讀到被拒的憑證、再被拒時不送請求就退回並寫好；不是憑證、別的閘道器、不比使用中的舊、別的金鑰、別的 CA 的 `.prev` 在啟動時刪除，可用的保留 |
| `the_replay_floor_is_the_start_of_the_last_proven_certificate`、`a_certificate_starting_within_the_clock_skew_is_used` | 防重送的下限是上一張證明可用的憑證：未證明的新憑證不會把下限往上推，證明之後比它早的被拒；notBefore 在 300 秒內的接受 |
| `a_renewed_certificate_that_fails_a_check_changes_nothing` | 十三種被竄改的回應（別的硬體 ID、別的 CN、別的公鑰、不是 `broker.ca` 的根、不受信任的鏈、`certificate` 不是鏈的第一張、notBefore 比本機時鐘晚一小時、已過期、不比上一張證明可用的新、鏈裡夾私鑰、超過 4 張、狀態碼不對、不是 JSON）都被拒，檔案、權限、版本與內層 TLS 設定都不變 |
| `a_challenge_the_agent_did_not_ask_for_is_not_signed` | 別的硬體 ID、別的用途、格式不對的 nonce 的 challenge 不簽 |
| `the_certificate_file_is_replaced_atomically_with_its_permissions`、`a_failed_write_leaves_the_old_certificate_file` | 原子替換沿用舊檔權限、修改時間是指定的時間、不留暫存檔；寫不進去時舊檔不變（`WriteError::NotReplaced`） |
| `a_renewal_swaps_in_the_new_certificate_everywhere` | 成功時檔案、版本、到期時間、身分與內層 TLS 設定都換新 |
| `heartbeats_report_the_expiry_of_the_renewed_certificate` | 心跳的 `cert_not_after` 跟著換發改變 |
| `renewal_settings_are_bounded` | `[renewal]` 的範圍與預設值 |

`cargo test -p agent --test link`（腳本化的 broker）：`renewed_credentials_rotate_the_connection_without_losing_what_it_accepted`（輪替時不接新的 publish，已接受的都送出並拿到 PUBACK，之後 `offline`／`shutdown` 與 DISCONNECT 0x00，再以新的 session 重連）、`a_held_permit_cannot_hold_up_a_rotation_past_its_deadline`（期限到時送 DISCONNECT 0x04）、`a_shutdown_during_a_rotation_stops_the_link`、`a_certificate_alert_is_reported_as_a_refusal_of_the_credentials_in_use`（內層與連線中的 alert 是明確的拒絕；外層 TLS 握手中的 `bad_certificate`、`certificate_revoked` 不是）、`connack_refusals_count_at_once_and_silent_inner_failures_only_after_a_while`（CONNACK 是明確的拒絕；內層無聲的失敗要過 `REFUSED_AFTER` 加隨機時間才算推斷的拒絕，固定抽最大值時至少 `REFUSED_AFTER` + `REFUSED_JITTER` − 1 秒，換上新憑證後重新計算）。

## 安全審查後的修正

針對第一輪 head `0a0ecfb2` 的安全審查（沒有 Critical 或 High）之後，修正 M1、M2、L1、L2 與兩項 Info，並重跑上面的檢查（`bun install` 與 `bun test scripts` 除外：這一輪沒有改 `scripts/` 與相依）。全部通過；原始輸出見 [checks-security-review.txt](data/2026-10-29-83-agent-cert-renewal/checks-security-review.txt)（`exit 0 at 16:04:37`）與 [renewal-tests-security-review.txt](data/2026-10-29-83-agent-cert-renewal/renewal-tests-security-review.txt)（換發相關的測試名稱，同一次執行，`exit 0 at 16:04:37`）。數量改變的只有 agent 的 lib 212（原 202）與 `tests/renewal.rs` 5（原 4）。

| 審查項目 | 處理 | 證據 |
| --- | --- | --- |
| M1：外層 TLS 未經驗證的 alert 被當成拒絕，加上退避歸零可以強迫一直換發 | 只有內層 mTLS 握手、連線中的串流、CONNACK 0x86／0x87 算拒絕；外層握手的 alert 不算。用它開的連線維持 60 秒才算證明可用，之後退避才歸零。證明可用的憑證因拒絕而換發至少間隔 15 分鐘 | `a_certificate_alert_in_the_outer_tls_handshake_does_not_renew_the_certificate`：替代伺服器對每個外層 ClientHello 回 fatal `bad_certificate`，五次以上都以這個 alert 失敗，沒有回報拒絕、換發端點沒有收到請求、版本仍是 0。另有 link 的外層 alert 案例與 `a_refused_certificate_...` |
| M2：notBefore 在未來的憑證加上防重送、沒有退回，可以把閘道器鎖在外面 | notBefore 晚於本機時鐘 300 秒以上或已過期的新憑證不用。證明可用的憑證換掉前存成 `.prev`，新憑證證明可用前被拒時退回。防重送的下限改為上一張證明可用的憑證 | 被竄改回應的兩個新案例、`a_certificate_starting_within_the_clock_skew_is_used`、兩個退回的測試、`an_unusable_previous_certificate_...`、`the_replay_floor_...` |
| L1：推斷的拒絕沒有抖動，憑證有效時也馬上換 | `REFUSED_AFTER` 加 0 到 300 秒的隨機時間；推斷的拒絕而憑證依本機時鐘有效時等退避 | link 的 `connack_refusals_...`、`an_inferred_refusal_...` |
| L2：時鐘快又重開時會重複換發 | 排程下限改從憑證檔的修改時間（寫檔時以閘道器的時鐘設定）加效期的 (1−p) 算。沒有照字面用 notBefore，因為 notBefore + (1−p)V 就是原本的到期時間 | `a_clock_ahead_renews_once_and_not_again_after_a_restart` |
| Info：fsync 目錄失敗時訊息誤導；伺服器來的錯誤文字直接進日誌 | `WriteError::NotDurable` 記「檔案已換、斷電仍可能回到舊的」；錯誤文字截斷到 512 字元、控制字元換成空白 | 程式審查；寫檔失敗的測試改為檢查 `WriteError::NotReplaced` |

端到端測試的憑證檔改為把修改時間設成 notBefore（像簽發時放上去的那樣），因為排程的下限現在也看修改時間。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 依門檻自動換發（可設定，有範圍與預設值） | 通過：`renewal.remaining_percent`（10–80，預設 33），單元測試與 `a_short_lived_certificate_...` |
| 憑證過期或被撤銷（MQTT mTLS 失敗）時經 HTTPS 換發 | 通過：`a_certificate_the_broker_refuses_...`（等同撤銷）與 `an_expired_certificate_...` |
| 換發成功後換上新憑證並重連 MQTT，緩衝的資料不遺失 | 通過：`a_short_lived_certificate_...`（933 則，各收到一次，沒有 `ConnectionLost`）與 link 的輪替測試 |
| 被拒、429、503 時指數退避加隨機抖動，遵守 `Retry-After` | 通過：`the_endpoint_s_429_and_503_...` 與單元測試的間隔順序與範圍 |
| 心跳回報的憑證到期時間是換發後的 | 通過：`heartbeats_report_the_expiry_of_the_renewed_certificate` 與 `a_short_lived_certificate_...` |
| 新憑證的 CN、公鑰、憑證鏈與根都檢查，不通過不安裝也不覆寫；原子寫檔、斷電安全 | 通過：`a_renewed_certificate_that_fails_a_check_changes_nothing` 與原子寫檔的測試；斷電由暫存檔加 rename 與 fsync 的設計保證，沒有真的斷電 |
| 安全審查的 M1、M2、L1、L2 | 通過：見「安全審查後的修正」 |
| 在閘道器的實體 TPM 上換發 | 待測：本 issue 不碰 lab 的閘道器與實體 TPM，建議另開 P2 issue 在閘道器上實測 |
