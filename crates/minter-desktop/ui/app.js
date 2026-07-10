import { applyI18n, getLang, setLang, t } from "./i18n.js";

const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);

/** Open http(s) links in system browser (Tauri shell plugin). */
async function openExternalUrl(url) {
  try {
    const { open } = window.__TAURI__.shell || {};
    if (open) {
      await open(url);
      return;
    }
  } catch {
    /* fall through */
  }
  try {
    await invoke("plugin:shell|open", { path: url });
  } catch {
    window.open(url, "_blank", "noopener,noreferrer");
  }
}

// Creator / social links → system browser
document.addEventListener("click", (e) => {
  const a = e.target.closest("a.creator-link");
  if (!a || !a.href) return;
  e.preventDefault();
  openExternalUrl(a.href).catch(console.warn);
});

const ROW_H = 36;
const OVERSCAN = 8;
const TASK_WALLET_ROW_H = 32;

let lastMintSummary = null;
/** @type {object[]} mint run history (newest first) — persisted in runs_history.json */
let mintRunHistory = [];
let runsHistoryLoaded = false;
let runsHistorySaveTimer = null;
let walletSelection = new Set();
let walletData = [];
let modalResolve = null;
let mintRenderScheduled = false;
let lastMintChain = "ethereum";
let mintStopping = false;
let appVersionStr = "0.1.0";

function showToast(msg, kind = "") {
  const host = $("toast-host");
  if (!host) {
    console.log("[toast]", msg);
    return;
  }
  const el = document.createElement("div");
  el.className = "toast" + (kind ? " " + kind : "");
  el.textContent = msg;
  host.appendChild(el);
  setTimeout(() => {
    el.remove();
  }, 4200);
}

/** Shared AudioContext — must be resumed after a user gesture (Start click). */
let mintAudioCtx = null;

function ensureMintAudio() {
  try {
    const Ctx = window.AudioContext || window.webkitAudioContext;
    if (!Ctx) return null;
    if (!mintAudioCtx || mintAudioCtx.state === "closed") {
      mintAudioCtx = new Ctx();
    }
    if (mintAudioCtx.state === "suspended") {
      mintAudioCtx.resume().catch(() => {});
    }
    return mintAudioCtx;
  } catch {
    return null;
  }
}

/** Unlock WebAudio on any click/keydown so later confirm can play. */
function armMintAudioOnGesture() {
  const arm = () => {
    ensureMintAudio();
  };
  document.addEventListener("pointerdown", arm, { once: true, capture: true });
  document.addEventListener("keydown", arm, { once: true, capture: true });
}

/**
 * Two-tone chime in the webview (secondary to Windows system Beep).
 */
function playConfirmChime() {
  try {
    const ctx = ensureMintAudio();
    if (!ctx) return;
    const playTone = (freq, when, dur, gain) => {
      const o = ctx.createOscillator();
      const g = ctx.createGain();
      o.type = "sine";
      o.frequency.value = freq;
      g.gain.setValueAtTime(0.0001, when);
      g.gain.exponentialRampToValueAtTime(gain, when + 0.02);
      g.gain.exponentialRampToValueAtTime(0.0001, when + dur);
      o.connect(g);
      g.connect(ctx.destination);
      o.start(when);
      o.stop(when + dur + 0.02);
    };
    const t0 = ctx.currentTime;
    playTone(880, t0, 0.14, 0.22);
    playTone(1175, t0 + 0.14, 0.2, 0.2);
  } catch {
    /* ignore */
  }
}

/**
 * First on-chain confirm UI. Sound when Settings.beep is true
 * (payload.beep from Rust snapshot). OS also beeps from Rust on Windows.
 * @param {{ beep?: boolean } | null} payload
 */
function flashConfirmBadge(payload) {
  const b = $("confirm-badge");
  if (b) {
    b.classList.remove("hidden");
    setTimeout(() => b.classList.add("hidden"), 3500);
  }
  const allowBeep = payload && payload.beep === true;
  if (allowBeep) {
    playConfirmChime();
  }
}

async function loadAppVersion() {
  try {
    appVersionStr = await invoke("app_version");
  } catch {
    appVersionStr = "0.1.0";
  }
  const el = $("app-version");
  if (el) el.textContent = "v" + appVersionStr;
}

/** Pure: explorer URL (mirrors core mint_ops for offline UI). */
function explorerTxUrlLocal(chain, txHash) {
  const h = String(txHash || "").startsWith("0x")
    ? String(txHash)
    : "0x" + String(txHash || "");
  const c = String(chain || "ethereum").toLowerCase();
  const map = {
    ethereum: "https://etherscan.io/tx/",
    eth: "https://etherscan.io/tx/",
    "1": "https://etherscan.io/tx/",
    base: "https://basescan.org/tx/",
    "8453": "https://basescan.org/tx/",
    polygon: "https://polygonscan.com/tx/",
    "137": "https://polygonscan.com/tx/",
    arbitrum: "https://arbiscan.io/tx/",
    "42161": "https://arbiscan.io/tx/",
    monad: "https://monadscan.com/tx/",
    "143": "https://monadscan.com/tx/",
    megaeth: "https://mega.etherscan.io/tx/",
    "4326": "https://mega.etherscan.io/tx/",
    robinhood: "https://robinhoodchain.blockscout.com/tx/",
    "robinhood_chain": "https://robinhoodchain.blockscout.com/tx/",
    "4663": "https://robinhoodchain.blockscout.com/tx/",
    apechain: "https://apescan.io/tx/",
    "33139": "https://apescan.io/tx/",
    shape: "https://shapescan.xyz/tx/",
    "360": "https://shapescan.xyz/tx/",
  };
  return (map[c] || "https://etherscan.io/tx/") + h;
}

/** Virtualized tbody: pad rows + only paint visible window. */
function paintVirtualTbody(wrap, tbody, count, paintRow) {
  if (!wrap || !tbody) return;
  if (count === 0) {
    tbody.innerHTML = "";
    return;
  }
  const scrollTop = wrap.scrollTop;
  const viewH = wrap.clientHeight || 320;
  let start = Math.floor(scrollTop / ROW_H) - OVERSCAN;
  if (start < 0) start = 0;
  let end = Math.ceil((scrollTop + viewH) / ROW_H) + OVERSCAN;
  if (end > count) end = count;
  const topPad = start * ROW_H;
  const botPad = (count - end) * ROW_H;
  const frag = document.createDocumentFragment();
  if (topPad > 0) {
    const tr = document.createElement("tr");
    tr.className = "vtable-pad";
    tr.innerHTML = `<td colspan="16" style="height:${topPad}px"></td>`;
    frag.appendChild(tr);
  }
  for (let i = start; i < end; i++) {
    frag.appendChild(paintRow(i));
  }
  if (botPad > 0) {
    const tr = document.createElement("tr");
    tr.className = "vtable-pad";
    tr.innerHTML = `<td colspan="16" style="height:${botPad}px"></td>`;
    frag.appendChild(tr);
  }
  tbody.replaceChildren(frag);
}

function bindVirtualScroll(wrapId, onScroll) {
  const wrap = $(wrapId);
  if (!wrap || wrap.dataset.vbound === "1") return;
  wrap.dataset.vbound = "1";
  let ticking = false;
  wrap.addEventListener("scroll", () => {
    if (ticking) return;
    ticking = true;
    requestAnimationFrame(() => {
      ticking = false;
      onScroll();
    });
  });
}

function show(el) {
  if (el) el.classList.remove("hidden");
}
function hide(el) {
  if (el) el.classList.add("hidden");
}

/**
 * Unified confirm modal.
 * @param {{ title: string, body?: string, lines?: string[], requireWord?: string|null, okLabel?: string }} opts
 * @returns {Promise<boolean>}
 */
function openConfirmModal(opts) {
  const {
    title,
    body = "",
    lines = [],
    requireWord = null,
    okLabel = "Continue",
  } = opts;
  return new Promise((resolve) => {
    modalResolve = resolve;
    $("modal-title").textContent = title;
    $("modal-body").textContent = body;
    $("modal-error").textContent = "";
    const list = $("modal-list");
    list.innerHTML = "";
    for (const line of lines) {
      const li = document.createElement("li");
      li.textContent = line;
      list.appendChild(li);
    }
    const wrap = $("modal-confirm-wrap");
    const input = $("modal-confirm-input");
    if (requireWord) {
      show(wrap);
      $("modal-confirm-word").textContent = requireWord;
      input.value = "";
      input.placeholder = requireWord;
      setTimeout(() => input.focus(), 50);
    } else {
      hide(wrap);
      input.value = "";
    }
    $("modal-ok").textContent = okLabel;
    show($("modal-overlay"));
  });
}

function closeModal(ok) {
  hide($("modal-overlay"));
  const r = modalResolve;
  modalResolve = null;
  if (r) r(!!ok);
}

$("modal-cancel")?.addEventListener("click", () => closeModal(false));
$("modal-overlay")?.addEventListener("click", (e) => {
  if (e.target === $("modal-overlay")) closeModal(false);
});
$("modal-ok")?.addEventListener("click", () => {
  const wrap = $("modal-confirm-wrap");
  if (!wrap.classList.contains("hidden")) {
    const need = $("modal-confirm-word").textContent.trim();
    const got = $("modal-confirm-input").value.trim();
    if (got !== need) {
      $("modal-error").textContent = `Type ${need} exactly to continue`;
      return;
    }
  }
  closeModal(true);
});
$("modal-confirm-input")?.addEventListener("keydown", (e) => {
  if (e.key === "Enter") $("modal-ok").click();
  if (e.key === "Escape") closeModal(false);
});

function escapeHtml(t) {
  return String(t)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function shortAddr(a) {
  if (!a) return "-";
  const s = String(a);
  return s.length > 14 ? s.slice(0, 6) + ".." + s.slice(-4) : s;
}

async function refreshStatus() {
  const s = await invoke("get_status");
  lastUiStatus = s;
  const vaultCls = s.unlocked ? "ok" : "warn";
  const rpcCls = s.rpc_ok ? "ok" : "warn";
  // Compact operator status — values only, no "from Settings" noise
  $("status-bar").innerHTML = `
    Vault <span class="${vaultCls}">${escapeHtml(s.vault_label)}</span>
    · <span class="ok">${s.wallet_count}</span> wallets
    · <span class="${rpcCls}">${escapeHtml(s.network || "Not selected")}</span>
    · RPC <span class="${rpcCls}">${escapeHtml(s.rpc)}</span>
  `;
  const chip = $("mode-chip");
  if (chip) {
    chip.textContent = s.dry_run ? "Dry Run" : "LIVE";
    chip.className = "chip " + (s.dry_run ? "ok" : "bad");
    chip.dataset.dry = s.dry_run ? "1" : "0";
  }
  const hint = $("hint");
  if (hint) {
    hint.innerHTML = `<strong>${escapeHtml(s.hint_title)}</strong><p>${escapeHtml(s.hint_body)}</p>`;
  }
  const stats = $("home-stats");
  if (stats) {
    stats.innerHTML = `
      <div class="stat-card"><div class="stat-label">Vault</div><div class="stat-value">${escapeHtml(s.vault_label)}</div></div>
      <div class="stat-card"><div class="stat-label">Wallets</div><div class="stat-value">${s.wallet_count}</div></div>
      <div class="stat-card"><div class="stat-label">Network</div><div class="stat-value" style="font-size:0.95rem">${escapeHtml(s.network || "—")}</div></div>
      <div class="stat-card"><div class="stat-label">Mode</div><div class="stat-value ${s.dry_run ? "success" : "fail"}">${s.dry_run ? "Dry" : "Live"}</div></div>
    `;
  }
  // Refresh vault address set for task readiness (best-effort).
  if (s.unlocked) {
    try {
      const list = await invoke("list_wallets");
      vaultAddrSet = new Set(list.map((w) => String(w.address).toLowerCase()));
    } catch {
      /* ignore */
    }
  } else {
    vaultAddrSet = new Set();
  }
  if (tasksLoaded) renderTaskList();
  return s;
}

function showPage(name) {
  document.querySelectorAll(".page").forEach((p) => p.classList.add("hidden"));
  const page = $("page-" + name);
  if (page) page.classList.remove("hidden");
  document.querySelectorAll(".nav-item").forEach((b) => {
    b.classList.toggle("active", b.dataset.page === name);
  });
  const title = $("page-title");
  if (title) title.textContent = t("page." + name) || name;
}

async function navigate(name) {
  showPage(name);
  if (name === "home") await refreshStatus();
  if (name === "wallets") await loadWallets();
  if (name === "settings") await loadSettings();
  if (name === "proxies") await loadProxiesPage();
  if (name === "nfts") renderNftsPage();
  if (name === "tasks") {
    await refreshStatus();
    renderTaskList();
  }
  if (name === "wl") {
    await refreshStatus();
    await loadWlWallets();
  }
  if (name === "raw") {
    await refreshStatus();
    await loadRawWallets();
  }
}

function showMain() {
  hide($("view-unlock"));
  show($("view-main"));
  loadAppVersion().catch(console.error);
  setupMintSideListeners().catch(console.error);
  armMintAudioOnGesture();
  Promise.all([loadTasksFromDisk(), loadRunsHistoryFromDisk()])
    .then(() => navigate("home"))
    .then(() => maybeOnboard())
    .catch(console.error);
}

async function loadRunsHistoryFromDisk() {
  try {
    const file = await invoke("load_runs_history");
    const runs = Array.isArray(file?.runs) ? file.runs : [];
    mintRunHistory = runs
      .map((r) => ({
        at: r.at || r.startedAt || null,
        slug: r.slug || "",
        phase: r.phase || "",
        chain: r.chain || "",
        confirmed: r.confirmed ?? 0,
        failed: r.failed ?? 0,
        elapsedMs: r.elapsedMs ?? r.elapsed_ms ?? null,
        dryRun: !!(r.dryRun ?? r.dry_run),
        exportJson: r.exportJson || r.export_json || null,
        exportCsv: r.exportCsv || r.export_csv || null,
      }))
      .filter((r) => r.slug || r.at);
    if (mintRunHistory.length > 100) mintRunHistory.length = 100;
    runsHistoryLoaded = true;
    // Restore home "last mint" from newest run if empty
    const home = $("home-last-mint");
    if (home && mintRunHistory.length && home.dataset.hasRun !== "1") {
      const r = mintRunHistory[0];
      home.dataset.hasRun = "1";
      home.textContent = [
        `${r.slug} · ${r.phase} · ${r.chain}`,
        `ok=${r.confirmed} fail=${r.failed} dry=${r.dryRun} ${r.elapsedMs ?? "—"}ms`,
        r.exportJson || "",
        r.exportCsv || "",
      ]
        .filter(Boolean)
        .join("\n");
    }
  } catch (e) {
    console.warn("load_runs_history", e);
    runsHistoryLoaded = true;
  }
}

function scheduleSaveRunsHistory() {
  if (runsHistorySaveTimer) clearTimeout(runsHistorySaveTimer);
  runsHistorySaveTimer = setTimeout(() => {
    saveRunsHistoryToDisk().catch((e) => console.warn("save runs history", e));
  }, 300);
}

async function saveRunsHistoryToDisk() {
  await invoke("save_runs_history", {
    file: {
      version: 1,
      runs: mintRunHistory.slice(0, 100).map((r) => ({
        at: r.at,
        slug: r.slug,
        phase: r.phase,
        chain: r.chain,
        confirmed: r.confirmed,
        failed: r.failed,
        elapsedMs: r.elapsedMs,
        dryRun: r.dryRun,
        exportJson: r.exportJson || null,
        exportCsv: r.exportCsv || null,
      })),
    },
  });
}

async function setupMintSideListeners() {
  try {
    const { listen } = window.__TAURI__.event;
    await listen("mint-first-confirm", (ev) => {
      flashConfirmBadge(ev.payload || { beep: false });
    });
    await listen("mint-reauth", (ev) => {
      const d = ev.payload?.detail || ev.payload?.message || "re-auth";
      showToast(String(d), "warn");
    });
  } catch (e) {
    console.warn("side listeners", e);
  }
}

// —— First-run onboarding ——
const ONBOARD_KEY = "minter_onboard_v1_done";

async function maybeOnboard() {
  try {
    if (localStorage.getItem(ONBOARD_KEY) === "1") return;
    const s = await invoke("get_status");
    const steps = [];
    if (!s.wallet_count) steps.push("Import or add burner wallets (Wallets)");
    if (!s.rpc_ok) steps.push("Set Alchemy or RPC URLs (Settings) and Probe (RPCs)");
    steps.push("Open Tasks → create task → Start → sim → tx → wait for confirm");
    steps.push("Only switch to LIVE when you are sure (top-right chip)");
    $("onboard-title").textContent = "Setup checklist";
    $("onboard-body").textContent =
      s.wallet_count && s.rpc_ok
        ? "Vault is ready. Quick path:"
        : "Complete these steps before a live drop:";
    const ol = $("onboard-steps");
    ol.innerHTML = "";
    for (const t of steps) {
      const li = document.createElement("li");
      li.textContent = t;
      ol.appendChild(li);
    }
    show($("onboard-overlay"));
  } catch (e) {
    console.warn("onboard", e);
  }
}

$("onboard-skip")?.addEventListener("click", () => {
  localStorage.setItem(ONBOARD_KEY, "1");
  hide($("onboard-overlay"));
});
$("onboard-next")?.addEventListener("click", async () => {
  localStorage.setItem(ONBOARD_KEY, "1");
  hide($("onboard-overlay"));
  const s = await invoke("get_status").catch(() => null);
  if (s && !s.wallet_count) navigate("wallets");
  else if (s && !s.rpc_ok) navigate("settings");
  else navigate("tasks");
});

// Unlock
const burner = $("burner-accept");
const btnUnlock = $("btn-unlock");
burner.addEventListener("change", () => {
  btnUnlock.disabled = !burner.checked;
});
btnUnlock.addEventListener("click", async () => {
  $("unlock-error").textContent = "";
  try {
    await invoke("accept_burner");
    const n = await invoke("unlock", { password: $("unlock-password").value });
    showMain();
    if ($("wallet-msg")) {
      $("wallet-msg").textContent =
        n === 0 ? "Vault unlocked (empty — import burners)" : `Unlocked ${n} wallet(s)`;
    }
  } catch (e) {
    $("unlock-error").textContent = String(e);
  }
});

// Sidebar nav
document.querySelectorAll(".nav-item[data-page]").forEach((btn) => {
  btn.addEventListener("click", () => navigate(btn.dataset.page));
});
document.querySelectorAll("[data-goto]").forEach((btn) => {
  btn.addEventListener("click", () => navigate(btn.dataset.goto));
});

// Language EN/RU
applyI18n();
$("lang-chip")?.addEventListener("click", () => {
  setLang(getLang() === "en" ? "ru" : "en");
  applyI18n();
  const title = $("page-title");
  const active = document.querySelector(".nav-item.active");
  if (title && active?.dataset.page) {
    title.textContent = t("page." + active.dataset.page);
  }
  renderWalletsVirtual();
  scheduleMintTableRender();
  if (tasksLoaded) renderTaskList();
});

// Dry / Live chip toggle
$("mode-chip")?.addEventListener("click", async () => {
  const chip = $("mode-chip");
  const currentlyDry = chip.dataset.dry !== "0";
  if (currentlyDry) {
    // switching to LIVE — one click, no typed word
    try {
      await invoke("set_dry_run", { dryRun: false });
      if ($("set-dry")) $("set-dry").checked = false;
      await refreshStatus();
    } catch (e) {
      alert(String(e));
    }
  } else {
    try {
      await invoke("set_dry_run", { dryRun: true });
      if ($("set-dry")) $("set-dry").checked = true;
      await refreshStatus();
    } catch (e) {
      alert(String(e));
    }
  }
});

// —— Wallets (virtualized) + groups / proxy map / balances / import ——
/** address(lower) → group A/B/C */
let walletGroups = {};
/** address(lower) → proxy list index */
let walletProxyMap = {};
/** @type {{index:number,label:string}[]} */
let proxyListItems = [];
let walletMetaLoaded = false;
let walletMetaTimer = null;

function addrKey(a) {
  return String(a || "").trim().toLowerCase();
}

function scheduleSaveWalletMeta() {
  if (walletMetaTimer) clearTimeout(walletMetaTimer);
  walletMetaTimer = setTimeout(() => {
    saveWalletMeta().catch((e) => console.warn("wallet_meta", e));
  }, 250);
}

async function loadWalletMeta() {
  try {
    const f = await invoke("load_wallet_meta");
    walletGroups = f.groups || {};
    walletProxyMap = f.proxyMap || {};
    // normalize keys
    const g = {};
    for (const [k, v] of Object.entries(walletGroups)) g[addrKey(k)] = v;
    walletGroups = g;
    const p = {};
    for (const [k, v] of Object.entries(walletProxyMap)) p[addrKey(k)] = Number(v);
    walletProxyMap = p;
    walletMetaLoaded = true;
  } catch (e) {
    console.warn("load_wallet_meta", e);
    walletMetaLoaded = true;
  }
}

async function saveWalletMeta() {
  await invoke("save_wallet_meta", {
    file: {
      version: 1,
      groups: walletGroups,
      proxyMap: walletProxyMap,
    },
  });
}

function walletGroupOf(address) {
  return walletGroups[addrKey(address)] || "";
}

function walletProxyIdxOf(w) {
  const k = addrKey(w.address);
  if (walletProxyMap[k] != null && Number.isFinite(Number(walletProxyMap[k]))) {
    return Number(walletProxyMap[k]);
  }
  return w.proxyIndex != null ? Number(w.proxyIndex) : null;
}

function walletProxyLabel(w) {
  const idx = walletProxyIdxOf(w);
  if (idx == null || !proxyListItems.length) return w.proxy || "direct";
  const item = proxyListItems.find((p) => p.index === idx);
  return item ? item.label : w.proxy || `p${idx}`;
}

function paintWalletRow(i) {
  const w = walletData[i];
  const tr = document.createElement("tr");
  tr.dataset.address = w.address;
  const sel = walletSelection.has(w.address);
  if (sel) tr.classList.add("selected");
  tr.style.height = ROW_H + "px";
  const g = walletGroupOf(w.address);
  const pidx = walletProxyIdxOf(w);
  let proxyOpts = `<option value="">auto</option>`;
  if (proxyListItems.length) {
    proxyOpts += proxyListItems
      .map(
        (p) =>
          `<option value="${p.index}" ${pidx === p.index ? "selected" : ""}>${escapeHtml(
            `#${p.index} ${p.label}`
          )}</option>`
      )
      .join("");
  } else {
    proxyOpts = `<option value="">${escapeHtml(w.proxy || "direct")}</option>`;
  }
  const bal =
    w.balanceEth != null
      ? `<span class="${w.balanceOk ? "ok" : "warn"}">${escapeHtml(w.balanceEth)}</span>`
      : `<span class="muted">—</span>`;
  tr.innerHTML = `
    <td><input type="checkbox" class="wallet-cb" data-addr="${escapeHtml(w.address)}" ${sel ? "checked" : ""} /></td>
    <td class="muted">${w.index}</td>
    <td class="mono" title="${escapeHtml(w.address)}">${escapeHtml(shortAddr(w.address))}</td>
    <td><span class="wallet-group-pill ${g ? "g-" + g : ""}">${g || "—"}</span></td>
    <td><select class="wallet-proxy-sel" data-addr="${escapeHtml(w.address)}">${proxyOpts}</select></td>
    <td class="mono">${bal}</td>`;
  const cb = tr.querySelector(".wallet-cb");
  cb.addEventListener("change", () => {
    const a = cb.dataset.addr;
    if (cb.checked) walletSelection.add(a);
    else walletSelection.delete(a);
    tr.classList.toggle("selected", cb.checked);
    updateWalletBulk();
  });
  const selEl = tr.querySelector(".wallet-proxy-sel");
  selEl?.addEventListener("change", () => {
    const a = addrKey(selEl.dataset.addr);
    const v = selEl.value;
    if (v === "" || v == null) delete walletProxyMap[a];
    else walletProxyMap[a] = Number(v);
    scheduleSaveWalletMeta();
  });
  return tr;
}

function renderWalletsVirtual() {
  const wrap = $("wallet-table-wrap");
  const tb = $("wallet-tbody");
  if (!tb) return;
  if (!walletData.length) {
    tb.innerHTML = `<tr><td colspan="6" class="muted">${escapeHtml(t("wallets.empty"))}</td></tr>`;
    return;
  }
  paintVirtualTbody(wrap, tb, walletData.length, paintWalletRow);
}

async function loadWallets() {
  if (!walletMetaLoaded) await loadWalletMeta();
  try {
    proxyListItems = (await invoke("list_proxies")) || [];
  } catch {
    proxyListItems = [];
  }
  const list = await invoke("list_wallets");
  const ul = $("wallet-list");
  if (ul) ul.innerHTML = "";
  // preserve balance cache by address
  const balMap = new Map(
    walletData
      .filter((w) => w.balanceEth != null)
      .map((w) => [addrKey(w.address), { balanceEth: w.balanceEth, balanceOk: w.balanceOk }])
  );
  walletData = (list || []).map((w) => {
    const b = balMap.get(addrKey(w.address));
    return {
      ...w,
      group: walletGroupOf(w.address),
      balanceEth: b?.balanceEth,
      balanceOk: b?.balanceOk,
    };
  });
  // keep selection only for still-present addresses
  const present = new Set(walletData.map((w) => w.address));
  for (const a of [...walletSelection]) {
    if (!present.has(a)) walletSelection.delete(a);
  }
  updateWalletBulk();
  const hint = $("wallet-count-hint");
  if (hint) {
    const ga = walletData.filter((w) => walletGroupOf(w.address) === "A").length;
    const gb = walletData.filter((w) => walletGroupOf(w.address) === "B").length;
    const gc = walletData.filter((w) => walletGroupOf(w.address) === "C").length;
    hint.textContent = `${t("wallets.count", { n: walletData.length }) || walletData.length + " wallet(s)"} · A:${ga} B:${gb} C:${gc} · proxies:${proxyListItems.length}`;
  }
  bindVirtualScroll("wallet-table-wrap", renderWalletsVirtual);
  renderWalletsVirtual();
}

function updateWalletBulk() {
  const n = walletSelection.size;
  const el = $("wallet-selected-count");
  if (el) el.textContent = String(n);
  const copy = $("btn-wallets-copy");
  const del = $("btn-wallets-remove-sel");
  const toTask = $("btn-wallets-to-task");
  if (copy) copy.disabled = n === 0;
  if (del) del.disabled = n === 0;
  if (toTask) toTask.disabled = n === 0;
  const all = $("wallets-select-all");
  if (all && walletData.length) {
    all.checked = n > 0 && n === walletData.length;
    all.indeterminate = n > 0 && n < walletData.length;
  }
}

function setSelectedGroup(group) {
  if (!walletSelection.size) {
    if ($("wallet-msg")) $("wallet-msg").textContent = "Select wallets first";
    return;
  }
  for (const a of walletSelection) {
    const k = addrKey(a);
    if (!group) delete walletGroups[k];
    else walletGroups[k] = group;
  }
  scheduleSaveWalletMeta();
  renderWalletsVirtual();
  if ($("wallet-msg")) {
    $("wallet-msg").textContent = group
      ? `Set group ${group} on ${walletSelection.size} wallet(s)`
      : `Cleared group on ${walletSelection.size} wallet(s)`;
  }
  const hint = $("wallet-count-hint");
  if (hint) loadWallets(); // refresh counts
}

document.querySelectorAll(".btn-group[data-group]").forEach((btn) => {
  btn.addEventListener("click", () => setSelectedGroup(btn.dataset.group || ""));
});

$("wallets-select-all")?.addEventListener("change", (e) => {
  const on = e.target.checked;
  if (on) {
    for (const w of walletData) walletSelection.add(w.address);
  } else {
    walletSelection.clear();
  }
  updateWalletBulk();
  renderWalletsVirtual();
});

$("btn-wallets-copy")?.addEventListener("click", async () => {
  const text = [...walletSelection].join("\n");
  try {
    await navigator.clipboard.writeText(text);
    $("wallet-msg").textContent = `Copied ${walletSelection.size} address(es)`;
  } catch {
    $("wallet-msg").textContent = text;
  }
});

$("btn-wallets-remove-sel")?.addEventListener("click", async () => {
  if (!walletSelection.size) return;
  const ok = await openConfirmModal({
    title: "Remove wallets",
    body: `Remove ${walletSelection.size} wallet(s) from the encrypted vault?`,
    lines: [...walletSelection].slice(0, 8).map(shortAddr),
    requireWord: null,
    okLabel: "Delete",
  });
  if (!ok) return;
  try {
    for (const address of [...walletSelection]) {
      await invoke("remove_wallet", { address });
      delete walletGroups[addrKey(address)];
      delete walletProxyMap[addrKey(address)];
    }
    scheduleSaveWalletMeta();
    $("wallet-msg").textContent = "Removed selected wallets";
    await loadWallets();
    await refreshStatus();
  } catch (e) {
    $("wallet-msg").textContent = String(e);
  }
});

$("btn-wallets-balances")?.addEventListener("click", async () => {
  if ($("wallet-msg")) $("wallet-msg").textContent = "Checking balances…";
  $("btn-wallets-balances").disabled = true;
  try {
    const addrs = walletSelection.size
      ? [...walletSelection]
      : walletData.map((w) => w.address);
    const rows = await invoke("wallet_balances", {
      input: { walletAddresses: addrs },
    });
    const map = new Map(rows.map((r) => [addrKey(r.address), r]));
    for (const w of walletData) {
      const r = map.get(addrKey(w.address));
      if (r) {
        w.balanceEth = r.balanceEth;
        w.balanceOk = r.ok;
      }
    }
    renderWalletsVirtual();
    const okN = rows.filter((r) => r.ok).length;
    if ($("wallet-msg"))
      $("wallet-msg").textContent = `Balances: ${okN}/${rows.length} funded`;
  } catch (e) {
    if ($("wallet-msg")) $("wallet-msg").textContent = String(e);
  } finally {
    $("btn-wallets-balances").disabled = false;
  }
});

$("btn-wallets-to-task")?.addEventListener("click", () => {
  if (!walletSelection.size) return;
  openTaskModal({
    mode: "create",
    template: {
      name: `group-${Date.now().toString(36).slice(-4)}`,
      wallets: [...walletSelection],
    },
  });
  navigate("tasks");
});

$("btn-add-key").addEventListener("click", async () => {
  try {
    const addr = await invoke("add_key", { privateKey: $("add-key").value });
    $("add-key").value = "";
    $("wallet-msg").textContent = `Added ${addr}`;
    await loadWallets();
    await refreshStatus();
  } catch (e) {
    $("wallet-msg").textContent = String(e);
  }
});

$("btn-pick-keys")?.addEventListener("click", async () => {
  try {
    const path = await invoke("pick_file", {
      title: "Import private keys",
      filters: ["txt", "csv", "*"],
    });
    if (path) $("import-path").value = path;
  } catch (e) {
    $("wallet-msg").textContent = String(e);
  }
});

$("btn-pick-keys-multi")?.addEventListener("click", async () => {
  try {
    const paths = await invoke("pick_files", {
      title: "Import private key files",
      filters: ["txt", "csv", "*"],
    });
    if (!paths?.length) return;
    const n = await invoke("import_files", { paths });
    $("wallet-msg").textContent = `Imported ${n} key(s) from ${paths.length} file(s)`;
    await loadWallets();
    await refreshStatus();
  } catch (e) {
    $("wallet-msg").textContent = String(e);
  }
});

$("btn-import").addEventListener("click", async () => {
  try {
    let path = $("import-path").value.trim();
    if (!path) {
      path = await invoke("pick_file", {
        title: "Import private keys",
        filters: ["txt", "csv", "*"],
      });
      if (!path) return;
      $("import-path").value = path;
    }
    const n = await invoke("import_file", { path });
    $("wallet-msg").textContent = `Imported ${n} key(s)`;
    await loadWallets();
    await refreshStatus();
  } catch (e) {
    $("wallet-msg").textContent = String(e);
  }
});

// Drag-drop key files onto dropzone (reads file contents via browser File API)
const dropzone = $("wallet-dropzone");
if (dropzone) {
  ["dragenter", "dragover"].forEach((ev) => {
    dropzone.addEventListener(ev, (e) => {
      e.preventDefault();
      e.stopPropagation();
      dropzone.classList.add("is-dragover");
    });
  });
  ["dragleave", "drop"].forEach((ev) => {
    dropzone.addEventListener(ev, (e) => {
      e.preventDefault();
      e.stopPropagation();
      if (ev === "dragleave") dropzone.classList.remove("is-dragover");
    });
  });
  dropzone.addEventListener("drop", async (e) => {
    dropzone.classList.remove("is-dragover");
    const files = [...(e.dataTransfer?.files || [])];
    if (!files.length) return;
    $("wallet-msg").textContent = `Reading ${files.length} file(s)…`;
    try {
      const texts = await Promise.all(
        files.map(
          (f) =>
            new Promise((resolve, reject) => {
              const r = new FileReader();
              r.onload = () => resolve(String(r.result || ""));
              r.onerror = () => reject(r.error);
              r.readAsText(f);
            })
        )
      );
      const merged = texts.join("\n");
      const n = await invoke("import_keys_text", { text: merged });
      $("wallet-msg").textContent = `Imported ${n} key(s) from drop (${files.length} file(s))`;
      await loadWallets();
      await refreshStatus();
    } catch (err) {
      $("wallet-msg").textContent = String(err);
    }
  });
}

$("btn-remove-wallet").addEventListener("click", async () => {
  const address = $("remove-addr").value.trim();
  if (!address) {
    $("wallet-msg").textContent = "Enter address to remove";
    return;
  }
  if (!confirm("Remove wallet " + address + " from vault?")) return;
  try {
    await invoke("remove_wallet", { address });
    delete walletGroups[addrKey(address)];
    delete walletProxyMap[addrKey(address)];
    scheduleSaveWalletMeta();
    $("remove-addr").value = "";
    $("wallet-msg").textContent = "Removed " + address;
    await loadWallets();
    await refreshStatus();
  } catch (e) {
    $("wallet-msg").textContent = String(e);
  }
});

// —— RPCs ——
$("btn-probe").addEventListener("click", async () => {
  const ul = $("probe-list");
  ul.innerHTML = "<li>Probing…</li>";
  try {
    const rows = await invoke("probe_rpc");
    ul.innerHTML = "";
    for (const r of rows) {
      const li = document.createElement("li");
      const lat = r.latencyMs ?? r.latency_ms;
      const chain = r.chainId ?? r.chain_id;
      const short = r.urlShort || r.url_short;
      li.textContent = r.ok
        ? `OK ${lat}ms chainId=${chain}  ${short}`
        : `FAIL ${short} — ${r.error}`;
      ul.appendChild(li);
    }
    await refreshStatus();
  } catch (e) {
    ul.innerHTML = `<li class="error">${escapeHtml(String(e))}</li>`;
  }
});

function formatLatency(r) {
  const lines = ["=== RPC ==="];
  for (const row of r.rpc || []) {
    if (!row.ok) {
      lines.push("FAIL " + (row.urlShort || "") + " — " + (row.error || ""));
      continue;
    }
    lines.push(
      "OK " +
        row.urlShort +
        " chainId=" +
        row.chainId +
        " (" +
        row.chainIdMs +
        "ms)" +
        (row.blockNumber != null ? " block=" + row.blockNumber + " (" + row.blockMs + "ms)" : "") +
        (row.baseFeeGwei != null
          ? " fees=" + row.baseFeeGwei + "/" + row.priorityGwei + "gwei (" + row.feesMs + "ms)"
          : "") +
        (row.nonce != null ? " nonce=" + row.nonce + " (" + row.nonceMs + "ms)" : "")
    );
  }
  lines.push("", "=== Proxies ===");
  if (!(r.proxies || []).length) lines.push("(none)");
  else {
    for (const p of r.proxies) {
      lines.push((p.ok ? "OK" : "DOWN") + " " + p.label + " " + (p.status || ""));
    }
  }
  return lines.join("\n");
}

$("btn-latency").addEventListener("click", async () => {
  $("latency-out").textContent = "Measuring…";
  $("btn-latency").disabled = true;
  try {
    const r = await invoke("measure_latency");
    $("latency-out").textContent = formatLatency(r);
    await refreshStatus();
  } catch (e) {
    $("latency-out").textContent = String(e);
  } finally {
    $("btn-latency").disabled = false;
  }
});

// —— Proxies page ——
async function loadProxiesPage() {
  const s = await invoke("get_settings");
  $("set-proxy").value = s.proxyUrl || "";
  const proxyLines = (s.proxyUrl || "")
    .split(/\r?\n/)
    .map((l) => l.trim())
    .filter((l) => l && !l.startsWith("#"));
  $("proxy-count-hint").textContent = proxyLines.length
    ? `${proxyLines.length} line(s)`
    : "No proxies";
}

$("btn-pick-proxies")?.addEventListener("click", async () => {
  try {
    const path = await invoke("pick_file", {
      title: "Import proxies list",
      filters: ["txt", "*"],
    });
    if (!path) return;
    const text = await invoke("read_text_file", { path });
    const cur = $("set-proxy").value.trim();
    $("set-proxy").value = cur ? cur + "\n" + text : text;
    $("proxy-msg").textContent = "Loaded into editor — click Save proxies";
  } catch (e) {
    $("proxy-msg").textContent = String(e);
  }
});

$("btn-save-proxies")?.addEventListener("click", async () => {
  $("proxy-msg").textContent = "Saving…";
  try {
    const cur = await invoke("get_settings");
    const msg = await invoke("save_settings", {
      input: {
        proxyUrl: $("set-proxy").value,
        dryRun: cur.dryRun,
      },
    });
    $("proxy-msg").textContent = msg || "Saved";
    await loadProxiesPage();
    await refreshStatus();
  } catch (e) {
    $("proxy-msg").textContent = String(e);
  }
});

$("btn-proxy-health")?.addEventListener("click", async () => {
  $("proxy-health-out").textContent = "Probing…";
  try {
    const r = await invoke("measure_latency");
    const lines = (r.proxies || []).map(
      (p) => (p.ok ? "OK" : "DOWN") + "  " + p.label + "  " + (p.status || "")
    );
    $("proxy-health-out").textContent = lines.length ? lines.join("\n") : "(no proxies — direct only)";
  } catch (e) {
    $("proxy-health-out").textContent = String(e);
  }
});

// —— Settings ——
async function loadSettings() {
  const s = await invoke("get_settings");
  $("settings-path").textContent = s.configPath ? `File: ${s.configPath}` : "";
  $("set-alchemy").value = "";
  $("set-alchemy").placeholder = s.alchemyMasked
    ? `Stored ${s.alchemyMasked} — leave blank to keep`
    : "Paste Alchemy API key";
  $("alchemy-hint").textContent = s.alchemyMasked
    ? `Key on disk: ${s.alchemyMasked}`
    : "No Alchemy key yet — set one or use custom RPCs.";
  $("set-clear-alchemy").checked = false;
  $("set-rpc-urls").value = s.rpcUrls || "";
  $("set-rpc-eth").value = s.rpcUrlEthereum || "";
  $("set-rpc-base").value = s.rpcUrlBase || "";
  $("set-rpc-polygon").value = s.rpcUrlPolygon || "";
  // proxies live on Proxies page; keep value if element shared
  if ($("set-proxy") && document.getElementById("page-proxies")?.classList.contains("hidden") === false) {
    /* loaded by loadProxiesPage */
  } else if ($("set-proxy") && !$("set-proxy").value) {
    $("set-proxy").value = s.proxyUrl || "";
  }
  $("set-gas").value = s.gasLimit;
  $("set-prio").value = s.priorityFeeGwei || "auto";
  $("set-base-mult").value = s.baseFeeMultiplier || "2.0";
  $("set-gas-mult").value = s.gasMultiplier || "1.15";
  $("set-retries").value = s.maxRetries ?? 20;
  $("set-gql").checked = !!s.useGql;
  $("set-dry").checked = s.dryRun;
  $("set-quiet").checked = s.quiet;
  $("set-skip").checked = s.skipPreflight;
  $("set-beep").checked = s.beep;
  $("set-export").checked = s.exportResults !== false;
}

$("btn-sniper").addEventListener("click", async () => {
  await invoke("apply_sniper");
  await loadSettings();
  $("settings-msg").textContent = "Sniper preset applied (click Save to persist)";
});

$("btn-save-settings").addEventListener("click", async () => {
  $("settings-msg").textContent = "Saving…";
  try {
    // Preserve proxies from current settings if not on proxies form
    const cur = await invoke("get_settings");
    const proxyUrl = $("set-proxy")?.value ?? cur.proxyUrl ?? "";
    const msg = await invoke("save_settings", {
      input: {
        alchemyApiKey: $("set-alchemy").value,
        clearAlchemy: $("set-clear-alchemy").checked,
        rpcUrls: $("set-rpc-urls").value,
        rpcUrlEthereum: $("set-rpc-eth").value,
        rpcUrlBase: $("set-rpc-base").value,
        rpcUrlPolygon: $("set-rpc-polygon").value,
        proxyUrl,
        gasLimit: Number($("set-gas").value) || 0,
        useGql: $("set-gql").checked,
        priorityFeeGwei: $("set-prio").value || "auto",
        baseFeeMultiplier: $("set-base-mult").value || "2.0",
        gasMultiplier: $("set-gas-mult").value || "1.15",
        maxRetries: Number($("set-retries").value) || 20,
        quiet: $("set-quiet").checked,
        skipPreflight: $("set-skip").checked,
        beep: $("set-beep").checked,
        exportResults: $("set-export").checked,
        dryRun: $("set-dry").checked,
      },
    });
    $("settings-msg").textContent = msg || "Saved";
    await loadSettings();
    await refreshStatus();
  } catch (e) {
    $("settings-msg").textContent = String(e);
  }
});

// —— Advanced (under Tasks) ——
$("btn-security").addEventListener("click", async () => {
  const s = await invoke("security_status");
  $("security-out").textContent = JSON.stringify(s, null, 2);
});

function formatSweepRows(rows) {
  if (!rows || !rows.length) return "(no transfers / empty balances)";
  return rows
    .map((r) => {
      const tx = r.txHash || r.tx_hash || "—";
      const err = r.error ? ` err=${r.error}` : "";
      return `${String(r.status).padEnd(12)} ${r.address}  tx=${tx}${err}`;
    })
    .join("\n");
}

$("btn-sweep-eth").addEventListener("click", async () => {
  const dry = $("sweep-eth-dry").checked;
  const to = $("sweep-eth-to").value.trim();
  if (!to) {
    $("sweep-eth-out").textContent = "Destination required";
    return;
  }
  $("sweep-eth-out").textContent = dry ? "Dry-run Sweep ETH…" : "LIVE Sweep ETH…";
  $("btn-sweep-eth").disabled = true;
  try {
    const rows = await invoke("sweep_eth", { destination: to, dryRun: dry, confirm: "" });
    $("sweep-eth-out").textContent = formatSweepRows(rows);
  } catch (e) {
    $("sweep-eth-out").textContent = String(e);
  } finally {
    $("btn-sweep-eth").disabled = false;
  }
});

$("btn-sweep-nft").addEventListener("click", async () => {
  const dry = $("sweep-nft-dry").checked;
  const contract = $("sweep-nft-contract").value.trim();
  const to = $("sweep-nft-to").value.trim();
  if (!contract || !to) {
    $("sweep-nft-out").textContent = "Contract + destination required";
    return;
  }
  $("sweep-nft-out").textContent = dry ? "Dry-run Sweep NFTs…" : "LIVE Sweep NFTs…";
  $("btn-sweep-nft").disabled = true;
  try {
    const rows = await invoke("sweep_nfts", {
      contract,
      destination: to,
      dryRun: dry,
      confirm: "",
    });
    $("sweep-nft-out").textContent = formatSweepRows(rows);
  } catch (e) {
    $("sweep-nft-out").textContent = String(e);
  } finally {
    $("btn-sweep-nft").disabled = false;
  }
});

$("btn-clear-auth").addEventListener("click", async () => {
  try {
    $("security-out").textContent = await invoke("clear_auth_cache");
  } catch (e) {
    $("security-out").textContent = String(e);
  }
});

// —— WL Check page (multi-wallet eligibility + proxies) ——
async function loadWlWallets() {
  const box = $("wl-wallet-list");
  if (!box) return;
  try {
    const list = await invoke("list_wallets");
    if (!list.length) {
      box.innerHTML = `<div class="muted" style="padding:8px">${escapeHtml(t("wallets.empty"))}</div>`;
      return;
    }
    const prev = new Set(
      [...document.querySelectorAll(".wl-wallet-cb:checked")].map((c) =>
        String(c.value).toLowerCase()
      )
    );
    const keepPrev = prev.size > 0;
    box.innerHTML = "";
    for (const w of list) {
      const row = document.createElement("label");
      row.className = "task-wallet-row";
      const checked = keepPrev
        ? prev.has(String(w.address).toLowerCase())
        : true;
      row.innerHTML = `<input type="checkbox" class="wl-wallet-cb" value="${escapeHtml(w.address)}" ${
        checked ? "checked" : ""
      } />
        <span>${w.index}. ${escapeHtml(shortAddr(w.address))}</span>`;
      box.appendChild(row);
    }
    if ($("wl-wallets-all")) {
      const cbs = [...document.querySelectorAll(".wl-wallet-cb")];
      $("wl-wallets-all").checked =
        cbs.length > 0 && cbs.every((c) => c.checked);
    }
  } catch (e) {
    box.textContent = String(e);
  }
}

function selectedWlWallets() {
  return [...document.querySelectorAll(".wl-wallet-cb:checked")].map((cb) => cb.value);
}

function wlStageChips(labels, kind, maxShow = 4) {
  const list = labels || [];
  if (!list.length) return `<span class="muted">—</span>`;
  const show = list.slice(0, maxShow);
  const rest = list.length - show.length;
  const chips = show
    .map((lab) => {
      // strip " (not eligible)" noise for chips
      const short = String(lab).replace(/\s*\([^)]*\)\s*$/, "");
      return `<span class="wl-chip ${kind}" title="${escapeHtml(lab)}">${escapeHtml(short)}</span>`;
    })
    .join("");
  const more =
    rest > 0
      ? `<span class="wl-chip more" title="${escapeHtml(list.slice(maxShow).join(", "))}">+${rest}</span>`
      : "";
  return `<div class="wl-stage-chips">${chips}${more}</div>`;
}

function renderWlReport(report) {
  const tb = $("wl-tbody");
  const detail = $("wl-detail");
  const stats = $("wl-run-stats");
  if (!tb) return;
  const wallets = report.wallets || [];
  if (!wallets.length) {
    tb.innerHTML = `<tr><td colspan="5" class="muted">${escapeHtml(t("wl.empty") || "No check yet.")}</td></tr>`;
    if (detail) detail.textContent = "";
    return;
  }
  let ok = 0;
  let fail = 0;
  tb.innerHTML = "";
  const detailLines = [
    `Slug: ${report.slug}`,
    `ChainId: ${report.chainId}`,
    `Wallets: ${wallets.length}`,
    "",
  ];
  for (const w of wallets) {
    if (w.ok) ok++;
    else fail++;
    const tr = document.createElement("tr");
    const eligHtml = wlStageChips(w.eligibleLabels, "ok", 5);
    const notHtml = wlStageChips(w.notEligibleLabels, "no", 3);
    const st = w.ok
      ? (w.eligibleLabels || []).length
        ? `<span class="status-pill status-ok">WL</span>`
        : `<span class="status-pill status-wait">OK</span>`
      : `<span class="status-pill status-fail">FAIL</span>`;
    const meta = w.error
      ? `<span class="error cell-clip" title="${escapeHtml(w.error)}">${escapeHtml(String(w.error).slice(0, 40))}</span>`
      : `<span class="muted">${w.latencyMs || 0}ms</span>`;
    tr.innerHTML = `
      <td class="mono" title="${escapeHtml(w.address)}">${escapeHtml(shortAddr(w.address))}</td>
      <td>${eligHtml}</td>
      <td>${notHtml}</td>
      <td class="mono muted">${escapeHtml(w.proxy || "direct")}</td>
      <td><div class="wl-status-cell">${st}${meta}</div></td>`;
    tb.appendChild(tr);

    detailLines.push(`—— ${w.address} · proxy=${w.proxy || "direct"} · ${w.latencyMs || 0}ms ——`);
    if (w.error) {
      detailLines.push(`  ERROR: ${w.error}`);
    } else if (!(w.stages || []).length) {
      detailLines.push("  (no stages)");
    } else {
      for (const s of w.stages) {
        detailLines.push(
          `  ${s.label} | ${s.stageType} | ${s.eligible}` +
            (s.priceEth ? ` | ${s.priceEth} ETH` : "") +
            (s.maxMintable != null ? ` | max=${s.maxMintable}` : "")
        );
      }
    }
    detailLines.push("");
  }
  if (stats) {
    stats.textContent = (t("wl.done") || "{ok} ok · {fail} fail · {n} wallet(s)")
      .replace("{ok}", String(ok))
      .replace("{fail}", String(fail))
      .replace("{n}", String(wallets.length));
  }
  if (detail) detail.textContent = detailLines.join("\n");
}

$("wl-wallets-all")?.addEventListener("change", (e) => {
  const on = e.target.checked;
  document.querySelectorAll(".wl-wallet-cb").forEach((cb) => {
    cb.checked = on;
  });
});

$("btn-wl-check")?.addEventListener("click", async () => {
  const slug = $("wl-slug")?.value.trim();
  const wallets = selectedWlWallets();
  const msg = $("wl-msg");
  if (!slug) {
    if (msg) msg.textContent = "Slug required";
    return;
  }
  if (!wallets.length) {
    if (msg) msg.textContent = "Select at least one wallet";
    return;
  }
  if (msg) msg.textContent = t("wl.checking") || "Checking wallets…";
  if ($("btn-wl-check")) $("btn-wl-check").disabled = true;
  if ($("wl-run-stats")) $("wl-run-stats").textContent = "…";
  try {
    const report = await invoke("check_eligibility_wallets", {
      input: { slug, walletAddresses: wallets },
    });
    renderWlReport(report);
    if (msg) msg.textContent = "";
  } catch (e) {
    if (msg) msg.textContent = String(e);
    if ($("wl-detail")) $("wl-detail").textContent = String(e);
  } finally {
    if ($("btn-wl-check")) $("btn-wl-check").disabled = false;
  }
});

$("btn-test-auth")?.addEventListener("click", async () => {
  $("auth-out").textContent = "Auth…";
  $("btn-test-auth").disabled = true;
  try {
    const rows = await invoke("test_auth", { allWallets: $("auth-all").checked });
    $("auth-out").textContent = rows
      .map((r) =>
        r.ok
          ? `OK ${r.address} ${r.latencyMs}ms chain=${r.chainId} proxy=${r.proxy} token=${r.tokenMasked || ""}`
          : `FAIL ${r.address} ${r.latencyMs}ms — ${r.error || ""}`
      )
      .join("\n");
  } catch (e) {
    $("auth-out").textContent = String(e);
  } finally {
    $("btn-test-auth").disabled = false;
  }
});

function rawSelectedChain() {
  return ($("raw-chain")?.value || "").trim();
}

async function loadRawWallets() {
  const box = $("raw-wallet-list");
  if (!box) return;
  try {
    const list = await invoke("list_wallets");
    if (!list.length) {
      box.innerHTML = `<div class="muted" style="padding:8px">${escapeHtml(t("wallets.empty"))}</div>`;
      return;
    }
    const prev = new Set(
      [...document.querySelectorAll(".raw-wallet-cb:checked")].map((c) =>
        String(c.value).toLowerCase()
      )
    );
    const keepPrev = prev.size > 0;
    box.innerHTML = "";
    for (const w of list) {
      const row = document.createElement("label");
      row.className = "task-wallet-row";
      const checked = keepPrev
        ? prev.has(String(w.address).toLowerCase())
        : true;
      row.innerHTML = `<input type="checkbox" class="raw-wallet-cb" value="${escapeHtml(w.address)}" ${
        checked ? "checked" : ""
      } />
        <span>${w.index}. ${escapeHtml(shortAddr(w.address))}</span>`;
      box.appendChild(row);
    }
    if ($("raw-wallets-all")) {
      const cbs = [...document.querySelectorAll(".raw-wallet-cb")];
      $("raw-wallets-all").checked =
        cbs.length > 0 && cbs.every((c) => c.checked);
    }
  } catch (e) {
    box.textContent = String(e);
  }
}

function selectedRawWallets() {
  return [...document.querySelectorAll(".raw-wallet-cb:checked")].map((cb) => cb.value);
}

$("raw-wallets-all")?.addEventListener("change", (e) => {
  const on = e.target.checked;
  document.querySelectorAll(".raw-wallet-cb").forEach((cb) => {
    cb.checked = on;
  });
});

$("raw-wallet-list")?.addEventListener("change", (e) => {
  if (!e.target?.classList?.contains("raw-wallet-cb")) return;
  const cbs = [...document.querySelectorAll(".raw-wallet-cb")];
  if ($("raw-wallets-all") && cbs.length) {
    $("raw-wallets-all").checked = cbs.every((c) => c.checked);
  }
});

/** Parse ABI arg types from `name(type1,type2)` (no nested tuples depth tracking beyond parens). */
function rawFnArgTypes(sig) {
  const s = String(sig || "").trim();
  const open = s.indexOf("(");
  const close = s.lastIndexOf(")");
  if (open < 0 || close <= open) return null;
  const inner = s.slice(open + 1, close).trim();
  if (!inner) return [];
  const types = [];
  let depth = 0;
  let cur = "";
  for (const ch of inner) {
    if (ch === "(") {
      depth++;
      cur += ch;
    } else if (ch === ")") {
      depth--;
      cur += ch;
    } else if (ch === "," && depth === 0) {
      if (cur.trim()) types.push(cur.trim());
      cur = "";
    } else {
      cur += ch;
    }
  }
  if (cur.trim()) types.push(cur.trim());
  return types;
}

function updateRawFnHint() {
  const hint = $("raw-fn-hint");
  const paramsEl = $("raw-params");
  if (!hint) return;
  const types = rawFnArgTypes($("raw-fn")?.value);
  if (types === null) {
    hint.textContent = "";
    return;
  }
  if (types.length === 0) {
    hint.textContent =
      t("raw.fnHint0") ||
      "This function has 0 args → leave Params empty (do not put quantity here).";
    if (paramsEl) paramsEl.placeholder = t("raw.paramsEmpty") || "leave empty";
  } else {
    hint.textContent =
      (t("raw.fnHintN") || "Expected {n} param(s): {types}")
        .replace("{n}", String(types.length))
        .replace("{types}", types.join(", "));
    if (paramsEl) paramsEl.placeholder = types.join(", ");
  }
}

$("btn-raw-discover").addEventListener("click", async () => {
  const chain = rawSelectedChain();
  const contract = $("raw-contract").value.trim();
  if (!chain) {
    $("raw-out").textContent = t("raw.needChain") || "Select network first";
    return;
  }
  if (!contract) {
    $("raw-out").textContent = "Contract required";
    return;
  }
  $("raw-out").textContent = "Scanning…";
  $("btn-raw-discover").disabled = true;
  try {
    const fns = await invoke("discover_raw_functions", { contract, chain });
    const sel = $("raw-fn-select");
    sel.innerHTML = '<option value="">— select —</option>';
    for (const f of fns) {
      const opt = document.createElement("option");
      opt.value = f.signature;
      opt.textContent = f.signature + " (" + f.source + ")";
      sel.appendChild(opt);
    }
    if (fns.length) {
      $("raw-fn").value = fns[0].signature;
      sel.value = fns[0].signature;
    }
    updateRawFnHint();
    $("raw-out").textContent = fns.length ? `Found ${fns.length} function(s)` : "No functions discovered";
  } catch (e) {
    $("raw-out").textContent = String(e);
  } finally {
    $("btn-raw-discover").disabled = false;
  }
});

$("raw-fn-select").addEventListener("change", () => {
  if ($("raw-fn-select").value) $("raw-fn").value = $("raw-fn-select").value;
  updateRawFnHint();
});
$("raw-fn")?.addEventListener("input", updateRawFnHint);
$("raw-fn")?.addEventListener("change", updateRawFnHint);

$("btn-raw-mint").addEventListener("click", async () => {
  const chain = rawSelectedChain();
  const contract = $("raw-contract").value.trim();
  const fn = $("raw-fn").value.trim();
  const dry = $("raw-dry").checked;
  const wallets = selectedRawWallets();
  if (!chain || !contract || !fn) {
    $("raw-out").textContent = t("raw.needContractFn") || "Network + contract + function required";
    return;
  }
  if (!wallets.length) {
    $("raw-out").textContent = t("raw.needWallets") || "Select at least one wallet";
    return;
  }
  const types = rawFnArgTypes(fn);
  const params = ($("raw-params").value || "")
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  if (types && types.length !== params.length) {
    $("raw-out").textContent =
      (t("raw.paramMismatch") ||
        "Params count mismatch: {fn} expects {exp} arg(s), got {got}. For freemint() leave Params empty.")
        .replace("{fn}", fn)
        .replace("{exp}", String(types.length))
        .replace("{got}", String(params.length));
    return;
  }
  $("raw-out").textContent = dry
    ? `Dry-run raw mint (${wallets.length} wallet(s))…`
    : `LIVE raw mint (${wallets.length} wallet(s))…`;
  $("btn-raw-mint").disabled = true;
  try {
    const rows = await invoke("raw_mint", {
      input: {
        chain,
        contract,
        function: fn,
        params,
        valueEth: $("raw-value").value || "0",
        dryRun: dry,
        confirm: "",
        walletAddresses: wallets,
      },
    });
    $("raw-out").textContent = formatSweepRows(rows);
  } catch (e) {
    $("raw-out").textContent = String(e);
  } finally {
    $("btn-raw-mint").disabled = false;
  }
});

// —— Mint tasks (persist + edit/dup + ready/queue + countdown) ——
/** @type {import('./task-types').Task[]} */
let mintTasks = [];
let activeTaskId = null;
let taskIdSeq = 1;
/** @type {"create"|"edit"|"duplicate"} */
let taskModalMode = "create";
let taskModalEditId = null;
/** last loaded phases for startTime capture */
let lastLoadedPhases = null;
/** vault addresses lowercase set for readiness */
let vaultAddrSet = new Set();
/** @type {any} */
let lastUiStatus = null;
/** FIFO queue of task ids waiting to run */
let taskQueue = [];
let tasksLoaded = false;
let persistTimer = null;
let countdownTimer = null;
let queueProcessing = false;

const TASK_TEMPLATES = {
  sniper: {
    name: "Sniper",
    quantity: 1,
    gasMode: "auto",
    gasLimit: null,
    phaseIndex: null,
    chainOverride: "auto",
  },
  manualGas: {
    name: "Sniper manual gas",
    quantity: 1,
    gasMode: "manual",
    gasLimit: 250000,
    phaseIndex: null,
    chainOverride: "auto",
  },
  multi: {
    name: "Multi qty",
    quantity: 3,
    gasMode: "auto",
    gasLimit: null,
    phaseIndex: null,
    chainOverride: "auto",
  },
};

function nowMs() {
  return Date.now();
}

function newTaskId() {
  return `t${taskIdSeq++}_${nowMs().toString(36)}`;
}

function normalizeTask(raw) {
  const t0 = raw || {};
  let status = t0.status || "ready";
  // Never restore running/queued after restart
  if (status === "running" || status === "queued" || status === "blocked") {
    status = "ready";
  }
  const gasMode = t0.gasMode === "manual" ? "manual" : "auto";
  return {
    id: String(t0.id || newTaskId()),
    name: String(t0.name || t0.slug || "task").slice(0, 64),
    slug: String(t0.slug || "").trim(),
    wallets: Array.isArray(t0.wallets)
      ? t0.wallets.map((a) => String(a).trim()).filter(Boolean)
      : [],
    phaseIndex:
      t0.phaseIndex == null || t0.phaseIndex === ""
        ? null
        : Number.isFinite(Number(t0.phaseIndex))
          ? Number(t0.phaseIndex)
          : null,
    quantity: Math.max(1, Number(t0.quantity) || 1),
    gasMode,
    gasLimit:
      gasMode === "manual"
        ? Math.max(21000, Number(t0.gasLimit) || 250000)
        : null,
    chainOverride: t0.chainOverride || "auto",
    phaseStartAt:
      t0.phaseStartAt != null && Number.isFinite(Number(t0.phaseStartAt))
        ? Number(t0.phaseStartAt)
        : null,
    phaseLabel: t0.phaseLabel || null,
    filterBalance: t0.filterBalance !== false,
    priorityFeeGwei: t0.priorityFeeGwei || t0.priority_fee_gwei || "",
    atTime: t0.atTime || t0.at_time || "",
    walletQuantities:
      t0.walletQuantities && typeof t0.walletQuantities === "object"
        ? t0.walletQuantities
        : null,
    skipEstimateOnOpen: !!t0.skipEstimateOnOpen,
    status,
    createdAt: Number(t0.createdAt) || nowMs(),
    updatedAt: Number(t0.updatedAt) || nowMs(),
  };
}

function taskToPersist(task) {
  return {
    id: task.id,
    name: task.name,
    slug: task.slug,
    wallets: task.wallets,
    phaseIndex: task.phaseIndex,
    quantity: task.quantity,
    gasMode: task.gasMode,
    gasLimit: task.gasLimit,
    chainOverride: task.chainOverride,
    phaseStartAt: task.phaseStartAt,
    phaseLabel: task.phaseLabel,
    filterBalance: task.filterBalance !== false,
    priorityFeeGwei: task.priorityFeeGwei || "",
    atTime: task.atTime || "",
    walletQuantities: task.walletQuantities || null,
    skipEstimateOnOpen: !!task.skipEstimateOnOpen,
    // runtime statuses not persisted as running/queued
    status:
      task.status === "running" || task.status === "queued"
        ? "ready"
        : task.status === "blocked"
          ? "ready"
          : task.status || "ready",
    createdAt: task.createdAt,
    updatedAt: task.updatedAt,
  };
}

function schedulePersistTasks() {
  if (persistTimer) clearTimeout(persistTimer);
  persistTimer = setTimeout(() => {
    persistTasks().catch((e) => console.warn("persist tasks", e));
  }, 300);
}

async function persistTasks() {
  const file = {
    version: 1,
    tasks: mintTasks.map(taskToPersist),
  };
  await invoke("save_tasks", { file });
}

async function loadTasksFromDisk() {
  try {
    const file = await invoke("load_tasks");
    const list = Array.isArray(file?.tasks) ? file.tasks : [];
    mintTasks = list.map(normalizeTask);
    // bump seq past existing numeric ids
    for (const t of mintTasks) {
      const m = String(t.id).match(/^t(\d+)/);
      if (m) taskIdSeq = Math.max(taskIdSeq, Number(m[1]) + 1);
    }
    tasksLoaded = true;
    renderTaskList();
    ensureCountdownTimer();
  } catch (e) {
    console.warn("load_tasks", e);
    tasksLoaded = true;
  }
}

function computeBlockReasons(task) {
  const reasons = [];
  if (!task.slug || !String(task.slug).trim()) {
    reasons.push(t("tasks.block.slug") || "No collection slug");
  }
  if (!task.wallets || !task.wallets.length) {
    reasons.push(t("tasks.block.wallets") || "No wallets selected");
  }
  if (!lastUiStatus || !lastUiStatus.unlocked) {
    reasons.push(t("tasks.block.locked") || "Vault locked");
  } else if (!lastUiStatus.wallet_count) {
    reasons.push(t("tasks.block.noKeys") || "No keys in vault");
  } else if (task.wallets?.length && vaultAddrSet.size) {
    const missing = task.wallets.filter(
      (a) => !vaultAddrSet.has(String(a).toLowerCase())
    );
    if (missing.length) {
      reasons.push(
        (t("tasks.block.missingWallets") || "{n} wallet(s) not in vault").replace(
          "{n}",
          String(missing.length)
        )
      );
    }
  }
  if (lastUiStatus && !lastUiStatus.rpc_ok) {
    reasons.push(t("tasks.block.rpc") || "RPC not configured");
  }
  if (task.status === "running") {
    reasons.push(t("tasks.block.running") || "Already running");
  }
  return reasons;
}

function taskDisplayStatus(task) {
  if (task.status === "running") return "running";
  if (task.status === "queued") return "queued";
  if (task.status === "done") return "done";
  if (task.status === "error") return "error";
  const blocked = computeBlockReasons(task);
  if (blocked.length) return "blocked";
  return "ready";
}

function formatCountdown(phaseStartAt) {
  if (phaseStartAt == null || !Number.isFinite(phaseStartAt)) return "—";
  const now = Math.floor(Date.now() / 1000);
  const diff = phaseStartAt - now;
  if (diff <= 0) return t("tasks.open") || "OPEN";
  const h = Math.floor(diff / 3600);
  const m = Math.floor((diff % 3600) / 60);
  const s = diff % 60;
  const pad = (n) => String(n).padStart(2, "0");
  if (h > 0) return `T-${pad(h)}:${pad(m)}:${pad(s)}`;
  return `T-${pad(m)}:${pad(s)}`;
}

function ensureCountdownTimer() {
  const need = mintTasks.some(
    (tk) =>
      tk.phaseStartAt != null &&
      tk.phaseStartAt > Math.floor(Date.now() / 1000)
  );
  if (need && !countdownTimer) {
    countdownTimer = setInterval(tickCountdowns, 1000);
  } else if (!need && countdownTimer) {
    clearInterval(countdownTimer);
    countdownTimer = null;
  }
}

function tickCountdowns() {
  document.querySelectorAll(".task-countdown[data-start]").forEach((el) => {
    const start = Number(el.dataset.start);
    el.textContent = formatCountdown(Number.isFinite(start) ? start : null);
  });
  ensureCountdownTimer();
}

function syncTaskGasUi() {
  const mode = $("task-gas-mode")?.value || "auto";
  const wrap = $("task-gas-limit-wrap");
  if (wrap) {
    if (mode === "manual") show(wrap);
    else hide(wrap);
  }
}

function setModalTitle(mode) {
  const h = $("task-modal-title");
  if (!h) return;
  if (mode === "edit") h.textContent = t("tasks.edit") || "Edit task";
  else if (mode === "duplicate") h.textContent = t("tasks.duplicate") || "Duplicate task";
  else h.textContent = t("tasks.create") || "Create task";
}

function fillPhaseSelect(stages, recommendedIndex, selectedIndex) {
  const sel = $("wizard-phase");
  if (!sel) return;
  const rec = recommendedIndex ?? 0;
  sel.innerHTML = `<option value="">— auto #${rec + 1} —</option>`;
  for (const s of stages || []) {
    const opt = document.createElement("option");
    opt.value = String(s.index);
    const price = s.priceEth ? ` · ${s.priceEth} ETH` : "";
    const star = s.recommended ? " ★" : "";
    opt.textContent = `#${s.index + 1} ${s.label} (${s.stageType}) ${s.eligible}${price}${star}`;
    sel.appendChild(opt);
  }
  if (selectedIndex == null || selectedIndex === "") sel.value = "";
  else sel.value = String(selectedIndex);
}

function phaseMetaFromSelection() {
  const phaseRaw = $("wizard-phase")?.value;
  const phaseIndex =
    phaseRaw === "" || phaseRaw == null ? null : Number(phaseRaw);
  let phaseStartAt = null;
  let phaseLabel = null;
  if (lastLoadedPhases?.stages?.length) {
    const idx =
      phaseIndex != null && Number.isFinite(phaseIndex)
        ? phaseIndex
        : lastLoadedPhases.recommendedIndex ?? 0;
    const st = lastLoadedPhases.stages.find((s) => s.index === idx);
    if (st) {
      phaseStartAt =
        st.startTime != null && Number(st.startTime) > 0
          ? Number(st.startTime)
          : null;
      phaseLabel = st.label || `#${idx + 1}`;
    }
  }
  return {
    phaseIndex: Number.isFinite(phaseIndex) ? phaseIndex : null,
    phaseStartAt,
    phaseLabel,
  };
}

/**
 * @param {{ mode?: string, taskId?: string, template?: object }} opts
 */
/** Task modal wallet list cache + group filter */
let taskModalWalletCache = [];
let taskModalPreselect = null;
let taskGroupFilter = "all";

async function openTaskModal(opts = {}) {
  const mode = opts.mode || "create";
  taskModalMode = mode;
  taskModalEditId = opts.taskId || null;
  setModalTitle(mode);
  $("wizard-msg").textContent = "";
  lastLoadedPhases = null;
  taskGroupFilter = "all";

  let pref = {
    name: "",
    slug: "",
    quantity: 1,
    gasMode: "auto",
    gasLimit: 250000,
    phaseIndex: null,
    chainOverride: "auto",
    wallets: null,
    filterBalance: true,
  };

  if (opts.template) {
    pref = { ...pref, ...opts.template };
  }

  if ((mode === "edit" || mode === "duplicate") && opts.taskId) {
    const src = mintTasks.find((x) => x.id === opts.taskId);
    if (src) {
      pref = {
        name: mode === "duplicate" ? `Copy of ${src.name}`.slice(0, 64) : src.name,
        slug: src.slug,
        quantity: src.quantity,
        gasMode: src.gasMode || "auto",
        gasLimit: src.gasLimit || 250000,
        phaseIndex: src.phaseIndex,
        chainOverride: src.chainOverride || "auto",
        wallets: [...(src.wallets || [])],
        phaseStartAt: src.phaseStartAt,
        phaseLabel: src.phaseLabel,
        filterBalance: src.filterBalance !== false,
        // preserve mint fields 13/14/16 on edit (do not wipe)
        priorityFeeGwei: src.priorityFeeGwei || "",
        atTime: src.atTime || "",
        walletQuantities: src.walletQuantities
          ? { ...src.walletQuantities }
          : null,
        skipEstimateOnOpen: !!src.skipEstimateOnOpen,
      };
    }
  }

  if ($("task-name")) $("task-name").value = pref.name || "";
  if ($("wizard-slug")) $("wizard-slug").value = pref.slug || "";
  if ($("wizard-qty")) $("wizard-qty").value = String(pref.quantity || 1);
  if ($("task-gas-mode")) $("task-gas-mode").value = pref.gasMode || "auto";
  if ($("task-gas-limit")) $("task-gas-limit").value = String(pref.gasLimit || 250000);
  if ($("task-chain")) $("task-chain").value = pref.chainOverride || "auto";
  if ($("task-filter-balance")) $("task-filter-balance").checked = pref.filterBalance !== false;
  if ($("task-skip-est")) $("task-skip-est").checked = !!pref.skipEstimateOnOpen;
  if ($("task-prio")) $("task-prio").value = pref.priorityFeeGwei || "";
  if ($("task-at")) $("task-at").value = pref.atTime || "";
  if ($("task-per-wallet-qty")) {
    $("task-per-wallet-qty").checked = !!(pref.walletQuantities && Object.keys(pref.walletQuantities).length);
    syncTaskPerWalletQtyUi();
  }
  window.__taskQtyPref = pref.walletQuantities || null;
  if ($("wizard-phase")) {
    $("wizard-phase").innerHTML = `<option value="">— auto (recommended) —</option>`;
    if (pref.phaseIndex != null) {
      const opt = document.createElement("option");
      opt.value = String(pref.phaseIndex);
      opt.textContent = `#${pref.phaseIndex + 1}`;
      $("wizard-phase").appendChild(opt);
      $("wizard-phase").value = String(pref.phaseIndex);
    }
  }
  if ($("phase-hint")) $("phase-hint").textContent = "";
  syncTaskGasUi();
  show($("task-modal"));
  if (!walletMetaLoaded) await loadWalletMeta();
  taskModalChecked = new Set();
  await loadTaskModalWallets(pref.wallets);
}

function closeTaskModal() {
  hide($("task-modal"));
  taskModalMode = "create";
  taskModalEditId = null;
}

async function loadTaskModalWallets(preselect) {
  const box = $("task-wallet-list");
  if (!box) return;
  try {
    const list = await invoke("list_wallets");
    taskModalWalletCache = list || [];
    vaultAddrSet = new Set(list.map((w) => String(w.address).toLowerCase()));
    taskModalPreselect = preselect;
    renderTaskModalWalletList();
  } catch (e) {
    box.textContent = String(e);
  }
}

/** Filtered list for task modal virtual scroll */
let taskModalFiltered = [];
/** @type {Set<string>} lowercase addresses checked */
let taskModalChecked = new Set();

function syncTaskPerWalletQtyUi() {
  const on = $("task-per-wallet-qty")?.checked;
  const wrap = $("task-qty-map-wrap");
  if (!wrap) return;
  if (on) {
    show(wrap);
    renderTaskQtyMap();
  } else hide(wrap);
}

function renderTaskQtyMap() {
  const box = $("task-qty-map");
  if (!box) return;
  const defQty = Math.max(1, Number($("wizard-qty")?.value) || 1);
  const pref = window.__taskQtyPref || {};
  const wallets = selectedTaskWallets();
  box.innerHTML = "";
  for (const a of wallets) {
    const row = document.createElement("div");
    row.className = "task-qty-map-row";
    const k = addrKey(a);
    const val = pref[k] ?? pref[a] ?? defQty;
    row.innerHTML = `<span class="mono">${escapeHtml(shortAddr(a))}</span>
      <input type="number" min="1" max="50" class="task-qty-input" data-addr="${escapeHtml(a)}" value="${val}" />`;
    box.appendChild(row);
  }
}

function collectTaskWalletQuantities() {
  if (!$("task-per-wallet-qty")?.checked) return null;
  const defQty = Math.max(1, Number($("wizard-qty")?.value) || 1);
  const m = {};
  document.querySelectorAll(".task-qty-input").forEach((inp) => {
    const a = inp.dataset.addr;
    const q = Math.max(1, Number(inp.value) || defQty);
    m[addrKey(a)] = q;
  });
  return Object.keys(m).length ? m : null;
}

function renderTaskModalWalletList() {
  const box = $("task-wallet-list");
  if (!box) return;
  const list = taskModalWalletCache;
  if (!list.length) {
    box.innerHTML = `<div class="muted" style="padding:8px">${escapeHtml(t("wallets.empty"))}</div>`;
    return;
  }
  // seed checked once
  if (!taskModalChecked.size) {
    if (taskModalPreselect == null) {
      for (const w of list) taskModalChecked.add(addrKey(w.address));
    } else {
      for (const a of taskModalPreselect) taskModalChecked.add(addrKey(a));
    }
  }
  taskModalFiltered = list.filter((w) => {
    const g = walletGroupOf(w.address);
    return taskGroupFilter === "all" || g === taskGroupFilter;
  });
  box.classList.add("virtual");
  box.innerHTML = `<div class="task-wallet-virt-inner" id="task-wallet-virt-inner"></div>`;
  const inner = $("task-wallet-virt-inner");
  const n = taskModalFiltered.length;
  inner.style.height = n * TASK_WALLET_ROW_H + "px";
  const paint = () => {
    const scrollTop = box.scrollTop;
    let start = Math.floor(scrollTop / TASK_WALLET_ROW_H) - 4;
    if (start < 0) start = 0;
    let end = Math.ceil((scrollTop + box.clientHeight) / TASK_WALLET_ROW_H) + 4;
    if (end > n) end = n;
    const frag = document.createDocumentFragment();
    for (let i = start; i < end; i++) {
      const w = taskModalFiltered[i];
      const row = document.createElement("label");
      row.className = "task-wallet-row";
      row.style.position = "absolute";
      row.style.left = "0";
      row.style.right = "0";
      row.style.top = i * TASK_WALLET_ROW_H + "px";
      row.style.height = TASK_WALLET_ROW_H + "px";
      row.style.padding = "4px 8px";
      const key = addrKey(w.address);
      const checked = taskModalChecked.has(key);
      const g = walletGroupOf(w.address);
      const gBadge = g ? ` [${g}]` : "";
      row.innerHTML = `<input type="checkbox" class="task-wallet-cb" value="${escapeHtml(w.address)}" ${
        checked ? "checked" : ""
      } />
        <span>${w.index}. ${escapeHtml(shortAddr(w.address))}${escapeHtml(gBadge)}</span>`;
      row.querySelector("input").addEventListener("change", (e) => {
        if (e.target.checked) taskModalChecked.add(key);
        else taskModalChecked.delete(key);
        if ($("task-wallets-all")) {
          $("task-wallets-all").checked =
            taskModalFiltered.length > 0 &&
            taskModalFiltered.every((x) => taskModalChecked.has(addrKey(x.address)));
        }
        if ($("task-per-wallet-qty")?.checked) renderTaskQtyMap();
      });
      frag.appendChild(row);
    }
    inner.replaceChildren(frag);
  };
  if (box.dataset.vbound !== "1") {
    box.dataset.vbound = "1";
    let ticking = false;
    box.addEventListener("scroll", () => {
      if (ticking) return;
      ticking = true;
      requestAnimationFrame(() => {
        ticking = false;
        paint();
      });
    });
  }
  paint();
  if ($("task-wallets-all")) {
    $("task-wallets-all").checked =
      taskModalFiltered.length > 0 &&
      taskModalFiltered.every((x) => taskModalChecked.has(addrKey(x.address)));
  }
  if ($("task-per-wallet-qty")?.checked) renderTaskQtyMap();
}

$("task-per-wallet-qty")?.addEventListener("change", syncTaskPerWalletQtyUi);

document.querySelectorAll(".btn-group-filter").forEach((btn) => {
  btn.addEventListener("click", () => {
    taskGroupFilter = btn.dataset.groupFilter || "all";
    // Update checked set for ALL matching wallets (not only painted virtual rows)
    if (taskGroupFilter === "all") {
      // keep existing selection; only re-render filter
    } else {
      taskModalChecked = new Set();
      for (const w of taskModalWalletCache) {
        if (walletGroupOf(w.address) === taskGroupFilter) {
          taskModalChecked.add(addrKey(w.address));
        }
      }
    }
    renderTaskModalWalletList();
  });
});

function selectedTaskWallets() {
  // Prefer virtual checked set when present
  if (taskModalChecked && taskModalChecked.size) {
    // map keys back to original casing from cache
    const byKey = new Map(
      taskModalWalletCache.map((w) => [addrKey(w.address), w.address])
    );
    return [...taskModalChecked]
      .map((k) => byKey.get(k))
      .filter(Boolean);
  }
  return [...document.querySelectorAll(".task-wallet-cb:checked")].map((cb) => cb.value);
}

function statusPillClass(disp) {
  if (disp === "running") return "auth";
  if (disp === "queued") return "sent";
  if (disp === "done") return "ok";
  if (disp === "error") return "fail";
  if (disp === "blocked") return "fail";
  return "ok";
}

function renderTaskList() {
  const list = $("task-list");
  if (!list) return;
  list.innerHTML = "";
  updateQueueBar();
  if (!mintTasks.length) {
    list.innerHTML = `<div class="empty-tasks muted" id="task-list-empty">${escapeHtml(
      t("tasks.empty") || "No tasks yet — create one."
    )}</div>`;
    ensureCountdownTimer();
    return;
  }
  for (const task of mintTasks) {
    const card = document.createElement("div");
    const disp = taskDisplayStatus(task);
    const reasons = computeBlockReasons(task);
    const canStart =
      disp !== "running" &&
      disp !== "queued" &&
      disp !== "blocked" &&
      task.status !== "running";
    const busy = task.status === "running" || task.status === "queued";
    card.className =
      "task-card" +
      (task.id === activeTaskId ? " is-running" : "") +
      (disp === "blocked" ? " is-blocked" : "") +
      (disp === "queued" ? " is-queued" : "");
    card.dataset.taskId = task.id;
    const phase =
      task.phaseIndex == null ? "auto" : `#${task.phaseIndex + 1}`;
    const chain = task.chainOverride || "auto";
    const gasLabel =
      task.gasMode === "manual" && task.gasLimit
        ? `gas ${task.gasLimit}`
        : "gas auto";
    const prioLab = task.priorityFeeGwei
      ? `prio ${task.priorityFeeGwei}`
      : "prio auto";
    const atLab = task.atTime ? `at ${String(task.atTime).slice(0, 16)}` : "";
    const cd = formatCountdown(task.phaseStartAt);
    const qPos = taskQueue.indexOf(task.id);
    const qBadge =
      qPos >= 0
        ? `<span class="badge accent-badge">Q${qPos + 1}</span>`
        : "";
    const blockLine =
      disp === "blocked" && reasons.length
        ? `<div class="task-card-block">${escapeHtml(reasons[0])}</div>`
        : "";
    card.innerHTML = `
      <div class="task-card-main">
        <div class="task-card-title">
          <span>${escapeHtml(task.name)}</span>
          <span class="status-pill status-${statusPillClass(disp)}">${escapeHtml(
            disp
          )}</span>
          ${qBadge}
          <span class="badge muted-badge task-countdown" data-start="${
            task.phaseStartAt != null ? task.phaseStartAt : ""
          }">${escapeHtml(cd)}</span>
          <span class="badge muted-badge">${escapeHtml(gasLabel)}</span>
          <span class="badge muted-badge">${escapeHtml(prioLab)}</span>
          ${atLab ? `<span class="badge muted-badge">${escapeHtml(atLab)}</span>` : ""}
        </div>
        <div class="task-card-meta">
          ${escapeHtml(task.slug)} · ${task.wallets.length} wallet(s) · phase ${phase}${
            task.phaseLabel ? ` (${escapeHtml(task.phaseLabel)})` : ""
          } · chain ${escapeHtml(chain)} · qty ${task.quantity}${
            task.walletQuantities ? " · per-wallet qty" : ""
          }
        </div>
        ${blockLine}
      </div>
      <div class="task-card-actions">
        <button type="button" class="primary btn-task-start" data-id="${escapeHtml(task.id)}" ${
          canStart ? "" : "disabled"
        } title="${escapeHtml(reasons[0] || "")}">${escapeHtml(t("tasks.start"))}</button>
        <button type="button" class="btn-task-edit" data-id="${escapeHtml(task.id)}" ${
          busy ? "disabled" : ""
        }>${escapeHtml(t("tasks.edit") || "Edit")}</button>
        <button type="button" class="btn-task-dup" data-id="${escapeHtml(task.id)}" ${
          busy ? "disabled" : ""
        }>${escapeHtml(t("tasks.duplicate") || "Dup")}</button>
        <button type="button" class="danger-btn btn-task-del" data-id="${escapeHtml(task.id)}" ${
          busy ? "disabled" : ""
        }>${escapeHtml(t("tasks.delete") || "Delete")}</button>
      </div>`;
    list.appendChild(card);
  }
  list.querySelectorAll(".btn-task-start").forEach((btn) => {
    btn.addEventListener("click", () => requestStartTask(btn.dataset.id));
  });
  list.querySelectorAll(".btn-task-edit").forEach((btn) => {
    btn.addEventListener("click", () =>
      openTaskModal({ mode: "edit", taskId: btn.dataset.id })
    );
  });
  list.querySelectorAll(".btn-task-dup").forEach((btn) => {
    btn.addEventListener("click", () =>
      openTaskModal({ mode: "duplicate", taskId: btn.dataset.id })
    );
  });
  list.querySelectorAll(".btn-task-del").forEach((btn) => {
    btn.addEventListener("click", () => {
      const id = btn.dataset.id;
      if (id === activeTaskId) return;
      mintTasks = mintTasks.filter((tk) => tk.id !== id);
      taskQueue = taskQueue.filter((q) => q !== id);
      schedulePersistTasks();
      renderTaskList();
    });
  });
  ensureCountdownTimer();
}

function updateQueueBar() {
  const bar = $("task-queue-bar");
  const lab = $("task-queue-label");
  if (!bar || !lab) return;
  if (!taskQueue.length && !activeTaskId) {
    hide(bar);
    return;
  }
  show(bar);
  const parts = [];
  if (activeTaskId) {
    const a = mintTasks.find((x) => x.id === activeTaskId);
    parts.push(`${t("tasks.running") || "Running"}: ${a?.name || activeTaskId}`);
  }
  if (taskQueue.length) {
    parts.push(`${t("tasks.queued") || "Queued"}: ${taskQueue.length}`);
  }
  lab.textContent = parts.join(" · ");
}

$("btn-create-task")?.addEventListener("click", () => openTaskModal({ mode: "create" }));
$("task-modal-cancel")?.addEventListener("click", closeTaskModal);
$("task-modal")?.addEventListener("click", (e) => {
  if (e.target === $("task-modal")) closeTaskModal();
});
$("task-gas-mode")?.addEventListener("change", syncTaskGasUi);
$("task-wallets-all")?.addEventListener("change", (e) => {
  const on = e.target.checked;
  if (taskModalFiltered.length) {
    for (const w of taskModalFiltered) {
      const k = addrKey(w.address);
      if (on) taskModalChecked.add(k);
      else taskModalChecked.delete(k);
    }
    renderTaskModalWalletList();
  } else {
    document.querySelectorAll(".task-wallet-cb").forEach((cb) => {
      cb.checked = on;
    });
  }
});
$("btn-clear-queue")?.addEventListener("click", () => {
  for (const id of taskQueue) {
    const tk = mintTasks.find((x) => x.id === id);
    if (tk && tk.status === "queued") tk.status = "ready";
  }
  taskQueue = [];
  renderTaskList();
  appendMintLog(t("tasks.queueCleared") || "Queue cleared");
});

$("task-template")?.addEventListener("change", (e) => {
  const key = e.target.value;
  e.target.value = "";
  if (!key || !TASK_TEMPLATES[key]) return;
  openTaskModal({ mode: "create", template: { ...TASK_TEMPLATES[key] } });
});

$("task-modal-save")?.addEventListener("click", () => {
  const slug = $("wizard-slug").value.trim();
  const name = ($("task-name").value.trim() || slug || `task-${taskIdSeq}`).slice(0, 64);
  const wallets = selectedTaskWallets();
  if (!slug) {
    $("wizard-msg").textContent = "Slug required";
    return;
  }
  if (!wallets.length) {
    $("wizard-msg").textContent = "Select at least one wallet";
    return;
  }
  const gasMode = $("task-gas-mode")?.value === "manual" ? "manual" : "auto";
  let gasLimit = null;
  if (gasMode === "manual") {
    gasLimit = Math.max(21000, Number($("task-gas-limit")?.value) || 250000);
  }
  const { phaseIndex, phaseStartAt, phaseLabel } = phaseMetaFromSelection();
  const base = {
    name,
    slug,
    wallets,
    phaseIndex,
    quantity: Math.max(1, Number($("wizard-qty").value) || 1),
    gasMode,
    gasLimit,
    chainOverride: $("task-chain").value || "auto",
    phaseStartAt,
    phaseLabel,
    filterBalance: $("task-filter-balance")?.checked !== false,
    skipEstimateOnOpen: !!$("task-skip-est")?.checked,
    priorityFeeGwei: ($("task-prio")?.value || "").trim(),
    atTime: ($("task-at")?.value || "").trim(),
    walletQuantities: collectTaskWalletQuantities(),
    updatedAt: nowMs(),
  };

  if (taskModalMode === "edit" && taskModalEditId) {
    const idx = mintTasks.findIndex((x) => x.id === taskModalEditId);
    if (idx >= 0) {
      const prev = mintTasks[idx];
      if (prev.status === "running" || prev.status === "queued") {
        $("wizard-msg").textContent = "Cannot edit while running/queued";
        return;
      }
      mintTasks[idx] = normalizeTask({
        ...prev,
        ...base,
        id: prev.id,
        createdAt: prev.createdAt,
        status: prev.status === "done" || prev.status === "error" ? "ready" : prev.status,
      });
    }
  } else {
    mintTasks.unshift(
      normalizeTask({
        ...base,
        id: newTaskId(),
        status: "ready",
        createdAt: nowMs(),
      })
    );
  }
  schedulePersistTasks();
  closeTaskModal();
  renderTaskList();
  $("wizard-msg").textContent = "";
});

// —— Mint phases picker (in create-task modal) ——
$("btn-load-phases")?.addEventListener("click", async () => {
  const slug = $("wizard-slug").value.trim();
  if (!slug) {
    $("wizard-msg").textContent = "Enter collection slug first";
    return;
  }
  $("btn-load-phases").disabled = true;
  $("phase-hint").textContent = "Loading phases…";
  try {
    const r = await invoke("list_drop_phases", { slug });
    lastLoadedPhases = r;
    const prev = $("wizard-phase")?.value;
    fillPhaseSelect(r.stages, r.recommendedIndex, prev === "" ? null : prev);
    $("phase-hint").textContent = `${r.name} · ${r.chain} · ${r.stages.length} phase(s) · recommended #${r.recommendedIndex + 1}`;
    if (!$("task-name").value.trim()) $("task-name").value = r.slug || slug;
    $("wizard-msg").textContent = "Phases loaded";
  } catch (e) {
    $("phase-hint").textContent = "";
    $("wizard-msg").textContent = String(e);
  } finally {
    $("btn-load-phases").disabled = false;
  }
});

// —— Mint Wizard (Tasks) — virtualized rows ——
const mintRows = new Map();
let mintRowOrder = [];

function ensureMintRow(addr) {
  const key = addr || "_";
  if (mintRows.has(key)) return mintRows.get(key);
  const row = { address: addr || "-", status: "WAIT", detail: "", tx: "", error: "" };
  mintRows.set(key, row);
  mintRowOrder.push(key);
  return row;
}

/** Map wallet status → pill badge class + short label (ref-style chips). */
function statusBadge(status) {
  const raw = String(status || "WAIT");
  const st = raw.toUpperCase();
  let kind = "wait";
  let label = raw;
  if (st.includes("CONFIRM") || st === "OK") {
    kind = "ok";
    label = "OK";
  } else if (st.includes("DRY")) {
    kind = "dry";
    label = "DRY";
  } else if (st.includes("FAIL") || st.includes("CANCEL")) {
    kind = "fail";
    label = st.includes("CANCEL") ? "STOP" : "FAIL";
  } else if (st.includes("SENT") || st.includes("PEND")) {
    kind = "sent";
    label = "SENT";
  } else if (st.includes("AUTH")) {
    kind = "auth";
    label = "AUTH";
  } else if (st.includes("CALL") || st.includes("DATA") || st.includes("SIM")) {
    kind = "data";
    label = st.includes("SIM") ? "SIM" : "DATA";
  } else if (st.includes("WAIT")) {
    kind = "wait";
    label = "WAIT";
  }
  return { kind, label };
}

function paintMintRow(i) {
  const key = mintRowOrder[i];
  const row = mintRows.get(key);
  const tr = document.createElement("tr");
  tr.style.height = ROW_H + "px";
  const badge = statusBadge(row.status);
  let txCell = "—";
  if (row.tx) {
    const url = explorerTxUrlLocal(lastMintChain, row.tx);
    txCell = `<a class="mono" href="${escapeHtml(url)}" target="_blank" rel="noopener">${escapeHtml(shortAddr(row.tx))}</a>`;
  }
  tr.innerHTML = `<td class="mono">${escapeHtml(shortAddr(row.address))}</td>
    <td><span class="status-pill status-${badge.kind}">${escapeHtml(badge.label)}</span></td>
    <td class="muted cell-clip">${escapeHtml(row.detail || "")}</td>
    <td>${txCell}</td>
    <td class="error cell-clip">${escapeHtml(row.error || "")}</td>`;
  return tr;
}

function renderMintTable() {
  const wrap = $("mint-table-wrap");
  const tb = $("mint-tbody");
  if (!tb) return;
  bindVirtualScroll("mint-table-wrap", scheduleMintTableRender);
  if (!mintRowOrder.length) {
    tb.innerHTML = "";
    return;
  }
  paintVirtualTbody(wrap, tb, mintRowOrder.length, paintMintRow);
}

function scheduleMintTableRender() {
  if (mintRenderScheduled) return;
  mintRenderScheduled = true;
  requestAnimationFrame(() => {
    mintRenderScheduled = false;
    renderMintTable();
  });
}

/**
 * Classify mint log line → { kind, emoji } for color + scanability.
 * Kinds: start|auth|ok|fail|wait|phase|gas|proxy|sim|send|info|warn|export
 */
function classifyMintLogLine(text) {
  const s = String(text || "");
  const l = s.toLowerCase();

  // Success summaries first — "0 fail" must NOT look like an error
  if (
    /^\[?done\]?/i.test(l.trim()) ||
    l.includes("done:") ||
    l.includes("task «") && l.includes("finished") ||
    l.includes("task \"") && l.includes("finished") ||
    /\b\d+\s*ok\b/.test(l) && /\b0\s*fail/.test(l)
  ) {
    // real failure summary: "0 ok · 5 fail" or "Done: 0 ok"
    if (/\b0\s*ok\b/.test(l) && /\b[1-9]\d*\s*fail/.test(l)) {
      return { kind: "fail", emoji: "❌" };
    }
    if (/\b[1-9]\d*\s*ok\b/.test(l) || l.includes("finished") || l.includes("0 fail")) {
      return { kind: "ok", emoji: "✅" };
    }
  }

  if (
    l.includes("error:") ||
    l.includes(" error") ||
    l.startsWith("error") ||
    l.includes("failed to") ||
    l.includes("pre-flight fail") ||
    l.includes("preflight fail") ||
    l.includes("low balance") ||
    l.includes("insufficient") ||
    l.includes("abort") ||
    l.includes("cannot start") ||
    l.includes("blocked") ||
    // "fail" only if not a zero-fail summary
    (/\bfail(ed|ure)?\b/.test(l) && !/\b0\s*fail/.test(l) && !/\bok\b.*\bfail/.test(l))
  ) {
    return { kind: "fail", emoji: "❌" };
  }
  if (
    l.includes("warn") ||
    l.includes("retry") ||
    l.includes("re-auth") ||
    l.includes("401") ||
    l.includes("down") ||
    l.includes("stop") ||
    l.includes("cancel") ||
    l.includes("queued")
  ) {
    return { kind: "warn", emoji: "⚠️" };
  }
  if (
    l.includes("waiting for phase") ||
    l.includes("until phase open") ||
    l.includes("scheduled mint")
  ) {
    return { kind: "wait", emoji: "⏳" };
  }
  if (
    l.includes("authenticat") ||
    l.includes("auth:") ||
    l.includes("siwe") ||
    l.includes("cached ok") ||
    (/\bok\s*\(\d+\s*ms\)/i.test(s) && (l.includes("0x") || l.includes("via ")))
  ) {
    // wallet auth success lines → green check; "Authenticating..." stays key
    if (/\bok\b/i.test(s) && !l.includes("authenticating")) {
      return { kind: "ok", emoji: "✅" };
    }
    return { kind: "auth", emoji: "🔑" };
  }
  if (
    l.includes("pre-flight ok") ||
    l.includes("preflight ok") ||
    l.includes("estimate_gas ok") ||
    l.includes("sim ") ||
    l.includes("pre-flight") ||
    l.includes("est gas")
  ) {
    return { kind: "sim", emoji: "🧪" };
  }
  if (
    l.includes("tx") &&
    (l.includes("sent") || l.includes("hash") || l.includes("0x")) &&
    !l.includes("failed")
  ) {
    // only if looks like send path
    if (l.includes("sent") || l.includes("broadcast") || l.includes("confirm")) {
      return { kind: "send", emoji: "🚀" };
    }
  }
  if (l.includes("--- checklist") || l.trim().startsWith("✓") || l.trim().startsWith("!")) {
    return l.trim().startsWith("!")
      ? { kind: "warn", emoji: "⚠️" }
      : { kind: "ok", emoji: "✅" };
  }
  if (
    l.includes("confirm") ||
    l.includes("finished") ||
    (l.includes("auth:") && l.includes("ok"))
  ) {
    return { kind: "ok", emoji: "✅" };
  }
  if (
    l.includes("proxy") ||
    l.includes("probing proxies")
  ) {
    return { kind: "proxy", emoji: "🌐" };
  }
  if (
    l.includes("balance") ||
    l.includes("nonce") ||
    l.includes("gas:") ||
    l.includes("priority") ||
    l.includes("fee")
  ) {
    return { kind: "gas", emoji: "⛽" };
  }
  if (
    l.includes("phase") ||
    l.includes("collection") ||
    l.includes("drop type") ||
    l.includes("nft contract") ||
    l.includes("recommended") ||
    l.includes("selected:") ||
    l.includes("mint quantity") ||
    l.includes("fetching collection") ||
    l.includes("re-fetching")
  ) {
    return { kind: "phase", emoji: "📦" };
  }
  if (l.includes("exported") || l.includes("export")) {
    return { kind: "export", emoji: "💾" };
  }
  if (
    l.includes("starting task") ||
    l.includes("task wallets") ||
    l.includes("live (sim")
  ) {
    return { kind: "start", emoji: "▶️" };
  }
  if (l.includes(" ok") || l.includes(") ok") || l.endsWith(" ok") || l.includes(" ok (")) {
    return { kind: "ok", emoji: "✅" };
  }
  return { kind: "info", emoji: "ℹ️" };
}

function appendMintLog(line) {
  const el = $("mint-log");
  if (!el) return;
  const ts = new Date().toLocaleTimeString();
  const text = String(line ?? "");
  const { kind, emoji } = classifyMintLogLine(text);
  const row = document.createElement("div");
  row.className = "mint-log-line mint-log-" + kind;
  row.innerHTML = `<span class="mint-log-ts">[${escapeHtml(ts)}]</span> <span class="mint-log-emoji">${emoji}</span> <span class="mint-log-msg">${escapeHtml(text)}</span>`;
  el.appendChild(row);
  // keep last ~800 lines for memory
  while (el.childElementCount > 800) {
    el.removeChild(el.firstChild);
  }
  el.scrollTop = el.scrollHeight;
}

function clearMintLog() {
  const el = $("mint-log");
  if (el) el.innerHTML = "";
}

function setMintPhaseBanner(phase, label) {
  const banner = $("mint-phase-banner");
  const emojiEl = $("mint-phase-emoji");
  const textEl = $("mint-phase-text");
  if (!banner || !textEl) return;
  const p = String(phase || "idle").toLowerCase();
  const map = {
    idle: "⏸️",
    prep: "🔧",
    auth: "🔑",
    wait: "⏳",
    fire: "🚀",
    confirm: "📡",
    done: "✅",
    error: "❌",
  };
  banner.className = "mint-phase-banner mint-phase-" + (map[p] ? p : "idle");
  if (emojiEl) emojiEl.textContent = map[p] || "ℹ️";
  textEl.textContent = label || t("tasks.phaseIdle") || "Ready";
}

function onMintEvent(ev) {
  const p = ev.payload || {};
  if (p.phase || p.phaseLabel) {
    setMintPhaseBanner(p.phase, p.phaseLabel || p.message || "");
    // phase events may also carry a mirrored log message
    if (p.message) appendMintLog(p.message);
    return;
  }
  if (p.message) {
    appendMintLog(p.message);
    // Heuristic banner if core didn't send phase (older paths)
    const l = String(p.message).toLowerCase();
    if (l.includes("waiting for phase") || l.includes("until phase open")) {
      setMintPhaseBanner("wait", p.message);
    } else if (l.includes("phase open") || l.includes("send ok") || l.includes("sent t+")) {
      setMintPhaseBanner("fire", p.message);
    } else if (l.includes("confirmed") || l.includes("done:")) {
      setMintPhaseBanner(
        l.includes("done:") ? "done" : "confirm",
        p.message
      );
    }
    return;
  }
  if (p.address) {
    const row = ensureMintRow(p.address);
    if (p.status) row.status = p.status;
    if (p.detail != null) row.detail = p.detail;
    if (p.txHash) row.tx = p.txHash;
    if (p.error != null) row.error = p.error;
    scheduleMintTableRender();
    const st = String(p.status || "").toUpperCase();
    if (st.includes("CONFIRM")) {
      setMintPhaseBanner("confirm", "Confirmations coming in…");
    } else if (st.includes("SENT")) {
      setMintPhaseBanner("fire", "Tx sent — waiting for block…");
    }
  }
}

let mintUnlisten = null;

async function setupMintListener() {
  try {
    const { listen } = window.__TAURI__.event;
    if (mintUnlisten) {
      mintUnlisten();
      mintUnlisten = null;
    }
    mintUnlisten = await listen("mint-event", onMintEvent);
  } catch (e) {
    console.warn("mint listen failed", e);
  }
}

function updateTaskGroupStats(ok, fail, total, label) {
  const elT = $("task-stat-total");
  const elO = $("task-stat-ok");
  const elF = $("task-stat-fail");
  if (elT) elT.textContent = total != null ? String(total) : "—";
  if (elO) elO.textContent = String(ok ?? 0);
  if (elF) elF.textContent = String(fail ?? 0);
  const name = $("task-group-name");
  if (name && label) name.textContent = label;
}

function applyMintSummary(summary) {
  lastMintSummary = summary;
  lastMintChain = summary.chain || lastMintChain;
  if (summary.wallets) {
    for (const w of summary.wallets) {
      const row = ensureMintRow(w.address);
      row.status = w.status || row.status;
      row.tx = w.txHash || w.tx_hash || row.tx;
      row.error = w.error || "";
      row.detail = w.gasUsed != null ? `gas=${w.gasUsed}` : row.detail;
    }
    scheduleMintTableRender();
  }
  const ok = summary.confirmed ?? 0;
  const fail = summary.failed ?? 0;
  const total = summary.wallets?.length ?? ok + fail;
  updateTaskGroupStats(ok, fail, total);
  $("mint-summary").textContent = `Done: ${ok} ok · ${fail} failed · ${summary.elapsedMs}ms · ${summary.phase} · ${summary.chain}`;
  const runStats = $("mint-run-stats");
  if (runStats) {
    runStats.innerHTML = `<span class="ok">${ok} ok</span> · <span class="fail">${fail} fail</span>`;
  }
  if (summary.exportJson) appendMintLog("Exported: " + summary.exportJson);
  mintRunHistory.unshift({
    at: new Date().toISOString(),
    slug: summary.slug,
    phase: summary.phase,
    chain: summary.chain,
    confirmed: ok,
    failed: fail,
    elapsedMs: summary.elapsedMs,
    dryRun: summary.dryRun,
    exportJson: summary.exportJson,
    exportCsv: summary.exportCsv,
  });
  if (mintRunHistory.length > 100) mintRunHistory.length = 100;
  scheduleSaveRunsHistory();
  renderNftsPage();
  const home = $("home-last-mint");
  if (home) {
    home.dataset.hasRun = "1";
    home.textContent = [
      `${summary.slug} · ${summary.phase} · ${summary.chain}`,
      `ok=${ok} fail=${fail} dry=${summary.dryRun} ${summary.elapsedMs}ms`,
      summary.exportJson || "",
      summary.exportCsv || "",
    ]
      .filter(Boolean)
      .join("\n");
  }
}

function renderNftsPage() {
  const hist = $("run-history-tbody");
  if (hist) {
    if (!mintRunHistory.length) {
      hist.innerHTML = `<tr><td colspan="6" class="muted">${escapeHtml(
        t("nfts.historyEmpty") || "No runs yet — finish a mint task."
      )}</td></tr>`;
    } else {
      hist.innerHTML = "";
      for (const r of mintRunHistory) {
        const tr = document.createElement("tr");
        const when = r.at ? new Date(r.at).toLocaleString() : "—";
        const dry = r.dryRun ? " · dry" : "";
        tr.innerHTML = `<td class="muted small">${escapeHtml(when)}</td>
          <td class="mono" title="${escapeHtml(r.exportJson || "")}">${escapeHtml(r.slug || "—")}</td>
          <td>${escapeHtml(r.phase || "—")}${dry}</td>
          <td class="ok">${r.confirmed ?? 0}</td>
          <td class="fail">${r.failed ?? 0}</td>
          <td class="muted">${r.elapsedMs ?? "—"}</td>`;
        hist.appendChild(tr);
      }
    }
  }
  const tb = $("nfts-tbody");
  const exp = $("nfts-export");
  if (!tb) return;
  if (!lastMintSummary || !(lastMintSummary.wallets || []).length) {
    tb.innerHTML = `<tr><td colspan="5" class="muted">No results yet — run a mint from Tasks.</td></tr>`;
    if (exp) exp.textContent = "Export paths appear after a mint with export enabled.";
    return;
  }
  tb.innerHTML = "";
  const chain = lastMintSummary.chain || lastMintChain;
  for (const w of lastMintSummary.wallets) {
    const tr = document.createElement("tr");
    const st = String(w.status || "");
    const cls =
      st.includes("OK") || st.includes("CONFIRM") || st.includes("DRY")
        ? "ok"
        : st.includes("FAIL")
          ? "error"
          : "";
    const tx = w.txHash || w.tx_hash || "";
    let txCell = "—";
    if (tx) {
      const url = explorerTxUrlLocal(chain, tx);
      txCell = `<a class="mono" href="${escapeHtml(url)}" target="_blank" rel="noopener">${escapeHtml(shortAddr(tx))}</a>`;
    }
    tr.innerHTML = `
      <td class="mono">${escapeHtml(shortAddr(w.address))}</td>
      <td class="${cls}">${escapeHtml(st)}</td>
      <td>${txCell}</td>
      <td>${w.gasUsed ?? w.gas_used ?? "—"}</td>
      <td class="error">${escapeHtml(w.error || "")}</td>`;
    tb.appendChild(tr);
  }
  if (exp) {
    const parts = [];
    if (lastMintSummary.exportJson) parts.push(lastMintSummary.exportJson);
    if (lastMintSummary.exportCsv) parts.push(lastMintSummary.exportCsv);
    exp.textContent = parts.length ? parts.join("\n") : "No export paths (enable Export in Settings).";
  }
}

$("btn-open-results")?.addEventListener("click", async () => {
  try {
    const p = await invoke("open_results_folder");
    showToast("Opened " + p, "ok");
  } catch (e) {
    showToast(String(e), "err");
  }
});

$("btn-open-logs")?.addEventListener("click", async () => {
  try {
    const p = await invoke("open_logs_folder");
    showToast("Opened " + p, "ok");
  } catch (e) {
    showToast(String(e), "err");
  }
});

function setMintUiRunning(running) {
  const stop = $("btn-mint-stop");
  if (stop) {
    stop.disabled = !running && !mintStopping;
    if (mintStopping) stop.textContent = t("tasks.stopping") || "Stopping…";
    else stop.textContent = t("tasks.stop") || "Stop";
  }
  const lab = $("tasks-selected-label");
  if (lab) {
    lab.textContent = mintStopping
      ? t("tasks.stopping") || "Stopping…"
      : running
        ? t("tasks.running")
        : t("tasks.ready");
    lab.classList.toggle("is-running", running || mintStopping);
  }
  updateQueueBar();
  renderTaskList();
}

$("btn-mint-stop")?.addEventListener("click", async () => {
  try {
    mintStopping = true;
    setMintUiRunning(true);
    setMintPhaseBanner("error", t("tasks.stopping") || "Stopping…");
    const msg = await invoke("cancel_mint");
    appendMintLog(msg);
    showToast(msg || "Stopping…", "warn");
  } catch (e) {
    appendMintLog("Stop failed: " + e);
    showToast(String(e), "err");
    mintStopping = false;
    setMintUiRunning(!!activeTaskId);
  }
});

$("btn-warm-auth")?.addEventListener("click", async () => {
  if (activeTaskId) {
    showToast(t("tasks.busy") || "Mint running", "warn");
    return;
  }
  const btn = $("btn-warm-auth");
  if (btn) btn.disabled = true;
  setMintPhaseBanner("auth", "Warm auth — OpenSea SIWE…");
  appendMintLog("Warm auth starting…");
  try {
    // Prefer wallets from active/selected tasks if any ready; else all
    let addrs = null;
    const ready = mintTasks.find((t) => t.status === "ready" || t.status === "done");
    if (ready?.wallets?.length) addrs = ready.wallets;
    const rows = await invoke("warm_auth", {
      input: { walletAddresses: addrs },
    });
    const ok = (rows || []).filter((r) => r.ok).length;
    const n = (rows || []).length;
    for (const r of rows || []) {
      appendMintLog(
        r.ok
          ? `Warm OK ${shortAddr(r.address)} ${r.latencyMs}ms via ${r.proxy}`
          : `Warm FAIL ${shortAddr(r.address)}: ${r.error || "?"}`
      );
    }
    const msg = (t("tasks.warmAuthOk") || "Warm auth: {ok}/{n} OK")
      .replace("{ok}", String(ok))
      .replace("{n}", String(n));
    setMintPhaseBanner(ok === n ? "done" : "error", msg);
    showToast(msg, ok === n ? "ok" : "warn");
    appendMintLog(msg);
  } catch (e) {
    appendMintLog("Warm auth ERROR: " + e);
    setMintPhaseBanner("error", String(e));
    showToast(String(e), "err");
  } finally {
    if (btn) btn.disabled = false;
  }
});

/** Enqueue if busy, else start immediately (no typed CONFIRM). */
function requestStartTask(taskId) {
  const task = mintTasks.find((x) => x.id === taskId);
  if (!task) return;
  if (task.status === "running" || task.status === "queued") return;
  const reasons = computeBlockReasons({ ...task, status: "ready" });
  if (reasons.length) {
    appendMintLog(`Blocked «${task.name}»: ${reasons[0]}`);
    showToast(reasons[0], "warn");
    renderTaskList();
    return;
  }
  if (activeTaskId || queueProcessing || mintStopping) {
    if (taskQueue.includes(taskId)) return;
    // Prefer toast when engine busy (single-flight)
    if (activeTaskId || mintStopping) {
      showToast(t("tasks.busy") || "Mint already running — wait or Stop first", "warn");
    }
    task.status = "queued";
    taskQueue.push(taskId);
    appendMintLog(`Queued «${task.name}» (position ${taskQueue.length})`);
    renderTaskList();
    return;
  }
  startMintTask(taskId, { fromQueue: false });
}

async function processQueue() {
  if (queueProcessing || activeTaskId) return;
  if (!taskQueue.length) {
    renderTaskList();
    return;
  }
  queueProcessing = true;
  try {
    while (taskQueue.length) {
      const nextId = taskQueue.shift();
      const task = mintTasks.find((x) => x.id === nextId);
      if (!task) continue;
      task.status = "ready";
      // Do not re-enter processQueue from startMintTask
      await startMintTask(nextId, { fromQueue: true });
    }
  } finally {
    queueProcessing = false;
    renderTaskList();
  }
}

/**
 * @param {string} taskId
 * @param {{ fromQueue?: boolean }} opts
 */
async function startMintTask(taskId, opts = {}) {
  const fromQueue = !!opts.fromQueue;
  const task = mintTasks.find((x) => x.id === taskId);
  if (!task) return;
  if (activeTaskId) {
    if (!fromQueue) requestStartTask(taskId);
    return;
  }
  const reasons = computeBlockReasons({ ...task, status: "ready" });
  if (reasons.length) {
    appendMintLog(`Blocked «${task.name}»: ${reasons[0]}`);
    task.status = "ready";
    renderTaskList();
    return;
  }

  // Optional: drop low-balance wallets before Start
  let runWallets = [...(task.wallets || [])];
  if (task.filterBalance !== false && runWallets.length) {
    try {
      appendMintLog("Balance filter: checking…");
      const rows = await invoke("wallet_balances", {
        input: { walletAddresses: runWallets },
      });
      const funded = new Set(
        rows.filter((r) => r.ok).map((r) => addrKey(r.address))
      );
      const before = runWallets.length;
      runWallets = runWallets.filter((a) => funded.has(addrKey(a)));
      const skipped = before - runWallets.length;
      if (skipped > 0) {
        appendMintLog(
          `Balance filter: ${runWallets.length}/${before} funded (skipped ${skipped})`
        );
      } else {
        appendMintLog(`Balance filter: all ${before} funded`);
      }
      if (!runWallets.length) {
        appendMintLog("No funded wallets — abort start");
        task.status = "error";
        renderTaskList();
        if (!fromQueue) setTimeout(() => processQueue(), 0);
        return;
      }
    } catch (e) {
      appendMintLog("Balance filter failed (continuing all): " + e);
    }
  }

  // Start always live: sim → if OK, send tx → wait for on-chain confirm.
  // No typed CONFIRM — one click starts immediately.
  const gasLabel =
    task.gasMode === "manual" && task.gasLimit
      ? `manual ${task.gasLimit}`
      : "auto";
  const prio = (task.priorityFeeGwei || "").trim() || "auto";
  const gasLimit = task.gasMode === "manual" ? task.gasLimit || 250000 : 0;

  // Build proxy overrides for selected wallets from wallet_meta
  if (!walletMetaLoaded) await loadWalletMeta();
  const proxyOverrides = {};
  for (const a of runWallets) {
    const k = addrKey(a);
    if (walletProxyMap[k] != null) proxyOverrides[a] = Number(walletProxyMap[k]);
  }

  activeTaskId = task.id;
  task.status = "running";
  task.updatedAt = nowMs();
  mintStopping = false;
  mintRows.clear();
  mintRowOrder = [];
  clearMintLog();
  $("mint-summary").textContent = "";
  setMintPhaseBanner("prep", `Starting «${task.name}»…`);
  updateTaskGroupStats(0, 0, runWallets.length, task.name);
  scheduleMintTableRender();
  appendMintLog(`Starting task «${task.name}» LIVE (sim → tx if OK, gas=${gasLabel}, prio=${prio})`);
  setMintUiRunning(true);
  // User gesture path — unlock WebAudio so first confirm can chime in UI too.
  ensureMintAudio();
  await setupMintListener();
  try {
    const summary = await invoke("run_mint", {
      input: {
        slug: task.slug,
        quantity: task.quantity,
        dryRun: false,
        phaseIndex: task.phaseIndex,
        walletAddresses: runWallets,
        chainOverride: task.chainOverride === "auto" ? null : task.chainOverride,
        gasLimit,
        priorityFeeGwei: (task.priorityFeeGwei || "").trim() || null,
        atTime: (task.atTime || "").trim() || null,
        walletQuantities: task.walletQuantities || null,
        skipEstimateOnOpen: !!task.skipEstimateOnOpen,
        proxyOverrides:
          Object.keys(proxyOverrides).length > 0 ? proxyOverrides : null,
      },
    });
    applyMintSummary(summary);
    updateTaskGroupStats(
      summary.confirmed,
      summary.failed,
      summary.wallets?.length,
      task.name
    );
    task.status = "done";
    task.updatedAt = nowMs();
    appendMintLog(`Task «${task.name}» finished`);
  } catch (e) {
    task.status = "error";
    task.updatedAt = nowMs();
    const es = String(e);
    appendMintLog("ERROR: " + es);
    $("mint-summary").textContent = es;
    if (es.toLowerCase().includes("settings") || es.toLowerCase().includes("chain mismatch")) {
      showToast(es, "err");
      const open = await openConfirmModal({
        title: "RPC / chain",
        body: es,
        lines: ["Open Settings to fix Connection?"],
        requireWord: null,
        okLabel: "Open Settings",
      });
      if (open) navigate("settings");
    } else if (es.toLowerCase().includes("401") || es.toLowerCase().includes("re-auth")) {
      showToast(es, "warn");
    } else {
      showToast(es, "err");
    }
  } finally {
    activeTaskId = null;
    mintStopping = false;
    schedulePersistTasks();
    setMintUiRunning(false);
    if (!fromQueue) setTimeout(() => processQueue(), 50);
  }
}
