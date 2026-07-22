# #9 — проводка метрик в OpenSea‑ран (шаг 1: сборка из результатов)

Первый, **осознанно контейнерный** шаг проводки метрик (#9). Собирает
`RunMetrics` из завершённого OpenSea‑рана и сохраняет JSON рядом с результатами.
**Горячий цикл минта не тронут** — сборка идёт на границе, после рана.

> Этот PR — **черновик для ревью**: он касается пути минта, поэтому мержим
> только после твоего просмотра.

## Background

Модуль `crate::metrics` (уже в `main`) даёт модель `RunMetrics` + коллектор, но
никуда не подключён. `run_opensea_mint` в конце уже собирает `MintRunExport` из
`results: Vec<MintResult>` и пишет JSON/CSV (под флагом `do_export`).

## Что сделано

- `export::write_run_metrics(&RunMetrics) -> PathBuf` — пишет
  `results/metrics_<slug>_<ts>.json`.
- В финализации `run_opensea_mint` (внутри существующего `do_export`) собираем
  `RunMetrics` из `results`: адрес, статус, tx_hash, gas_used, ошибка →
  `error_class` (через таксономию этапа 1). Считаем сводку (confirmed/failed +
  гистограмма причин), `elapsed_ms` берём из **измеренного раном** `elapsed`.

```rust
let collector = MetricsCollector::new("opensea", slug, chain, dry_run, Some(phase));
for w in &results { collector.upsert_wallet(WalletMetrics::new(addr)…with_error(err)); }
let mut m = collector.finish();
m.summary.elapsed_ms = elapsed;               // authoritative
export::write_run_metrics(&m)?;               // results/metrics_*.json
```

## Границы (осознанно вне этого шага)

- **Пофазные спаны** (`auth_ms`/`nonce_ms`/`first_send_ack_ms`/…) и **точный
  t0** — требуют протаскивания коллектора через сам цикл минта. Это следующий
  шаг; здесь мы намеренно не трогаем горячий цикл.
- **Raw‑путь** и **History UI** — отдельные шаги.

Таким образом сейчас в JSON есть: kind/slug/chain/dry_run/phase, список кошельков
с исходами и `error_class`, сводка (total/confirmed/failed + `fail_reasons`),
`elapsed_ms`. Спаны пока пустые.

## Verification

`cargo test -p minter-core` → **169 passed** (логика сборки/классификации уже
покрыта тестами `metrics::`/`errors::`). `cargo check --workspace` — ядро и
десктоп собираются. Пишется только при включённом экспорте (как и результаты),
т.е. поведение по умолчанию не меняется.

Ручная проверка: включить export, сделать (dry) ран OpenSea → появится
`results/metrics_<slug>_<ts>.json` с исходами и гистограммой.

## Suggested people to talk to

- **Andark** (`oderandrej56@gmail.com`) — автор `mint.rs`; согласовать точки
  `mark_span` в цикле для следующего шага (пофазные метрики) и формат для
  History UI.

## Quiz

<details>
<summary>1. Почему спаны сейчас пустые?</summary>

- **A. (верно)** Метрики собираются пост‑фактум из результатов на границе рана;
  пофазные спаны требуют протаскивания коллектора через горячий цикл — это
  следующий шаг, чтобы сейчас не трогать критичный путь.
- **B.** Спаны не нужны.
- **C.** serde их выкидывает.
</details>

<details>
<summary>2. Откуда берётся <code>error_class</code> в метриках кошелька?</summary>

- **A. (верно)** Из `metrics::error_class`, который опирается на общую таксономию
  ошибок этапа 1 (`crate::errors`).
- **B.** Из HTTP‑кода.
- **C.** Всегда "retryable".
</details>

<details>
<summary>3. Меняется ли поведение по умолчанию?</summary>

- **A. (верно)** Нет: файл метрик пишется только при включённом export (как и
  результаты); горячий цикл не тронут.
- **B.** Да, всегда пишет файл.
- **C.** Да, замедляет минт.
</details>
