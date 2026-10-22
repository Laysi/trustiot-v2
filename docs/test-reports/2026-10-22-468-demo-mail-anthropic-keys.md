# 展示環境：外寄郵件與 Anthropic 金鑰的設定紀錄

對應 issue：Laysi/trustiot-v2#468（父 issue Laysi/trustiot-v2#457）。

紀錄日期為 2026-10-22。本報告不寫金鑰，也不寫寄件網域的名稱。寄件網域是展示主機名稱所在的 zone 本身，依[密鑰與權限](../architecture/secrets.md)「公開 repo 的資訊規則」，文中只稱「寄件網域」。設定步驟見[展示環境 runbook](../runbooks/demo-environment.md)「外寄郵件與 Anthropic 金鑰」。

## 環境

- **Resend**：一個寄件網域，區域 Tokyo（`ap-northeast-1`）。DNS 紀錄以 Cloudflare API 建立，變更清單事先經使用者同意（見 Laysi/trustiot-v2#468 的現狀）。
- **Anthropic**：Claude Console 裡展示專用的工作區。
- **SOPS 檔**：`deploy/flux/clusters/demo/trustiot/trustiot-smtp.sops.yaml` 與 `trustiot-anthropic.sops.yaml`，在 commit `543f6e81` 提交。
  - 產生方式：維運人員把主控台只顯示一次的金鑰經 stdin 交給 `scripts/flux/sops-secret.ts`。
  - 同一個 commit 把兩個檔案列進同目錄的 `kustomization.yaml`。
- **金鑰的檢查**：
  - 在開發者工作站上，於分支 `feat/468-demo-smtp-test-mail` 的 checkout 執行。
  - 解密用的 age 私鑰屬於 `.sops.yaml` 的收件者之一。
  - 工具：`.prototools` 釘選的 Bun `1.4.2` 與 sops `3.13.3`。
  - Anthropic 的檢查另外用官方 TypeScript SDK `@anthropic-ai/sdk` `1.1.0`，裝在 repo 以外的暫存目錄。
- **腳本與 SOPS 檔的檢查**：
  - 在建置主機（x86_64 Linux）上經檢查佇列執行，跑的是 commit `7b93db0e` 的副本（本報告與測試資料以外，之後沒有變更）。
  - 設了 `CI=1`，容器測試在沒有 podman 時會失敗，不會略過。
  - gitleaks 在開發者工作站執行。

## 指令

三個檢查腳本都在本機程序內解密 SOPS 檔，金鑰不經過指令列、環境變數或輸出。腳本只印出 Secret 的名稱、關於值的布林結果與 HTTP 狀態碼。

腳本的內容在本節最後。使用方式：存成 repo 以外的檔案，然後在 repo 根目錄執行：

```sh
bun <暫存目錄>/smtp-check.ts deploy/flux/clusters/demo/trustiot/trustiot-smtp.sops.yaml
bun <暫存目錄>/key-scope-check.ts deploy/flux/clusters/demo/trustiot/trustiot-smtp.sops.yaml

bun add --cwd <暫存目錄> @anthropic-ai/sdk@1.1.0
# 只送到公開的 API，不讀環境裡的 base URL、token 或 profile
env -u ANTHROPIC_API_KEY -u ANTHROPIC_AUTH_TOKEN -u ANTHROPIC_BASE_URL -u ANTHROPIC_PROFILE \
  bun <暫存目錄>/check.ts deploy/flux/clusters/demo/trustiot/trustiot-anthropic.sops.yaml
```

以下兩項不經腳本：

- **寄件網域與金鑰清單**：以維運人員的 Resend 權限讀 Resend API 的 `GET /domains`、`GET /domains/{id}` 與 `GET /api-keys`。展示環境的金鑰讀不到這三個端點，見「結果」。
- **工作區的用量上限與金鑰的設定**：由有權限的人在 Claude Console 讀。用量上限在 Settings → Workspaces → 展示的工作區 → Spend limits，金鑰的設定在 API keys 頁。

腳本與 SOPS 檔的檢查：

```sh
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts/demo
TRUSTIOT_DEMO_MAIL_E2E=1 bun test scripts/demo/test-mail.container.test.ts
bun test deploy/flux
bun scripts/ci/check-secret-handling.ts
bun scripts/ci/check-workflows.ts
bun test scripts
cargo test --locked -p repo-check
gitleaks git --redact --log-opts="origin/main..HEAD" .
git archive HEAD | tar -x -C <空目錄> && gitleaks dir --redact <空目錄>
```

<details>
<summary><code>smtp-check.ts</code>：SMTP Secret 的形狀</summary>

```ts
// Checks the decrypted SMTP Secret's shape without printing any value.
import { YAML } from "bun";

const file = Bun.argv[2];
if (!file) throw new Error("usage: smtp-check.ts <sops file>");
const proc = Bun.spawnSync(["sops", "decrypt", file], { stdout: "pipe", stderr: "pipe" });
if (proc.exitCode !== 0) {
  console.error(`sops decrypt failed with exit ${proc.exitCode}`);
  process.exit(1);
}
const doc = YAML.parse(proc.stdout.toString()) as { stringData?: Record<string, string> };
const data = doc.stringData ?? {};
const password = data.password ?? "";
console.log(`keys: ${Object.keys(data).sort().join(", ")}`);
console.log(`username is "resend": ${data.username === "resend"}`);
console.log(`password has the Resend key prefix: ${password.startsWith("re_")}`);
console.log(`password has no whitespace: ${!/\s/.test(password)}`);
console.log(`password length at least 20: ${password.length >= 20}`);
```

</details>

<details>
<summary><code>key-scope-check.ts</code>：SMTP 金鑰只能寄信</summary>

```ts
// Shows that the SMTP key in the SOPS file is a sending-only key: Resend refuses it on an
// endpoint that needs full access. Prints only the HTTP status and Resend's error name.
import { YAML } from "bun";

const file = Bun.argv[2];
if (!file) throw new Error("usage: key-scope-check.ts <sops file>");
const proc = Bun.spawnSync(["sops", "decrypt", file], { stdout: "pipe", stderr: "pipe" });
if (proc.exitCode !== 0) {
  console.error(`sops decrypt failed with exit ${proc.exitCode}`);
  process.exit(1);
}
const key = (YAML.parse(proc.stdout.toString()) as { stringData: { password: string } }).stringData.password;
for (const path of ["/domains", "/api-keys"]) {
  const res = await fetch(`https://api.resend.com${path}`, { headers: { Authorization: `Bearer ${key}` } });
  let name = "";
  try {
    name = String(((await res.json()) as { name?: unknown }).name ?? "");
  } catch {
    name = "(no JSON body)";
  }
  console.log(`GET ${path}: HTTP ${res.status} ${name}`);
}
```

</details>

<details>
<summary><code>check.ts</code>：Anthropic 金鑰可用，且不是管理用的金鑰</summary>

```ts
// Checks the demo environment's Anthropic key in its SOPS file without printing it: decrypts in
// this process, checks the Secret's shape, and lists one model with the key (no tokens are used).
// Prints only the Secret's names, booleans about the value, and the HTTP status.
import Anthropic from "@anthropic-ai/sdk";
import { YAML } from "bun";

const file = Bun.argv[2];
if (!file) throw new Error("usage: check.ts <sops file>");
const proc = Bun.spawnSync(["sops", "decrypt", file], { stdout: "pipe", stderr: "pipe" });
if (proc.exitCode !== 0) {
  console.error(`sops decrypt failed with exit ${proc.exitCode}`);
  process.exit(1);
}
const doc = YAML.parse(proc.stdout.toString()) as {
  kind?: string;
  metadata?: { name?: string; namespace?: string };
  type?: string;
  stringData?: Record<string, string>;
};
const key = doc.stringData?.["api-key"] ?? "";
console.log(
  `Secret: kind=${doc.kind} name=${doc.metadata?.name} namespace=${doc.metadata?.namespace} type=${doc.type} keys=${Object.keys(doc.stringData ?? {}).join(",")}`,
);
console.log(
  `api-key: non-empty=${key.length > 0} no-surrounding-whitespace=${key === key.trim()}`,
);

// The key goes only to the public API: no base URL, auth token or profile from the environment.
const client = new Anthropic({
  apiKey: key,
  authToken: null,
  baseURL: "https://api.anthropic.com",
  maxRetries: 0,
});
try {
  const page = await client.models.list({ limit: 1 });
  console.log(`GET /v1/models: OK, ${page.data.length} model listed`);
} catch (err) {
  if (err instanceof Anthropic.APIError) {
    const type = (err as { type?: string }).type ?? "";
    console.log(`GET /v1/models: HTTP ${err.status} ${type}`.trimEnd());
  } else {
    console.log(`GET /v1/models: request failed (${(err as Error).name})`);
  }
  process.exit(2);
}

// A key that can manage the organization (an Admin API key) must never go into the cluster: the
// Admin API's management endpoints have to refuse this key. Only statuses and counts are printed,
// never the data (the member list holds email addresses).
console.log(`api-key: starts-with-sk-ant-admin=${key.startsWith("sk-ant-admin")}`);
const probes: [string, () => Promise<{ data: unknown[] }>][] = [
  ["organization.retrieve() (informational)", async () => ({ data: [await client.beta.organization.retrieve()] })],
  ["organization.users.list()", () => client.beta.organization.users.list({ limit: 1 })],
  ["organization.apiKeys.list()", () => client.beta.organization.apiKeys.list({ limit: 1 })],
  ["organization.workspaces.list()", () => client.beta.organization.workspaces.list({ limit: 1 })],
];
let adminAccess = false;
for (const [name, call] of probes) {
  try {
    const page = await call();
    console.log(`${name}: OK, ${page.data.length} item(s)`);
    if (!name.endsWith("(informational)")) adminAccess = true;
  } catch (err) {
    if (err instanceof Anthropic.APIError) {
      const type = (err as { type?: string }).type ?? "";
      console.log(`${name}: HTTP ${err.status} ${type}`.trimEnd());
    } else {
      console.log(`${name}: request failed (${(err as Error).name})`);
      process.exit(4);
    }
  }
}
console.log(`admin endpoints refused: ${!adminAccess}`);
if (adminAccess) process.exit(3);
```

</details>

## 結果

### Resend 的寄件網域

`GET /domains` 只列出一個網域，就是寄件網域：

- 區域 `ap-northeast-1`。
- Sending `enabled`，Receiving `disabled`。
- 開信與點擊追蹤都關閉。
- 整體狀態 `partially_failed`。

`GET /domains/{id}` 列出的各筆紀錄如下，名稱相對於寄件網域：

| 紀錄 | 名稱 | 狀態 |
| --- | --- | --- |
| DKIM（TXT） | `resend._domainkey` | `verified` |
| SPF 的退信 MX | `send` | `verified` |
| SPF 的 TXT | `send` | `verified` |
| Resend Forge 的退信 CNAME | `rsend` | `failed` |

`partially_failed` 只來自 `rsend`：

- 那是 Resend Forge 的紀錄，Forge 只在 `us-east-1` 運作。
- 展示環境刻意不加這筆紀錄，寄信留在 Tokyo 區域（runbook「Resend 的寄件網域與金鑰」第 2 步）。

寄信需要的 DKIM 與 `send` 的 MX、TXT 都已驗證。上層網域原本的 MX、SPF 與 DMARC 都沒有改。

### Resend 的金鑰

- `GET /api-keys` 只列出一把金鑰 `trustiot-demo-smtp`，2026-10-22 建立。
  - 建立時權限選 Sending access，Domain 限定寄件網域，由使用者在主控台設定並確認。
  - `GET /api-keys` 不回傳權限，所以權限由下面的 `key-scope-check.ts` 佐證。
- 同一天第一次建立的金鑰是完整權限，檢查時發現它能讀 `GET /domains`（200）。處理方式：
  - 在 Resend 刪除；
  - 它的 SOPS 檔也刪掉，從沒進過 commit。
- `smtp-check.ts` 確認 SOPS 檔的形狀：

  ```text
  keys: password, username
  username is "resend": true
  password has the Resend key prefix: true
  password has no whitespace: true
  password length at least 20: true
  ```

- `key-scope-check.ts` 確認權限：需要完整權限的兩個端點都拒絕這把金鑰。

  ```text
  GET /domains: HTTP 401 restricted_api_key
  GET /api-keys: HTTP 401 restricted_api_key
  ```

### Anthropic 的工作區與金鑰

`check.ts` 的輸出：

```text
Secret: kind=Secret name=trustiot-anthropic namespace=trustiot type=Opaque keys=api-key
api-key: non-empty=true no-surrounding-whitespace=true
GET /v1/models: OK, 1 model listed
api-key: starts-with-sk-ant-admin=false
organization.retrieve() (informational): OK, 1 item(s)
organization.users.list(): HTTP 401 authentication_error
organization.apiKeys.list(): HTTP 401 authentication_error
organization.workspaces.list(): HTTP 401 authentication_error
admin endpoints refused: true
```

結束碼是 0。

- 金鑰可以呼叫 API。列出模型不耗用 token。
- 這不是管理用的金鑰：Admin API 管理成員、金鑰與工作區的三個端點都回 401。`GET /v1/organizations/me`（`organization.retrieve()`）一般金鑰也能讀，所以只當參考，不代表有管理權限。
- Claude Console 的設定，使用者 2026-10-22 確認：
  - 金鑰限定在展示專用的工作區，Linked account 是 service account，該 service account 的組織角色是 Developer。
  - 工作區的每月用量上限是 20 美元。
  - 沒有設通知門檻，用量接近上限時不會先收到通知。
  - 金鑰沒有設到期日，不會自己失效。輪替依 [secrets runbook](../runbooks/secrets.md)「何時輪替」與[展示環境 runbook](../runbooks/demo-environment.md)「輪替 Resend 與 Anthropic 金鑰」。

### 測試信容器映像的釘選

2026-10-22 在建置主機第一次跑容器測試時，5 項全部失敗：podman 拉不到釘選的 curl 映像，回「manifest unknown」。

- 原因：上游在 2026-10-20 重新推送了 `quay.io/curl/curl:8.23.0`。quay.io 的 tag 歷史顯示，舊的 index 只在 2026-10-14 到 2026-10-20 掛在這個 tag 上；tag 移走後 quay.io 刪掉了它，所以 registry 對它回 404。
- 處理：commit `c2a9eafb` 改釘這個 tag 目前的 index `sha256:11e64e28c5f5…`。
  - Docker Hub 的 `curlimages/curl:8.23.0` 是同一個 digest。
  - index 列有 linux/arm64 v8。
  - 映像裡 `id` 的輸出是 `uid=100(curl_user) gid=101(curl_group)`，與 pod 設定的 UID、GID 相同。
- `scripts/demo/test-mail.ts` 的註解寫了下次再遇到時怎麼改釘。

### 腳本與 SOPS 檔的檢查

在 commit `7b93db0e` 上經檢查佇列執行，結果如下。

| 指令 | 結束碼 | 結果 |
| --- | --- | --- |
| `bun install --cwd scripts --frozen-lockfile` | 0 | 安裝完成 |
| `bun run --cwd scripts typecheck` | 0 | 沒有型別錯誤 |
| `bun test scripts/demo` | 0 | 423 pass、5 skip、0 fail（skip 的是需要容器的測試，下一列單獨跑） |
| `TRUSTIOT_DEMO_MAIL_E2E=1 bun test scripts/demo/test-mail.container.test.ts` | 0 | 5 pass、0 fail |
| `bun test deploy/flux` | 0 | 73 pass、0 fail |
| `bun scripts/ci/check-secret-handling.ts` | 0 | 502 個檔案、6 個容器建置檔，no violations |
| `bun scripts/ci/check-workflows.ts` | 0 | 4 個檔案，no violations |
| `bun test scripts` | 0 | 1583 pass、9 skip、0 fail |
| `cargo test --locked -p repo-check` | 0 | 全部通過 |

### gitleaks

在 commit `7b93db0e` 上，`gitleaks git --redact --log-opts="origin/main..HEAD" .` 與 `git archive HEAD` 解開後的 `gitleaks dir --redact` 都回報 no leaks found。加入本段結果的 commit 在推送前又掃了一次，結果相同。

## 結論

- **驗收條件 1 通過**：寄件網域在 Resend 已驗證，寄信需要的 DKIM 與 `send` 的 MX、TXT 都是 `verified`。SMTP 金鑰只有 Sending access 並限定寄件網域，需要完整權限的端點都拒絕它。設定紀錄就是本報告，不含金鑰與網域名稱（`scripts/demo/public-text.test.ts` 檢查）。
- **驗收條件 2 通過**：Anthropic 金鑰所在的展示專用工作區，每月用量上限是 20 美元。這把金鑰也不是管理用的金鑰。
- **驗收條件 3 通過**：`bun test scripts/demo` 與容器測試 5 項都通過（容器測試在改釘映像之後）。在展示環境實際寄出的實測在 Laysi/trustiot-v2#492。
- **驗收條件 4 通過**：兩把金鑰只以 SOPS 檔進 repo，`bun scripts/ci/check-secret-handling.ts` 通過，gitleaks 掃 branch 與 tree 都沒有發現。
- 與 runbook 建議不同的兩點，由使用者在 2026-10-22 決定：工作區沒有設通知門檻，Anthropic 金鑰也沒有設到期日。兩者都不影響驗收條件：用量上限仍然擋住超支，金鑰依 runbook 輪替。
