# webpki-roots-empty

取代 crates.io 的 `webpki-roots`（Mozilla 公開根憑證）的空清單，讓服務連 Postgres 時只信任設定的 CA（Laysi/trustiot-v2#272）。

- 原因：sqlx 的 rustls 連線器一律先放 `webpki_roots::TLS_SERVER_ROOTS`，再加上設定的 `sslrootcert`，沒有可以關掉的設定或 feature。根目錄 `Cargo.toml` 以 `[patch.crates-io]` 把 `webpki-roots` 換成這個 crate，信任清單就只剩 `TRUSTIOT_POSTGRES_CA_FILE` 的 CA。信任範圍與用法見 [service-kit 的連線](../service-kit/README.md#連線postgres)。
- 套件名稱必須是 `webpki-roots`，版本必須滿足圖中對它的需求（sqlx-core 0.9 要求 `1`）。
- 替換對整個 workspace 生效：其他 crate 不能依賴 `webpki-roots`，需要公開根憑證的 client 請用系統憑證庫或 `webpki-root-certs`。`cargo test -p service-kit --test postgres_trust` 檢查 `Cargo.lock`：`webpki-roots` 是這個 crate、只有 sqlx 用它、sqlx 沒有其他信任來源。升級 sqlx 後若它要求更新的 `webpki-roots`，patch 會失效（Cargo 只警告），這項測試會失敗。
