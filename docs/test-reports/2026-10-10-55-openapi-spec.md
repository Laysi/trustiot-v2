# API 規格由程式碼產生並合併為 api/openapi.json（測試報告）

對應 issue：`Laysi/trustiot-v2#55`。

## 環境

- 本機（Fedora，x86_64），rustc 1.99.0、cargo 1.99.0（`.prototools` 與 `rust-toolchain.toml` 鎖定的版本）。
- 分支 `feat/55-openapi-merge`，基於 `origin/main` 8ce2dfd。
- 沒有服務依賴外部資源；不需要 lab 環境。

## 指令

```sh
export CARGO_BUILD_JOBS=2
cargo run -p openapi                    # 產生 api/openapi.json
cargo run -p openapi -- --check         # 只比對
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
bun scripts/ci/check-workflows.ts       # workflow-policy
```

「故意改一個 API 型別而不重新產生」的實驗：在 `crates/contract/src/error.rs` 的 `ErrorBody` 暫時加一個選填欄位 `trace_id: Option<String>`（`#[serde(default, skip_serializing_if = "Option::is_none")]`），不重新產生，執行 `cargo test -p openapi --test openapi`，再執行 `cargo run -p openapi -- --check`，最後還原（`git checkout -- crates/contract/src/error.rs`）。

## 結果

- 產生：`wrote api/openapi.json (2973 bytes)`；立刻再執行 `--check` 得到 `api/openapi.json is up to date`，`git status` 無變化（輸出穩定）。
- `api/openapi.json` 的 `components.schemas` 含 `ErrorBody`、`ErrorCode`、`FieldError`、`MessageKey`；`ErrorCode.enum` 與 `contract::ErrorCode::ALL` 一致（`spec_contains_the_m1_error_schemas`）。
- fmt、clippy（`-D warnings`）、`cargo test --workspace --locked` 全部通過；新增測試：`tools/openapi` 單元測試 12 項（合併規則、錯誤情況、排序、決定性）、`tests/openapi.rs` 5 項。
- `bun scripts/ci/check-workflows.ts`：`workflow policy: 2 file(s) checked, no violations`。
- 改型別不重新產生的實驗：`committed_spec_is_what_the_code_generates` 失敗，`--check` 以 1 結束，輸出見 [stale-spec-test-output.txt](data/2026-10-10-55-openapi-spec/stale-spec-test-output.txt)：

  ```
  api/openapi.json is out of date with the code; run `cargo run -p openapi` and commit the result.
  first difference at line 25:
    committed:           }
    generated:           },
  ```

  其餘 4 項測試照常通過，且還原後全部恢復通過。
- ci.yml 步驟「OpenAPI spec is current」在同一個實驗下的重播（`cargo run --locked --package openapi`，再看 `git status --porcelain -- api`）：輸出非空，所以步驟會 `exit 1`；它印出的差異見 [ci-step-diff-when-stale.txt](data/2026-10-10-55-openapi-spec/ci-step-diff-when-stale.txt)。為了讓實驗隔離，那次也以 `CONTRACT_UPDATE_FIXTURES=1` 重新產生了 contract 的 fixture，所以只有 OpenAPI 檢查會失敗，其他測試全部通過。
- 沒有開一個故意失敗的暫時 PR：repo 的 pre-commit Codex 審查關卡正確地擋下了這個故意讓規格過期的 commit（兩次，理由都是「規格與程式不一致」），而關卡不得繞過；實驗改在本機以測試與 CI 步驟重播證明。正式 PR 的 CI 結果見 PR 描述。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 執行 CLAUDE.md 記載的指令可重新產生 `api/openapi.json` | 通過：`cargo run -p openapi`（CLAUDE.md「Build, lint, and test」） |
| 故意改一個 API 型別而不重新產生時 CI 失敗 | 通過：本機測試實驗如上；CI 的「OpenAPI spec is current」步驟與 `cargo test --workspace` 都做同一項比對（PR 的 CI 證據見 PR 描述與 issue comment） |
| `api/openapi.json` 含 M1 錯誤 schema | 通過 |

目前沒有任何 HTTP 服務，`SERVICES` 是空的：服務的合併路徑以 `tests/openapi.rs` 的替身服務（utoipa 的 `#[utoipa::path]` handler，錯誤回應參照共用的 `ErrorBody`）驗證。
