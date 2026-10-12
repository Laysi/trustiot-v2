# Laysi/trustiot-v2#290 以欄位區分種類的列舉改成兩段解碼

issue：Laysi/trustiot-v2#290。來源是 PR Laysi/trustiot-v2#281（Laysi/trustiot-v2#269）的後續提議：`Status`、`AckOutcome`、`LiveAction` 由 serde 解碼時會先把整個 map 緩衝成中間值，是解碼器剩下配置倍數最高的地方。

## 環境

- 本機 Linux 7.2.8（Fedora 45，x86_64），8 核與其他 agent 共用；`CARGO_BUILD_JOBS=2`，cargo target 在 worktree 內。
- Rust 1.99.0（`rust-toolchain.toml`）；aarch64 交叉建置用 rustup 的 `aarch64-unknown-linux-gnu` 標準函式庫。
- 分支 `perf/290-two-phase-tag-decode`，從 `main` 開出（merge base `9b13ad8`），2026-10-12 重定基底到 `a7f332b`。
- 沒有新增相依；`Cargo.toml`、`Cargo.lock` 沒有變更，`tests/dependencies.rs` 的允許清單不變。
- 建置主機 `ruisi-wsl` 在 2026-10-11 17:10–17:20 之間連不上（`rcheck.sh` 回 255，最後一次是 `websocket: bad handshake`），所以 2026-10-11 的結果都在本機執行。
- 2026-10-12 分支重定基底到 `main` 的 `a7f332b` 之後，全部檢查在建置主機 `ruisi-wsl`（共用檢查佇列 `rcheck.sh`，`CI=1`，需要容器的 `tests/broker.rs` 也有跑）重跑一次，見「2026-10-12 合併 main 後重跑」。

## 指令

```
cargo fmt --all --check
cargo clippy -p protocol --all-targets --locked -- -D warnings
cargo test -p protocol --locked --lib --test validation --test adversarial --test golden --test compat \
  --test robustness --test limits --test allocation --test split --test fuzz_split --test readme \
  --test dependencies --test fuzz
cargo test -p protocol --locked --doc
RUSTDOCFLAGS="-D warnings" cargo doc -p protocol --no-deps --locked
cargo build --locked --target aarch64-unknown-linux-gnu -p protocol
# 長時間 fuzz（release），含兩段解碼與 serde 單段解碼的比對、各 channel 的配置量探測：
FUZZ_ITERS=50000 FUZZ_SEED=4 cargo test --release --locked -p protocol --test fuzz -- --nocapture --test-threads=1
```

## 結果

### 做法

`codec::decode` 改呼叫 `Message::decode_item`，預設仍是單段解碼。`Status`、`TelemetryAck`、`LiveControl` 改寫成兩段：

1. 第一段只讀標籤：`state`、`outcome.status`、`action.op`。用 derive 出來的探測結構，其他鍵以 `IgnoredAny` 略過，不配置記憶體。
2. 第二段依標籤，以該變體的欄位再解一次整個 payload。`Status` 直接解 `Online` 或 `Offline`；另外兩種用私有的 `AckWire<F>`、`LiveWire<F>`，其中 `F` 是變體欄位的結構。

變體以外的鍵在兩段都由 `IgnoredAny` 略過。`decode_item` 的參數 `sealed::Token` 只有本 crate 能建立，所以其他 crate 不能呼叫它，避開大小、版本與驗證檢查。

公開型別的 `Deserialize` 不變，仍是 serde 自動產生的單段解碼。測試拿它當比對基準：`decode` 改之前，解碼這三種訊息就是這條路徑。

### 不認得的項目不再配置記憶體

測試 `tests/fuzz.rs` 的 `tagged_maps_decode_in_two_passes_without_buffering` 在三個 channel 的每個樣本中加入大型項目後解碼。

探測範圍：

- 樣本：`status` 兩種、`telemetry-ack` 四種加上一個不認得的狀態、`live-control` 三種。
- 項目放的位置：不認得的鍵 `future`，以及每個已知欄位，例如 `devices`、`message`、`session`。
- 項目的形狀：0 的定長與不定長陣列、空陣列的陣列、`[0]` 的陣列、`0: 0` 的 map、`"a"` 的陣列、長文字。
- 大小：從 2 的次方加 8 bytes 一路到 channel 上限，放在標籤之前與之後各試一次。

總共 3,008 個 payload，結果（[`fuzz-release-seed4-900k.log`](data/2026-10-11-290-two-phase-tagged-decode/fuzz-release-seed4-900k.log)）：

- 不認得的項目不論形狀、大小與位置，解出的訊息都和沒有它時相同，配置的 bytes 也與解碼原樣本時完全相同。各樣本的基準值為 `status` 49、51 bytes，`telemetry-ack` 36–77 bytes，`live-control` 36–208 bytes。測試以 `assert_eq!` 檢查。
- 兩段解碼與 serde 單段解碼的配置量對照如下。單段解碼就是 `decode` 改之前的反序列化步驟，表中數字是它對同一組 payload 的配置量。

| channel（上限） | 兩段解碼：1 KiB 以上的最大倍數 | 兩段解碼：配置高峰 | serde 單段解碼：最大倍數 | serde 單段解碼：配置高峰 |
| --- | --- | --- | --- | --- |
| `status`（4 KiB） | 1.36 倍（`reason` 為 1 KB 文字） | 4,488 bytes | 93.5 倍 | 196,992 bytes |
| `telemetry-ack`（4 KiB） | 1.25 倍（`code` 為 1 KB 文字） | 4,390 bytes | 91.0 倍 | 196,900 bytes |
| `live-control`（64 KiB） | 36.39 倍（`devices` 為 16,387 個 `"a"`） | 1,588,517 bytes | 95.8 倍 | 3,146,148 bytes |

測試釘住的上限：

| channel | 倍數上限 | 高峰上限 |
| --- | --- | --- |
| `status` | 1.5 倍 | 4,608 bytes |
| `telemetry-ack` | 1.5 倍 | 4,608 bytes |
| `live-control` | 40 倍 | 1,700,000 bytes |

測試也檢查 serde 單段解碼的倍數至少 16 倍，確保探測真的會讓舊做法緩衝，探測本身有意義。

`live-control` 剩下的配置來自合法內容本身：每個 2 bytes 的設備 ID 在記憶體中是 24 bytes 的 `String` 加 1 byte，`codec::seq` 的陣列又會加倍成長。這和其他 channel 的陣列一樣，與實際解出的內容成正比。

### `LiveControl` 驗證的配置

同一個探測在 `LiveControl::validate` 改之前量到 179.93 倍（[`live-control-validation-before.txt`](data/2026-10-11-290-two-phase-tagged-decode/live-control-validation-before.txt)）。來源不是解碼而是驗證：設備清單有 16,387 個重複的 `"a"`，驗證為每個重複項建一個 issue，還建了同樣大小的 `HashSet`。

現在清單超過 `MAX_LIVE_DEVICES`（64）時只回報一次 `too_many`。訊息仍然是 `Invalid`，第一個 issue 不變。測試為 `tests/validation.rs` 的 `live_rules`：30,000 個重複設備只得到一個 `too_many` issue。

### 結果與錯誤分類

- 固定樣本：`tests/golden/current/` 與 `tests/golden/v1-frozen/` 沒有變更。`golden.rs`、`compat.rs`（含 `frozen_v1_payloads_still_decode` 與 `required_fields_are_required` 的 `missing field` 訊息）、`adversarial.rs` 的 `golden_payloads_survive_a_decode_and_encode_byte_for_byte` 都通過。
- 既有的 fuzz 與對抗測試都通過。`repeated_keys_are_refused_wherever_a_field_could_be_chosen` 涵蓋 `state`、`outcome.status`、`outcome.records`、`action.op`、`action.ttl_s` 的重複；`allocation.rs` 的 `declared_lengths_never_drive_allocation` 也通過。
- 新增的 `tests/adversarial.rs` 的 `tagged_maps_decode_like_every_other_map` 對九種樣本逐一檢查：
  - 標籤移到最後，結果與原樣本相同；
  - 標籤前後加上重複、含 CBOR tag 或極小負整數的不認得項目，照常略過；
  - 變體欄位在標籤之前重複時拒絕；
  - 整數鍵、byte string 鍵、byte string 形式的標籤一律拒絕。

  它也確認其他變體的欄位不論型別、是否重複都照舊略過，並確認 byte string 文字、陣列形式的 outcome、8 bytes 的 `u32` 一律拒絕。
- 差異比對：fuzz 的每個 `status`、`telemetry-ack`、`live-control` 輸入都會再用 serde 單段解碼（加上相同的前置檢查與驗證）解一次，兩者必須接受相同的訊息，或以同一類錯誤拒絕（`malformed` 或 `invalid`）。例外只有 `single_pass_features` 認得的非標準編碼。release fuzz 90 萬個輸入：
  - 0 個違反不變條件、0 個 panic；
  - 有 2,962 個輸入兩者結果不同，全部屬於下列編碼：`array`、`byte string`、`non-text key`、`wide integer`、`item serde cannot buffer`、`invalid UTF-8`，各組合的件數見資料檔。

  這幾種差異都是這三個 map 改採其他 map 的既有規則，記在 README「解碼規則與下游責任」與「v1 接收範圍的變更紀錄」。

  第一次 release 執行找到 `invalid UTF-8`：略過的項目中有不合法的 UTF-8 文字時，舊做法拒絕，新做法略過。把它加入允許清單與 README 後重跑通過。
- 整體配置：release fuzz（種子 4，90 萬個輸入）配置最多是 payload 的 12.7 倍，最壞的是 1 MiB 的設定。Laysi/trustiot-v2#269 的同一設定是 56.2 倍，最壞的是 `LiveControl`。絕對高峰 13.2 MB，同樣來自 1 MiB 的設定，與之前相同。

### 建置

- `cargo build --locked --target aarch64-unknown-linux-gnu -p protocol` 成功（`libprotocol.rlib`）。
- `tests/dependencies.rs` 通過，允許清單沒有變更。
- `cargo fmt --all --check`、clippy（`-D warnings`）、`cargo doc`（`-D warnings`）都沒有警告。

### 2026-10-12 合併 main 後重跑

分支以 `git rebase origin/main` 重定基底到 `a7f332b`（沒有衝突），之後加了一個只改測試與 README 的 commit（`d0e718f`，見下一節）。在建置主機以 `rcheck.sh` 一次跑完：

```
rcheck.sh 290 'cargo fmt --all --check && echo FMT_OK; cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -2; echo CLIPPY_EXIT=${PIPESTATUS[0]}; cargo test --locked -p protocol --no-fail-fast 2>&1 | grep -E "^(test result|     Running|   Doc-tests|test .*(FAILED|panicked)|failures:)" ; echo TEST_EXIT=${PIPESTATUS[0]}; RUSTDOCFLAGS="-D warnings" cargo doc -p protocol --no-deps --locked 2>&1 | tail -1; echo DOC_EXIT=${PIPESTATUS[0]}; cargo build --locked --target aarch64-unknown-linux-gnu -p protocol 2>&1 | tail -1; echo AARCH64_EXIT=${PIPESTATUS[0]}; FUZZ_ITERS=50000 FUZZ_SEED=4 cargo test --release --locked -p protocol --test fuzz -- --nocapture --test-threads=1 2>&1 | grep -v "^   Compiling"; echo FUZZ_EXIT=${PIPESTATUS[0]}'
```

結果（[`2026-10-12-merged-rcheck.log`](data/2026-10-11-290-two-phase-tagged-decode/2026-10-12-merged-rcheck.log)）：

- `cargo fmt --all --check`、整個 workspace 的 clippy（`-D warnings`）、`cargo doc`（`-D warnings`）都通過。
- `cargo test -p protocol`：15 個測試執行檔與 doc-tests 全部通過，包含需要 Mosquitto 容器的 `tests/broker.rs`（2 個測試）。
- aarch64 建置成功。
- release fuzz（種子 4，90 萬個輸入）：0 個違反、0 個 panic；配置最多 12.7 倍、絕對高峰 13,238,266 bytes；兩段與單段解碼的差異件數與 2026-10-11 完全相同（共 2,962 件，分類見資料檔），在下一節收緊的規則下全部屬於可解釋的方向。三個 channel 的配置探測數字也與上表相同。

### 差異比對的方向（2026-10-12 審查後收緊）

審查時發現 `run_tagged` 的比對太寬：payload 任何位置只要出現一種非標準編碼，兩段與單段解碼的任何差異都會被放過，甚至包括兩段解碼接受了前置檢查會拒絕的 payload。改成依方向判斷：

- 兩段解碼拒絕的編碼（`non-text key`、`byte string`、`wide integer`、`array`）只能解釋「兩段解碼判為 malformed」；
- 兩段解碼略過的項目（`item serde cannot buffer`、`invalid UTF-8`）只能解釋「單段解碼判為 malformed」；
- 兩者都接受卻解出不同的訊息，或兩段解碼接受了前置檢查拒絕的 payload，一律算違反。

收緊後的 release fuzz 結果如上，沒有新的違反。

### 安全審查（2026-10-12）

`crates/protocol` 解碼來自閘道器的不可信訊息，所以逐項確認兩段解碼保留了既有的規則：

- 大小、版本、結構檢查：`decode` 在呼叫 `decode_item` 之前照舊檢查空 payload、channel 上限與 `v`/`rev`；每次 `codec::from_slice` 都先跑 `check_structure`（宣告長度、巢狀深度、分段字串、尾端位元組）。兩段多做的只是線性的重讀，payload 最大 64 KiB。
- 重複的鍵：標籤在第一段由 derive 的探測結構拒絕重複，變體欄位與外層欄位在第二段拒絕重複；`repeated_keys_are_refused_wherever_a_field_could_be_chosen` 與 `tagged_maps_decode_like_every_other_map` 涵蓋標籤前後的重複。
- 不認得的標籤與欄位：`Status` 不認得的 `state` 照舊拒絕；`AckOutcome`、`LiveAction` 不認得的標籤照舊解成 `Unknown`，由 `validate_outgoing` 擋住送出。未知欄位照舊略過。
- `AckOutcome::Rejected` 的 `message` 長度與 U+0000、`LiveSpec` 的設備數、重複設備、間隔與 TTL 範圍都在 `validate`，`decode` 在兩段解碼之後照舊呼叫；`validate` 只改了 `LiveControl` 的設備清單超過上限時的回報方式（仍是 `Invalid`）。IPv4-mapped 的設備 `host` 與心跳的交叉檢查（Laysi/trustiot-v2#269）屬於 `DesiredConfig` 與 `Heartbeat`，不經過兩段解碼，程式碼沒有變動，`tests/validation.rs` 與 `tests/adversarial.rs` 照常通過。
- 沒有新增 panic（`src/` 的新程式碼沒有 `unwrap`、索引或算術溢位）、沒有無上限的配置：第二段的陣列照舊走 `codec::seq`，不認得的項目以 `IgnoredAny` 略過、不配置。
- `decode_item` 以只有本 crate 能建立的 `sealed::Token` 封住，其他 crate 不能呼叫它繞過檢查；本 repo 目前也沒有其他 crate 使用 `protocol`。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 三個列舉解碼時不再緩衝整個 map，最壞配置倍數有測試釘住 | 通過 | `tagged_maps_decode_in_two_passes_without_buffering`：不認得的項目配置 0 bytes；各 channel 的倍數與高峰釘在上表 |
| 所有 golden 樣本、既有 fuzz 與對抗測試都通過，錯誤分類不變 | 通過，附一項需要確認的偏差 | 樣本與既有測試全數通過。對所有標準編碼，結果與錯誤分類都與 serde 單段解碼相同（差異比對）。下列非標準編碼的接受與否改成與其他 map 一致，本 crate 的編碼器與固定樣本都不會產生：整數或 byte string 鍵、byte string 文字、陣列形式、過寬的整數改為拒絕；略過項目中的 CBOR tag、不合法 UTF-8 等改為略過 |
| `crates/protocol` 的 aarch64 建置與相依允許清單不變 | 通過 | aarch64 建置成功；`Cargo.toml` 與 `Cargo.lock` 未變；`tests/dependencies.rs` 通過 |

2026-10-11 的待測項目（在建置主機以 `CI=1` 重跑 `cargo test -p protocol`，含 `tests/broker.rs`）已在 2026-10-12 完成並通過，見上方「2026-10-12 合併 main 後重跑」。
