# Этап 1: единая таксономия RPC/tx‑ошибок

Пояснительная записка к первому этапу плана `docs/CORE_HARDENING_PLAN.md`.
Цель — собрать разрозненную классификацию ошибок минта в один модуль
(`crate::errors`), расширить покрытие формулировок провайдеров и закрыть
табличными тестами. Поведение старого контракта `classify_mint_error`
сохранено 1:1.

---

## Background

### Совсем с нуля (можно пропустить)

Когда мы отправляем транзакцию минта, нода может ответить ошибкой. Реакция на
ошибку критична: на одни надо **не** повторять (кошелёк пуст, контракт отклонил
безвозвратно), на другие — **повторить**, обновив нонс или подняв комиссию (RBF —
replace‑by‑fee). Беда в том, что разные клиенты (Geth, Erigon, Nethermind) и
RPC‑провайдеры (Alchemy, publicnode) описывают одно и то же **разными словами**, а
часть возвращает ещё и числовой код JSON‑RPC.

> **RBF (replace‑by‑fee).** Повторная отправка транзакции с тем же нонсом, но
> более высокой комиссией, чтобы «перебить» застрявшую. Нода принимает замену,
> только если комиссия выросла достаточно — иначе отвечает «underpriced».

### Узкий контекст

До этого этапа классификаторы жили в двух файлах и матчились по подстрокам:

```rust
// mint.rs
pub(crate) fn classify_mint_error(msg) -> &'static str { /* funds+contract → fatal */ }
fn is_already_known(err) -> bool { lower.contains("already known") }
fn is_nonce_too_low(err) -> bool { /* … */ }
fn is_underpriced(err) -> bool { /* "underpriced" | "fee too low" */ }
// gas.rs
pub fn is_intrinsic_gas_error(msg) -> bool { /* … */ }
```

Вызовы разбросаны по горячему пути (`mint.rs:1998, 2068, 2283, 2292, 2306, 2383`).
Проблема: если провайдер сформулирует «nonce_too_low» вместо «nonce too low», мы
молча **не** распознаем ситуацию и не обновим нонс.

---

## Intuition

Идея простая: **один источник правды**. Каждый предикат владеет своим списком
фраз; функция `classify` собирает их в enum `TxErrorKind` с фиксированным
приоритетом; а `classify_mint_error` остаётся тем же «fatal/retryable», только
теперь поверх общей таксономии.

Приоритет читается как «безвозвратное побеждает восстановимое»:

```
insufficient funds → fatal contract → already known → nonce too low
→ underpriced → intrinsic gas too low → retryable
```

Игрушечный пример: строка `execution reverted; insufficient funds for gas`
содержит и «reverted» (обычно retryable), и «insufficient funds». Приоритет
ставит фонды выше — итог `InsufficientFunds` (fatal), и мы не жжём газ в пустом
кошельке.

---

## Code

**Новый модуль `errors.rs`** — предикаты + enum + классификаторы:

```rust
pub enum TxErrorKind {
    InsufficientFunds, FatalContract, AlreadyKnown,
    NonceTooLow, Underpriced, IntrinsicGasTooLow, Retryable,
}

pub fn classify(msg: &str) -> TxErrorKind { /* приоритетная цепочка */ }

pub fn classify_mint_error(msg: &str) -> &'static str {
    match classify(msg) {
        TxErrorKind::InsufficientFunds | TxErrorKind::FatalContract => "fatal",
        _ => "retryable",
    }
}
```

Списки фраз — **надмножество** прежних, добавлены только однозначные синонимы:

| Категория | Было | Добавлено (безопасные синонимы) |
|---|---|---|
| already known | `already known` | `alreadyknown`, `known transaction`, `already imported` |
| nonce too low | `nonce too low`, `nonce is too low` | `nonce_too_low`, `oldnonce` |
| underpriced | `underpriced`, `fee too low` | `max fee per gas less than block base fee` |
| insufficient funds | (набор из 7 фраз) | без изменений — набор чувствительный |
| fatal contract | (6 паттернов) | без изменений |

Плюс утилита `json_rpc_code(msg) -> Option<i64>` — вытаскивает код JSON‑RPC для
логов/наблюдаемости (в классификации **не** участвует: код `-32000`
переиспользуется под разные условия).

**Миграция вызывающих (без смены поведения):**

```rust
// mint.rs — тонкие re-export/use вместо локальных тел
pub(crate) use crate::errors::classify_mint_error;
use crate::errors::{is_already_known, is_nonce_too_low, is_underpriced};

// gas.rs — сохранённое имя для disperse.rs
pub use crate::errors::is_intrinsic_gas_too_low as is_intrinsic_gas_error;
```

> **Важно.** Наборы `insufficient funds` и `fatal contract` не тронуты, поэтому
> вывод `classify_mint_error` на любых прежних входах **идентичен**. Расширены
> только `already_known/nonce/underpriced`, что лишь **увеличивает** шанс
> корректного retry/RBF — то есть безопасно.

---

## Verification

- `cargo test -p minter-core` — **132 passed** (было 123, +9 новых в `errors::`).
- Новые тесты табличные: варианты funds/contract/already‑known/nonce/underpriced/
  intrinsic + приоритет (funds важнее revert) + извлечение кода JSON‑RPC.
- Прежние тесты `classify_mint_error` (в `mint.rs`) и `is_intrinsic_gas_error`
  (в `gas.rs`) остаются зелёными через re‑export — доказательство отсутствия
  регрессии контракта.

Ручная проверка: `cargo test -p minter-core errors::` — 9 тестов; полный прогон —
132.

---

## Alternatives

**Матчить по коду JSON‑RPC вместо фраз.**

| Коды (rejected как основной путь) | Фразы (выбрано) |
|---|---|
| Стабильнее, если бы коды были специфичны | Работает и там, где кода нет вовсе |
| — | Код `-32000` неспецифичен (nonce/funds/underpriced — все под ним) |
| — | Совместимо с текущими форматированными строками ошибок |

Компромисс: код **извлекаем** (`json_rpc_code`) для логов, но решение — по фразам.

**Оставить как есть (не рефакторить).**

| За | Против |
|---|---|
| Ноль изменений | Дублирование, дрейф; пропуски формулировок в бою |
| — | Нет тестов на nonce/underpriced/already‑known |

---

## Suggested people to talk to

- **Andark** (`oderandrej56@gmail.com`) — автор горячего пути минта (`mint.rs`) и
  газовой логики (`gas.rs`); знает, почему `execution reverted` намеренно
  retryable и какие контрактные ошибки считаются безвозвратными.
- **Viktor** (`viktor@zetalabs.ai`) — делал аудит panic/ошибочных путей в
  `mint.rs`/`rpc.rs`; полезен по краевым случаям классификации и по тому, какие
  формулировки провайдеров встречались на практике.

---

## Quiz

<details>
<summary>1. Почему <code>classify_mint_error</code> гарантированно не изменил поведение?</summary>

- **A. (верно)** Наборы `insufficient funds` и `fatal contract` перенесены
  дословно, а `classify_mint_error` смотрит только на них; расширены лишь
  предикаты nonce/underpriced/already‑known, которые на «fatal/retryable» не
  влияют.
- **B.** Потому что добавлены новые фразы во все категории.
- **C.** Потому что функция стала возвращать enum.
</details>

<details>
<summary>2. Что вернёт <code>classify("execution reverted; insufficient funds for gas")</code>?</summary>

- **A.** `Retryable` — есть «reverted».
- **B. (верно)** `InsufficientFunds` — фонды имеют более высокий приоритет, и это
  правильно: не повторяем при пустом балансе.
- **C.** `FatalContract`.
</details>

<details>
<summary>3. Почему код JSON‑RPC не используется для классификации?</summary>

- **A.** Его невозможно распарсить.
- **B. (верно)** Код `-32000` переиспользуется провайдерами под разные условия
  (nonce/underpriced/funds), поэтому он неспецифичен; его извлекаем только для
  логов/наблюдаемости.
- **C.** Потому что alloy его прячет.
</details>

<details>
<summary>4. Зачем в <code>gas.rs</code> оставлен <code>pub use … as is_intrinsic_gas_error</code>?</summary>

- **A. (верно)** Чтобы не ломать существующего вызывающего (`disperse.rs`
  импортирует `crate::gas::is_intrinsic_gas_error`) при переносе реализации в
  `errors`.
- **B.** Для обхода приватности.
- **C.** Это требование alloy.
</details>

<details>
<summary>5. Почему добавление синонима «nonce_too_low» считается безопасным?</summary>

- **A.** Оно ничего не меняет.
- **B. (верно)** Оно лишь **расширяет** распознавание «нонс слишком мал», а
  реакция на это — обновить нонс и повторить; больше корректных повторов, без
  риска ложного «fatal».
- **C.** Потому что провайдеры так больше не пишут.
</details>
