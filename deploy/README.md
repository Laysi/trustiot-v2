# deploy

部署設定。說明見[專案目錄](../docs/architecture/repository-layout.md)與 [CI 與發佈](../docs/architecture/ci-release.md)。

- `chart/`：Helm chart；同一份 chart 搭配 `values-saas.yaml` 與 `values-onprem.yaml`，客戶自有的 k8s 或 OpenShift 也用這份 chart。
- `flux/`：SaaS 的 GitOps 設定（Flux）。地端不裝 Flux。
- `host/`：主機安裝：k3s、cloudflared。cloudflared 是 SaaS 的主機元件，不在 chart 內。
