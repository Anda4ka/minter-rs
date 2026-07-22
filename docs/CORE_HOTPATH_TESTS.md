# Hot-path юнит-тесты: raw sniper helpers (#10, п.2)

Тест-only добавление (продакшн-код не менялся): safety net для чистых хелперов
раннего пути raw sniper, которые раньше не были покрыты.

## Что покрыто (`raw_sniper.rs`)

- `u256_to_i64_sat` / `u256_to_u8_sat` — насыщающие конверсии U256 → i64/u8
  (используются для времени и «фазовых» слов из произвольных контрактов): малые
  значения, границы (`i64::MAX`, `u8::MAX`), `U256::MAX` → клампы, без паники.
- `parse_sniper_at_time` — `None` → `None`; unix-секунды; **миллисекунды
  нормализуются в секунды**; пустая строка → `None`; мусор → ошибка.
- `build_mint_params` — раскладка параметров mint:
  - `mint(uint256)` + пусто → `[quantity]` (не меньше 1);
  - явные `params` побеждают;
  - zero-arg `claim()` → `[]`;
  - fallback → слово `quantity`.

## Verification

`cargo test -p minter-core` → **169 passed** (было 162, +7). Продакшн-логика не
тронута — это чистая проверка существующего поведения (safety net для правок
энкодера/RPC/метрик из недавних PR).

## Suggested people to talk to

- **Andark** (`oderandrej56@gmail.com`) — автор `raw_sniper.rs`; по ожидаемому
  поведению `build_mint_params` для разных пресетов.

## Quiz

<details>
<summary>1. Что делает <code>u256_to_i64_sat(U256::MAX)</code>?</summary>

- **A. (верно)** Возвращает `i64::MAX` — насыщение, без паники/переполнения.
- **B.** Паникует.
- **C.** 0.
</details>

<details>
<summary>2. Как трактуется <code>parse_sniper_at_time(Some("1700000000000"))</code>?</summary>

- **A. (верно)** Как миллисекунды → нормализуется в секунды `1700000000`.
- **B.** Ошибка (слишком большое).
- **C.** Как есть, в миллисекундах.
</details>

<details>
<summary>3. Что вернёт <code>build_mint_params</code> для <code>mint(uint256)</code> с quantity 0?</summary>

- **A. (верно)** `["1"]` — quantity приподнят до минимума 1.
- **B.** `["0"]`.
- **C.** Пусто.
</details>
