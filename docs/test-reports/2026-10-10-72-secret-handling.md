# 2026-10-10 密鑰與權限規則（M3）驗收

對應 issue：Laysi/trustiot-v2#72。

## 環境

- 本機：Fedora Linux（x86_64），repo 的 git worktree，分支 `feat/72-secrets-policy`，以 main `8ce2dfd` 為基礎，2026-10-11 合併 main `404817d` 後重跑下列全部檢查（下表是合併後的結果）。
- 工具（`.prototools` 釘選，`proto install` 安裝）：bun 1.4.2、helm、kubeconform、actionlint、gitleaks 8.30.1；TypeScript 7.0.2（`bun install --cwd scripts --frozen-lockfile`）。
- 不需要 Kubernetes 與 lab 機器：檢查是靜態的，chart 測試只用 `helm template` 與 kubeconform。

## 指令

```sh
# 密鑰處理檢查（CI 的 workflow-policy job 也跑這一行）
bun scripts/ci/check-secret-handling.ts

# 檢查自己的單元測試，與 scripts/ 其他測試、型別檢查
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun run --cwd scripts test

# chart：全部檢查，或只跑本 issue 加的 values 與 Secret 檢查
bun test deploy/chart
bun test deploy/chart -t "values reference Secrets"

# repo 規則與 workflow 規則
CARGO_BUILD_JOBS=2 cargo test -p repo-check
bun scripts/ci/check-workflows.ts
proto install actionlint && bun scripts/ci/lint-workflows.ts

# 確認測試資料本身不會被 gitleaks 當成祕密
gitleaks dir --no-banner --redact deploy/chart scripts docs .dockerignore .github CLAUDE.md
```

fixture 的檢查：在 session 的暫存目錄建一個假的 repo（根目錄 `.dockerignore` 為 `*`、一個 COPY 私鑰與 `.env`、宣告 `ARG GHCR_TOKEN` 與 `ENV DATABASE_PASSWORD` 的 Containerfile，以及以 `--build-arg` 傳 `NPM_AUTH_TOKEN` 的腳本），再以 `bun scripts/ci/check-secret-handling.ts <fixture 目錄>` 檢查。release workflow（Laysi/trustiot-v2#68，2026-10-10 尚未合併）的檔案以 `git archive origin/ci/68-release-workflow scripts/release .github deploy/images` 取出後同樣檢查。

## 結果

| 檢查 | 結果 |
| --- | --- |
| fixture（[原始輸出](data/2026-10-10-72-secret-handling/fixture-check.txt)） | 結束碼 1，7 項違規：COPY 私鑰、COPY `.env`、`ARG GHCR_TOKEN`、`ENV DATABASE_PASSWORD`、沒有 ignore 檔卻 `COPY .`（列出每一層可能的 context）、`--build-arg NPM_AUTH_TOKEN`、不經 `containerBuildArgs` 的建置指令 |
| 本分支（[原始輸出](data/2026-10-10-72-secret-handling/repository-check.txt)） | 結束碼 0：159 個檔案、1 個 Containerfile（nightly 的 identity double，`scripts/nightly/platform.ts` 改經 `containerBuildArgs` 建置），沒有違規 |
| Laysi/trustiot-v2#68 分支的檔案（`cef0f50`） | `deploy/images/rust-binary.Containerfile` 與 `release.yml` 都沒有違規（`release.yml` 只以 action 的 `with` 傳 `AI_API_KEY`，沒有寫進 `run`）；唯一一項是 `scripts/release/build.ts` 直接寫 `podman build` 的參數，兩個分支都進 main 後要改用 `containerBuildArgs(context, options)`；它用到的選項、label 與 tag 都在白名單內（結束碼 1） |
| `bun run --cwd scripts test` | 14 個檔案、211 個測試：210 通過、1 略過（本地開發環境的 podman e2e，需要 `TRUSTIOT_DEV_E2E=1`），0 失敗；其中 `scripts/ci/check-secret-handling.test.ts` 57 個全過 |
| `bun run --cwd scripts typecheck` | 通過 |
| `bun test deploy/chart` | 182 個測試全過（main 的 174 個，加上本 issue 的 8 個）。機器負載高時，渲染兩種型態全部服務的測試超過預設 5 秒，所以本 issue 的渲染測試設 60 秒逾時 |
| `bun test deploy/chart -t "values reference Secrets"` | 8 個全過：六個 values 檔（`values.yaml`、兩份部署型態、兩個安裝實例範例、`smoke.yaml`）逐一檢查、規則本身抓得到明文（含小寫 hex 金鑰，字詞組成的長名稱不算）、渲染結果 |
| `cargo test -p repo-check` | 通過 |
| `bun scripts/ci/check-workflows.ts` | 3 個檔案，沒有違規（ci.yml 加了一個 step） |
| `bun scripts/ci/lint-workflows.ts` | 2 個 workflow，沒有錯誤 |
| gitleaks（上列路徑） | no leaks found（測試裡的 AWS key 範例在執行時才組出來） |

`check-secret-handling.test.ts` 涵蓋的情況：各種檔名（`Dockerfile`、`Containerfile`、`*.Containerfile`、`Dockerfile.*`）都找得到，ignore 檔不會被當成 Containerfile；續行、續行中的註解、`escape` 指令與 heredoc 的解析（heredoc 只在 `RUN`、`COPY`、`ADD` 與 `ONBUILD` 包著的這三種指令、而且 `<<` 自成一個字時才算，`ENV NOTE=<<EOF` 後面的 `ARG` 照樣檢查），shell 形式的來源依 Dockerfile 的引號規則切字（`"cert folder/server.key"`、`cert\ folder/server.key` 都是一個來源），含 `..` 的來源依建置程式實際讀的路徑判斷（`foo/..`、`foo/../*` 是整個 context，`app/../certs/server.key` 是私鑰）；COPY／ADD 私鑰、keystore、`.env`、kubeconfig、`.ssh/`、`.npmrc`、`.git-credentials`、`credentials.json`、檔名含憑證字詞的檔案（`service-account.json`、`client_secret.json`、`gcp-credentials.json`、`prod.token`、`api_key.txt`，直接複製、萬用字元與目錄複製都算；`tokenizer.json` 不算）、`.git`（含 `--from` 其他階段、`ONBUILD` 與旗標後的 JSON 形式）都被擋，來源用變數（`$FILE`、`${DIR}`）時也擋，以 `.env` 結尾的檔名（`app.env`、`production.env.local`）與 `.envrc` 在 COPY 與 context 掃描裡都算 dotenv 檔，`.env.example`、`app.env.example`、`.crt`、`.pub` 不擋；`ARG`、`ENV`（含舊式寫法）名稱像密鑰時被擋；`ADD` 帶帳密的網址被擋；整個 context 的複製要有以 `*` 開頭的 ignore 檔，`.containerignore`、`.dockerignore` 任一份不是、或以 `!` 點名或以萬用字元（例如 `!certs/*`、`!config/?env`）比對到金鑰類檔案就失敗；repo 裡實際存在的金鑰類檔案經 `COPY .`、目錄或萬用字元來源帶進映像時失敗（Containerfile 所在目錄往上到 repo 根目錄的每一層都當成可能的 context），ignore 檔後面的規則再排除時通過；`.dockerignore` 的比對規則（最後符合者為準、父目錄符合即涵蓋、`**`、字元類別）；根目錄 `.dockerignore` 缺少、不是 `*` 或有 `!` 例外時失敗；Dockerfile 專屬的 ignore 檔與 `--ignorefile` 被擋；`--build-arg`（shell 寫法、指令陣列、`=` 寫法、續行）名稱像密鑰、值讀 `secrets`（不分大小寫）、名稱不是字面值、名稱或值在執行時才組出（字串相加、樣板字串、`$VAR`、`${{ env.* }}`、與引號相黏的 shell 字）、只寫名稱沒有值、`--build-arg-file` 都被擋，只有 `${{ github.sha }}` 等白名單的公開執行資訊可以用（`github.token`、step 輸出被擋），compose 的任何用法（`docker compose up --build`、`podman compose run`、`docker-compose up`，shell、workflow 與指令陣列，執行器也擋）被擋；workflow step 的 `build-args` 也檢查，建置映像的 action（`docker/build-push-action`、`docker/bake-action`、`redhat-actions/buildah-build`、`depot/build-push-action`、kaniko）被擋，`actions/checkout`、`docker/login-action` 不擋；讀 `secrets` context 的運算式（`Secrets.TOKEN`、`toJSON(secrets)`、`SECRETS['TOKEN']`）寫進 `run`、workflow 與 job 層級的 `env`、service 的 `env` 被擋，放在 step 的 `env` 與 `with`、reusable workflow 的 `secrets`、container 與 service 的 `credentials` 不擋；ignore 檔裡的 `.`、`./`、`!.` 不算任何規則（不會被當成「全部排除」）；未加密、只加密一部分、值只是 `ENC[` 開頭、把明文 base64 後套上 SOPS 格式、IV 可讀或長度不對、缺 MAC、age 收件者格式不對、缺包裝的資料金鑰或資料金鑰是明文、age 檔的 X25519 stanza 形狀不對（共享值或包裝的檔案金鑰不是 32 bytes、包裝的金鑰是可讀文字、多一行、非 X25519 的收件者類型、沒有 stanza；grease stanza 可以有）的 Secret（含多文件與 `kind: List`）、`secretGenerator`（`literals`、`files`、`envs`）、宣告 Secret 或 kind 本身是樣板的 Helm 樣板（含 `.tpl`、`kind: Secret` 後面有註解、樣板語法讓 YAML 無法解析、`List` 的項目）都被擋，以名稱引用 Secret 的 `kind: Secret`（Gateway 的 `certificateRefs`，旁邊有 `name` 或 `group`、沒有 `apiVersion`）不擋；含 `format('{0}', ...)` 這類字串內大括號的運算式也完整解析；context 的掃描包含 `node_modules` 等 repo 掃描略過的目錄；不經 `containerBuildArgs` 的建置指令（指令陣列、先檢查別的目錄再建置、shell 腳本與 workflow 直接 `podman build`、`podman image build`、`docker buildx build`、`docker builder build`、`buildah bud`、`nerdctl build`、`docker compose build`（含 `docker-compose`、`podman compose`）、`docker buildx bake`）都被擋，`cargo build` 不算；子指令在執行時才決定的容器指令（`[podman, operation]`、`["docker", ...args]`、`podmanOk([verb, ...])`、Bun shell 的 `` $`podman ${op}` ``、`sh -c`、`execSync`、shell 的 `docker $CMD`（含引號的 `podman "$CMD"`、`` `echo build` ``）、子指令前有全域選項的指令陣列與 shell 指令，含 `\` 續行）被擋，shell 的 `docker --context prod build` 算建置，固定子指令、`...containerBuildArgs(...)`、由輔助函式產生的參數、註解與錯誤訊息裡的文字不擋；podman 執行器少了 `assertCheckedBuild(args)` 時被擋；`assertCheckedBuild` 擋下不是來自 `containerBuildArgs` 的建置參數（包括執行時組出的 `build`、複製後的陣列、`image`／`buildx` 底下的 `build`；只看指令位置，`podman exec <容器> build` 不算；子指令前與 `image`／`buildx` 子指令前的選項被擋），`containerBuildArgs` 回傳的陣列不能再加參數；`containerBuildArgs` 回傳的參數以檢查過的 context 結尾（相對路徑的 context 與 `--file` 都轉成絕對路徑），context 有 `.env` 或私鑰時、選項不在白名單（`--build-context`、`-v`／`--volume`、`--ignorefile`、`--build-arg-file`、`--build-arg`、會寫主機檔案的 `--iidfile`）、選項裡多一個目錄、選項缺值、不在清單上的 label（`TOKEN=...`）、label 或其他選項的值不符合格式（含帳密的網址、任意文字的 title、`--pull=sometimes`、不是兩種固定形式的 tag、含 token 形狀片段的 tag 或 label）時拋出錯誤，`--file` 指向的建置檔（含 context 外的檔案）違反 Containerfile 規則、不是一般檔案（`-`、網址、目錄）、指定兩個以上，或 context 沒有建置檔時也拋出錯誤，release 與本地開發環境用到的選項通過，ignore 檔排除後通過；JSON 形式的 Secret 也檢查；`node_modules`、`target`、巢狀 checkout（例如 `.claude/worktrees/*`）與測試檔不掃描；本地開發環境產生的 Containerfile 與 release workflow 的寫法通過；本 repo 通過。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 密鑰清冊與輪替 runbook 進 repo（位置在本 issue 決定並從 docs/ 連結） | 通過：清冊與規則在 `docs/architecture/secrets.md`，runbook 在 `docs/runbooks/secrets.md`，兩者都列在 `docs/README.md` 的目錄 |
| CI 檢查 Dockerfile 不 COPY 密鑰類檔案、建置不經 build arg 傳密鑰（測試或 lint） | 通過：`scripts/ci/check-secret-handling.ts` 在 ci.yml 的 `workflow-policy` job 執行，涵蓋 repo 內每一個 Dockerfile 與 Containerfile；fixture 以結束碼 1 失敗（見上） |
| chart 只以名稱引用 Secret，values 內沒有明文密鑰 | 通過：`bun test deploy/chart` 的「values reference Secrets by name only and hold no secret material」對兩份 values 與其他 values 檔逐欄檢查，並確認渲染結果沒有 Secret、只以名稱與 key 讀 Secret |
| SaaS 祕密進叢集的方式與理由寫進 docs/architecture/ci-release.md 的 GitOps 段 | 通過：「SaaS 的祕密怎麼進叢集」小節（SOPS 加 age，由 Flux 解密），決策 5 同步註記 |

待人提供（不影響本 issue 的驗收，Laysi/trustiot-v2#70 開始前需要）：SOPS 收件者的人選與他們的 age 公鑰、SaaS 叢集 age 私鑰的離線備份保管人、GHCR 拉取 token 的機器帳號，見 `docs/architecture/ci-release.md`。
