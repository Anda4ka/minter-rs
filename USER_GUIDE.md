# MINTER — инструкция оператора

Полный гайд: **что это**, **как запустить**, **что нажимать по порядку**, **что нельзя**.  
Версия приложения: **0.1.0** (внизу слева в сайдбаре будет `v0.1.0`).  
Обновление гайда: **2026-07-25** (public launch: Releases, EN guide, packaging).

Краткий гайд на английском: [`docs/OPERATOR_GUIDE.md`](docs/OPERATOR_GUIDE.md).

**Создатель:** [X @AndarkFomo](https://x.com/AndarkFomo) · [Telegram](https://t.me/grassfoundationn)

**Исходники:** [github.com/Anda4ka/minter-rs](https://github.com/Anda4ka/minter-rs)  
**Релизы (exe):** [Releases](https://github.com/Anda4ka/minter-rs/releases)

---

## 0. Что это за программа

**MINTER** — Windows-приложение (GUI) для mint OpenSea drops: кошельки, прокси, RPC, задачи (фаза + кошельки + Start), проверка whitelist, результаты. Отдельно — Raw mint по контракту.

| Да | Нет |
|----|-----|
| Локально на вашем ПК | Облако / телеметрия |
| Burner-кошельки | Основные кошельки с «жизнью» |
| Ключи в зашифрованном vault | Ключи в чатах / Discord |
| Настройки в `config.json` | Обязательный ручной `.env` (legacy; config — источник правды) |

**Правило №1:** только **burner** (одноразовые / дешёвые кошельки).  
**Правило №2:** live mint тратит реальный ETH / газ.  
**Правило №3:** **Start = LIVE** (не dry-run): по умолчанию модалка — введите **`LIVE`**. Отключить: Settings → снять «Require type LIVE…».  
**Правило №4:** успех mint = **CONFIRMED в блоке**. **SENT** (tx ушла в сеть) — ещё не победа.

**OpenSea Tasks (просто):** slug → **фаза** → **кошельки** → **Start**.  
Бот ждёт open по wall clock и шлёт с **fixed gas** (отдельный «снайпер-режим» не нужен).

Поток LIVE: **Start → prep/auth → wait open → fixed gas send → receipt → CONFIRMED = ok**.

Не аффилирован с OpenSea. Нет гарантии mint. Вы отвечаете за ключи, средства и соблюдение правил площадок / закона.

---

## 1. Что в папке

### Из релиза (zip с GitHub Releases)

| Файл | Зачем |
|------|--------|
| **`minter-desktop.exe`** | Сама программа |
| `USER_GUIDE.md` | Этот файл (RU) |
| `OPERATOR_GUIDE.md` | Краткий EN |
| `LICENSE-*`, `SECURITY.md`, `README.md` | Лицензия и безопасность |

### После первого запуска (создаёт сама, **не шарьте**)

| Файл / папка | Что внутри |
|--------------|------------|
| `keys.vault` | Зашифрованные private keys |
| `config.json` | RPC, gas, beep, export… |
| `tasks.json` | Сохранённые задачи mint |
| `runs_history.json` | История runs (slug, ok/fail, paths) |
| `wallet_meta.json` | Группы A/B/C и proxy map |
| `proxies.txt` | Если сохраняли прокси |
| `auth_cache.bin` | Кэш SIWE OpenSea |
| `results/` | JSON/CSV после mint (если export) |
| `logs/` | Полный лог каждого mint-run |

---

## 2. Запуск

### Из релиза

1. Скачайте zip с [Releases](https://github.com/Anda4ka/minter-rs/releases), проверьте SHA256 при наличии.  
2. Распакуйте, например в `C:\Minter\`.  
3. **Двойной клик** `minter-desktop.exe`.  
4. SmartScreen («неизвестный издатель») → **Подробнее** → **Выполнить в любом случае** (сборка может быть без code signing).  
5. Откроется окно **MINTER** (тёмная тема, слева меню).

### Из исходников

```powershell
git clone https://github.com/Anda4ka/minter-rs
cd minter-rs
cargo run -p minter-desktop --release
```

---

## 3. Первый запуск — по шагам

### Шаг 1. Burner warning + пароль

1. Галочка: понимаете, что это **burner only**.  
2. Пароль vault (запомните — без него ключи не откроются).  
3. **Unlock**.

Пароль **не** уходит в интернет. Он только разблокирует `keys.vault` на диске.

### Шаг 2. Кошельки (Wallets)

1. Слева → **Wallets**.  
2. **Add key** / **Browse…** / **Multi-file…** / drag-and-drop `.txt` (одна строка = один key).  
3. В таблице — адреса `0x…`. Private keys **на экран не** выводятся.

Полезно:

- **A / B / C** — группы.  
- **Proxy** — привязка прокси к адресу (или auto).  
- **Network** (селектор) + **Check balances** — native balance **на выбранной сети** (ethereum, base, polygon, arbitrum, optimism, blast, zora, apechain, **robinhood**, monad).  
  Без выбора сети / с eth-only RPC на L2 балансы будут неверные — сначала Settings + **Ping networks**.  
- **→ New task** — задача из выбранных.

### Шаг 3. Сеть (Settings)

1. **Settings** → **Connection**.  
2. **Alchemy API key** (рекомендуется) *или* custom RPC / eth / base / polygon.  
   - Один ключ → app сам строит **private** URL по сети:  
     `https://{slug}.g.alchemy.com/v2/{KEY}` (base-mainnet, arb-mainnet, robinhood-mainnet, …).  
   - **Public Alchemy** (`…/public`) **не** используется — только свой ключ.  
3. **Save settings**.  
4. **RPCs** → **Ping networks** — таблица: chain / ping / **chainId** / path / URL.  
   - У каждой сети **свой** chainId (base ≈ 8453, robinhood = 4663, eth = 1).  
   - Если везде chainId `1` и eth-mainnet — обновите app (старый баг fallback).  
   - Опция **Via proxy** — пинг через первый proxy из Settings.  
5. Дополнительно: **Probe URL list** / **Deep latency** — точечная проверка.

Без рабочего RPC mint / balances **не** поедут нормально.

### Шаг 4. Прокси (желательно)

1. **Proxies** — по строке:  
   - `host:port:user:pass`  
   - `socks5://host:port`  
   - `http://user:pass@host:port`  
2. **Save proxies**.

Без прокси OpenSea чаще режет IP (auth / eligibility / drop GQL).

### Шаг 5. WL Check (по желанию)

1. **WL Check** → slug или URL коллекции.  
2. Кошельки → **Check WL**.  
3. Смотрите Eligible / нет.

### Шаг 6. Задача mint (Tasks)

1. **Tasks** → **Create task**.  
2. **Slug** — slug или ссылка OpenSea.  
3. **Load phases** — фазы (нужен unlock + сеть).  
4. **Phase** — auto или конкретная.  
5. **Quantity** — mint на кошелёк.  
6. **Gas** — Auto или Manual (+ limit).  
7. **Priority fee (gwei)** — `auto` или число (`1.5`).  
8. **At time** — опционально: unix или ISO (`2024-01-01T15:00:00Z`).  
9. Кошельки + фильтр **A/B/C**; при необходимости **Per-wallet quantity**.  
10. **On Start: only funded wallets** — отсечь пустые (рекомендуется).  
11. **Save task**.

Задачи → `tasks.json`.

### Шаг 7. Start (LIVE)

1. На карточке → **Start**.  
2. По умолчанию (Settings → **Require type LIVE before Tasks Start**) — модалка: введите **`LIVE`** и подтвердите.  
3. Снять галку в Settings — Start без ввода (power-user).

Что происходит:

1. (Опционально) фильтр баланса.  
2. Auth / availability / prep.  
3. **Ожидание open фазы** (wall clock).  
4. Перед open: pre-fetch calldata (~5s), refresh nonces (~2s).  
5. **LIVE: fixed gas** → **sign + send** (без gate `eth_estimateGas` на open; SeaDrop NotActive декодируется в логе).  
6. **Ждём receipt** → **CONFIRMED** = успех.  
7. Таблица + цветной лог + баннер фазы (wait / fire / confirm / done).  
8. Первый on-chain confirm → бейдж (звук — если **Beep** в Settings).

**Stop** → «Stopping…» (best-effort).  
Пока mint идёт — второй Start: toast «уже running» / очередь.

### Mission Control (LIVE OpenSea)

После **Start LIVE** справа внизу — тёмный HUD:

- фаза (prep / wait / fire / confirm / done);  
- счётчики OK / FAIL / SENT / WAIT / TOTAL;  
- таблица кошельков + хвост лога;  
- **▾** — свернуть; **Stop** — как Stop на Tasks.

Страницу Tasks не заменяет: полный log/table там же. Overlay можно не закрывать после run.

---

## 4. Меню слева — коротко

| Пункт | Зачем |
|-------|--------|
| **Home** | Vault / wallets / network, last mint, чеклист |
| **Tasks** | Задачи, Start/Stop, log; **Mission Control** при LIVE |
| **Raw Mint** | Контрактный mint / pre-sign race (multi-wallet) |
| **WL Check** | Whitelist / eligible (export без PUBLIC_SALE as eligible) |
| **Wallets** | Ключи, группы, proxy map, **баланс по сети** |
| **NFTs** | История runs, tx-ссылки, **Open results**, **Open logs** |
| **RPCs** | **Ping networks** + Probe URL + latency |
| **Proxies** | Список + health |
| **Settings** | Connection / Gas / Safety |

Справа сверху:

- **EN/RU**  
- **Dry Run / LIVE** — глобальный чип (для sweep/raw).  
  **Tasks → Start всегда LIVE**, независимо от чипа.

---

## 5. Важные сценарии

### «Только проверить, без tx»

Start задачи (sim → tx → confirm).  
Не жмите Start, если не готовы.  
Проверки без mint: **WL Check**, **Load phases**, balances, **Auth test**.  
Для **raw / sweep** в Advanced есть **Dry run**

### «Один drop — разные наборы»

Wallets → группа **A** / **B** → отдельные tasks с фильтром.

### «Свой priority»

В задаче: **Priority fee (gwei)** = `2`. На карточке: `prio 2`.

### «Старт ровно в X»

**At time** = unix (секунды или мс) или ISO / RFC3339.  
Open фазы: **wall clock** (не lag chain ~12s).  
**Неверный** формат `at time` → **ошибка сразу** (бот **не** ждёт «чужую» phase start молча).

### «Где результаты и логи»

- **NFTs** → история + last run.  
- **Open results folder** → `results/`.  
- **Open logs folder** → `logs/mint_<slug>_<time>.log` (полный trail run).  
- Клик по **tx** → explorer.

### «SENT vs CONFIRMED»

| Статус | Значение |
|--------|----------|
| **SENT** | Tx в mempool / broadcast — **ещё не** success |
| **CONFIRMED** | В блоке, mint засчитан **ok** |
| **FAILED** | Revert / error / preflight fail |

### «Ошибка chain / RPC»

Mismatch → **Settings** → Connection → Save → **RPCs → Ping networks**.  
Убедитесь, что chainId совпадает с сетью (не все `1`).

### «401 / re-auth / 429 OpenSea»

Auth протух или rate-limit. Проверьте **прокси**, не долбите без паузы.  
Drop GQL: retries + короткий cache; availability — ограниченный parallel.  
Прокси **привязан к кошельку** с момента auth (в т.ч. после cache SIWE / re-auth).

### «L2 / Base / Arbitrum / Robinhood — gas too low»

На elevated L2 app поднимает gas limit (floor ~150k) на estimate path.  
Если **fixed gas** слишком низкий — тоже clamp + запись в лог.  
Robinhood (chainId **4663**) — elevated; Flashbots **не** для этой сети.

### «Raw Mint — много кошельков (mint(uint256))»

Страница **Raw Mint** (сайдбар), не Tasks.

**Рекомендуется режим Simple mint** (чип сверху):

1. **Network** — сеть контракта (напр. Robinhood Chain).  
2. **Contract** — `0x…` (можно **proxy**; app сам ходит в implementation).  
3. **NFTs** — сколько mint **с каждого** кошелька (обычно `1`).  
4. **Pay** — ETH **за 1 NFT** (free → `0`; paid → цена; итого value = price × qty).  
5. **Wallets** — отметить много адресов; **Funded only** + **Balances**.  
6. **Gas limit** — по умолчанию `650000` (race path без estimate на T0).  
7. **Fire timestamp** — пусто = сразу; или unix T0 drop (pre-sign ~T−5s).  
8. Advanced → **Dry run** — сначала sim/pre-sign **без** send.  
9. **Start** — multi-wallet race; **Send now** — разовый прогон без wait.

**N кошельков = N отдельных tx** (одинаковый call, разные from/nonce).

**Custom mode** (если Discover / нестандартная сигнатура):

1. **Discover functions** — сканирует bytecode.  
   - EIP-1167 / EIP-1967 proxy → implementation.  
   - Источники: explorer ABI, hardcoded mint, 4byte.  
   - Пример: proxy на Robinhood → `mint(uint256) (hardcoded@proxy)`.  
2. **Function** = `mint(uint256)` (или из списка).  
3. **Params** = `1` (один uint = qty; Custom: Pay = **total** ETH на call).  
4. Дальше как выше: wallets → Dry → Start.

**Flashbots** — только Ethereum mainnet; на Robinhood/L2 **выкл**.

| Симптом | Что сделать |
|---------|-------------|
| Function пустой после Discover | Обновите exe (proxy resolve); или впишите `mint(uint256)` вручную |
| sim revert | price/value, sale closed, max per wallet, wrong phase |
| low balance | gas + Pay на **этой** сети (Balances) |
| Flashbots error | снять галку — не mainnet |

---

## 6. Settings

| Секция | Поля |
|--------|------|
| **Connection** | Alchemy, RPC URLs, eth/base/polygon |
| **Gas & mint** | gas limit, priority, multipliers, retries, export, beep |
| **Safety** | dry run default (глобальный), quiet, LIVE confirm, idle lock |

**Beep on first confirm** — звук только если галка **вкл**.  
**Export results** — JSON/CSV в `results/`.  

**Connection / RPC:** `config.json` — источник правды. Пустые RPC + Save (и **Clear Alchemy** для ключа) реально сбрасывают; старые строки в `.env` больше не «воскресают».  
**LIVE OpenSea** всегда fixed gas — отдельный «sniper preset» / skip-estimate в задаче не нужны.

---

## 7. Файлы и безопасность

```
C:\Minter\
  minter-desktop.exe
  USER_GUIDE.md
  OPERATOR_GUIDE.md      ← из релиза / docs
  config.json            ← после Save (секреты!)
  keys.vault             ← после import (секреты!)
  tasks.json
  wallet_meta.json
  proxies.txt
  runs_history.json
  auth_cache.bin
  results\
  logs\
```

- Пароль vault **не** лежит открытым текстом.  
- **Не шарьте** `keys.vault` / свой `config.json` / `auth_cache.bin` / `results`.  
- Если делитесь программой — только exe + docs из релиза, без секретов.  
- Уязвимости с утечкой ключей — только private report: [`SECURITY.md`](SECURITY.md).

---

## 8. Типичные проблемы

| Симптом | Что сделать |
|---------|-------------|
| Не открывается exe | SmartScreen → Подробнее → Выполнить; антивирус |
| «Vault locked» | Unlock с паролем |
| Нет кошельков | Wallets → import |
| RPC not configured / blocked | Settings → Save → RPCs Probe |
| Start серый (Blocked) | Причина на карточке (slug / wallets / RPC / vault) |
| Mint already running | Stop или ждать |
| PRE-FLIGHT FAIL | Phase не open / нет WL / нет газа / wrong chain |
| Invalid at_time | Проверьте unix / ISO; опечатка больше не «молча» ждёт phase |
| intrinsic gas too low (L2) | Fixed gas ↑ или Auto; app clamp ≥ ~150k на L2 |
| 401 / re-auth | Прокси, Warm auth, не hammer OpenSea |

---

## 9. Контакты с реальностью

1. Программа **не** гарантирует mint.  
2. OpenSea / RPC / прокси / timing — внешние риски.  
3. Gas / revert / OutOfFunds — нормальная часть sniper UX.  
4. Вы отвечаете за ключи и средства.  
5. Не аффилированы с OpenSea.

---

## 10. Для разработчиков (rebuild)

Исходники: https://github.com/Anda4ka/minter-rs

```powershell
# из корня репо — локальная папка Public\ (gitignored)
powershell -ExecutionPolicy Bypass -File scripts\package-public.ps1
# опционально safe zip без секретов:
powershell -ExecutionPolicy Bypass -File scripts\package-public.ps1 -MakeZip
```

Официальные бинарники — через **GitHub Releases** (push tag `v*`, workflow `release.yml`).  
Папка `target\` — кэш сборки, для раздачи не нужна.  
`Public\` в git **не** коммитится.

### Settings (safety)

| Опция | Смысл |
|-------|--------|
| **Require type LIVE before Tasks Start** | default ON — accidental LIVE harder |
| **Idle lock (minutes)** | 0 = off; default 30 — vault lock after idle (not mid-mint) |
| **Raw sniper fee refresh** | mainnetOnly / always / never |

При multi-wallet без прокси Start покажет warning (429 risk).

Контрибьют: [`CONTRIBUTING.md`](CONTRIBUTING.md).

---

**Итог:**  
папка → `minter-desktop.exe` → Unlock → Wallets → Settings/RPC → Task (фаза + кошельки) → **Start** (type **LIVE**) → wait open → fixed gas send → **CONFIRMED** → results / logs.

Исходники: **https://github.com/Anda4ka/minter-rs**

Удачи. Не миньте с main wallet.
