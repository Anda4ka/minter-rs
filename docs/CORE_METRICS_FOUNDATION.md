# Метрики run: модель данных + коллектор (фундамент #9)

Пояснительная записка к первому шагу пункта **#9** плана `IMPROVEMENT_PLAN.md`
(«Observability / latency budget»). Добавлен модуль `crate::metrics`: структуры
метрик забега + потокобезопасный коллектор. **Горячий путь не тронут** — это
только фундамент; инструментирование (OpenSea/raw) и History UI — отдельные шаги.

---

## Background

### Совсем с нуля (можно пропустить)

Во время минта хочется понимать: **где ушли миллисекунды** (auth, nonce, ожидание
открытия, подпись, первый ack, первый confirm) и **почему упали кошельки** (нет
средств? отклонил контракт? шумит RPC?). Сейчас есть только текстовые логи и
общий `elapsed_ms` — по ним не построишь разбивку и не сравнишь забеги.

### Узкий контекст

План #9 предлагает локальную модель метрик (JSON + History UI). Первый и самый
безопасный кусок — **сами структуры данных и сборщик**, не завязанные на горячий
путь: вызывающий создаёт коллектор, по ходу отмечает спаны/исходы кошельков, в
конце получает сериализуемый `RunMetrics`.

---

## Intuition

Разделяем «сбор» и «использование». `MetricsCollector` — тонкая обёртка над
`Mutex<RunMetrics>` + `Instant` (t0). По ходу забега:

```
mark_span("prep_sign", 42)     // подпись заняла 42 мс
set_open_signal("mintbay_view", open_ts, fire_lag_ms=-8)  // выстрелили за 8 мс до T0
upsert_wallet(WalletMetrics::new("0xA").with_error("insufficient funds"))
```

В конце `finish()` считает сводку: total / confirmed / failed + **гистограмма
причин** по классам. Класс ошибки берётся из общей таксономии этапа 1
(`crate::errors`) — синергия: одна логика классификации на весь проект.

Игрушечный пример гистограммы: 4 кошелька → 1 confirmed, 3 failed
(`funds: 2, rpc: 1`).

---

## Code (`crate::metrics`)

**Структуры** (serde, `camelCase` для будущего JSON/History):

```rust
pub struct RunMetrics { run_id, kind, slug, chain, dry_run, open_mode,
                        t0_unix_ms, spans, open_signal, wallets, summary }
pub struct RunSpans { auth_ms, nonce_ms, balance_ms, wait_ms, prefetch_ms,
                      prep_sign_ms, first_send_ack_ms, first_confirm_ms, done_ms } // все Option
pub struct WalletMetrics { address, proxy, status, send_attempts, t_send_ack_ms,
                           t_confirm_ms, tx_hash, gas_used, error, error_class }
pub struct RunMetricsSummary { total, confirmed, failed, elapsed_ms, fail_reasons }
```

**Классификатор класса ошибки** — мост к этапу 1:

```rust
pub fn error_class(msg) -> &'static str // funds | fatal | rpc | cancel | retryable
```

**Коллектор:**

```rust
MetricsCollector::new(kind, slug, chain, dry_run, open_mode)
  .mark_span(name, ms) / .set_open_signal(...) / .upsert_wallet(w)
  .finish() -> RunMetrics   // считает summary + проставляет done_ms
```

Замечания по дизайну: `std::sync::Mutex` (критические секции короткие, `.await`
под локом нет); `upsert_wallet` матчит по адресу (обновление, не дубликаты);
неизвестные имена спанов игнорируются (совместимость вперёд).

> **Почему без проводки в горячий путь.** Это самый рискованный кусок #9
> (mint.rs/raw_sniper.rs). Сначала — проверенный фундамент; проводку сделаем
> отдельным PR, где данные уже есть, включая `SendReport` из этапа 5.

---

## Verification

`cargo test -p minter-core` → **146 passed** (было 141, +5). Тесты:

| Тест | Что проверяет |
|---|---|
| `error_class_mapping` | funds/fatal/retryable/rpc/cancel по строкам |
| `summary_counts_and_histogram` | total/confirmed/failed + гистограмма (`funds:2, rpc:1`) |
| `upsert_replaces_by_address` | один адрес обновляется, а не дублируется; `run_` при пустом slug |
| `spans_and_open_signal_recorded` | спаны, неизвестный спан игнорится, open‑signal |
| `serde_roundtrip_camel_case` | JSON в camelCase (`authMs`, `tSendAckMs`, `t0UnixMs`) + round‑trip |

Модуль ни к чему не подключён → на поведение приложения не влияет.

---

## Alternatives

**Собирать метрики прямо в существующий `MintRunExport`.**

| За | Против (поэтому отдельная модель) |
|---|---|
| Нет нового модуля | Смешение «результата для UI» и «метрик латентности» |
| — | Отдельная модель проще эволюционирует (спаны/гистограммы) |

**`parking_lot::Mutex` вместо `std`.**

| За | Против |
|---|---|
| Чуть быстрее | Лишняя зависимость в core; критсекции и так микроскопические |

---

## Suggested people to talk to

- **Andark** (`oderandrej56@gmail.com`) — автор `mint.rs`/`raw_sniper.rs`/
  `progress.rs`; с ним согласовать точки инструментирования (где ставить
  `mark_span`) и как метрики лягут в History UI.

---

## Quiz

<details>
<summary>1. Почему коллектор не подключён к горячему пути в этом PR?</summary>

- **A. (верно)** Проводка в mint/raw — самый рискованный кусок #9; сначала даём
  проверенный фундамент, инструментирование — отдельным шагом.
- **B.** Потому что метрики не нужны в бою.
- **C.** Так требует serde.
</details>

<details>
<summary>2. Откуда берётся `error_class`?</summary>

- **A. (верно)** Из общей таксономии этапа 1 (`crate::errors::classify`) +
  хинты на `cancel`/транспорт; одна логика классификации на весь проект.
- **B.** Из отдельного набора строк, дублирующего mint.rs.
- **C.** Из кода ответа HTTP.
</details>

<details>
<summary>3. Почему `std::sync::Mutex`, а не async‑мьютекс?</summary>

- **A. (верно)** Критические секции короткие и **не** держат лок через `.await`,
  поэтому обычного мьютекса достаточно и он проще.
- **B.** async‑мьютексов нет в tokio.
- **C.** Чтобы избежать сериализации.
</details>

<details>
<summary>4. Что делает `upsert_wallet` при повторном том же адресе?</summary>

- **A.** Добавляет дубликат.
- **B. (верно)** Обновляет существующую запись (матч по адресу) — в сводке кошелёк
  считается один раз.
- **C.** Игнорирует второй вызов.
</details>

<details>
<summary>5. Зачем `camelCase` в serde?</summary>

- **A.** Требование Rust.
- **B. (верно)** Чтобы JSON естественно читался фронтендом History UI
  (`authMs`, `tSendAckMs`) на следующем шаге #9.
- **C.** Для меньшего размера.
</details>
