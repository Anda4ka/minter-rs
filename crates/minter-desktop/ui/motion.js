/**
 * motion.js — transitions.dev-inspired motion layer for MINTER.
 *
 * DESIGN CONTRACT: this file is *additive and defensive*. It never touches
 * app.js internals or mint logic. It only observes the DOM that app.js
 * renders and layers micro-interactions on top:
 *   - number pop-in on live counters
 *   - blur-rise text swap on the mint phase text
 *   - success check + confetti when a run reaches "done"; shake on "error"
 *   - a sliding underline that follows the active tab / mode chip / filter
 *
 * Everything is wrapped in try/catch and gated by prefers-reduced-motion, so a
 * failure here can never break the operator console. The heavy lifting (the
 * actual mint / sniper / sweep logic) stays entirely in app.js.
 */

const RM = () => window.matchMedia("(prefers-reduced-motion: reduce)").matches;
const SPRING = "cubic-bezier(0.34, 1.56, 0.64, 1)";
const OUT = "cubic-bezier(0.22, 0.61, 0.36, 1)";
const $ = (id) => document.getElementById(id);

/* Elements whose sliding underline needs re-measuring when a page is shown. */
const repositioners = [];
function repositionAll() {
  repositioners.forEach((fn) => requestAnimationFrame(fn));
}

/* ---------------------------------------------------------------- numbers */
/* Loop-safe: we remember the last plain value per element. When our own
 * digit-span write fires the observer, the plain text still equals the stored
 * value, so we skip — no infinite loop. */
const numLast = new WeakMap();

function popNumber(el) {
  if (!el) return;
  const text = (el.textContent || "").trim();
  if (numLast.get(el) === text) return; // unchanged or our own write
  numLast.set(el, text);
  if (RM() || text === "") return;
  el.textContent = "";
  const chars = [...text];
  chars.forEach((ch, i) => {
    const s = document.createElement("span");
    s.className = "mx-digit";
    s.textContent = ch;
    el.appendChild(s);
    try {
      s.animate(
        [
          { opacity: 0, transform: "translateY(0.5em)", filter: "blur(6px)" },
          { opacity: 1, transform: "none", filter: "blur(0)" },
        ],
        { duration: 400, delay: i * 45, easing: SPRING, fill: "backwards" }
      );
    } catch (e) {
      /* WAAPI unsupported — leave the digit static */
    }
  });
  numLast.set(el, (el.textContent || "").trim()); // == text → next fire skips
}

function watchNumbers() {
  const ids = [
    "mc-ok",
    "mc-fail",
    "mc-sent",
    "mc-wait",
    "mc-total",
    "task-stat-total",
    "task-stat-ok",
    "task-stat-fail",
  ];
  ids.forEach((id) => {
    const el = $(id);
    if (!el) return;
    numLast.set(el, (el.textContent || "").trim());
    const mo = new MutationObserver(() => popNumber(el));
    mo.observe(el, { childList: true, characterData: true, subtree: true });
  });
}

/* ------------------------------------------------------------ phase text */
function watchPhaseText() {
  ["mint-phase-text", "mc-phase-label"].forEach((id) => {
    const el = $(id);
    if (!el) return;
    let last = el.textContent;
    const mo = new MutationObserver(() => {
      const cur = el.textContent;
      if (cur === last) return;
      last = cur;
      if (RM()) return;
      try {
        el.animate(
          [
            { opacity: 0, transform: "translateY(60%)", filter: "blur(7px)" },
            { opacity: 1, transform: "none", filter: "blur(0)" },
          ],
          { duration: 320, easing: SPRING }
        );
      } catch (e) {
        /* ignore */
      }
    });
    mo.observe(el, { childList: true, characterData: true, subtree: true });
  });
}

/* ------------------------------------------- phase banner → success / shake */
function watchPhaseBanner() {
  const banner = $("mint-phase-banner");
  if (!banner) return;
  let last = banner.className;
  const mo = new MutationObserver(() => {
    const cls = banner.className;
    if (cls === last) return;
    last = cls;
    if (RM()) return;
    if (cls.includes("mint-phase-done")) {
      successBurst(banner);
      confettiFrom(banner);
    } else if (cls.includes("mint-phase-error")) {
      shake(banner);
    }
  });
  mo.observe(banner, { attributes: true, attributeFilter: ["class"] });
}

/* ------------------------------------------------------- sliding underline */
function setupUnderline(container, btnSel, isActive) {
  if (!container || container.querySelector(":scope > .mx-underline")) return;
  const bar = document.createElement("span");
  bar.className = "mx-underline";
  bar.style.opacity = "0";
  container.appendChild(bar);
  const move = () => {
    const btns = [...container.querySelectorAll(btnSel)];
    if (!btns.length) return;
    const active = btns.find(isActive);
    if (!active || !active.offsetWidth) {
      bar.style.opacity = "0";
      return;
    }
    bar.style.opacity = "1";
    bar.style.width = active.offsetWidth + "px";
    bar.style.transform = "translateX(" + active.offsetLeft + "px)";
  };
  container.addEventListener("click", () => requestAnimationFrame(move));
  const mo = new MutationObserver(() => move());
  container.querySelectorAll(btnSel).forEach((b) =>
    mo.observe(b, {
      attributes: true,
      attributeFilter: ["class", "aria-selected", "aria-pressed"],
    })
  );
  repositioners.push(move);
  requestAnimationFrame(move);
}

function watchTabs() {
  document
    .querySelectorAll(".seg-toggle")
    .forEach((c) =>
      setupUnderline(
        c,
        ".seg-btn",
        (b) =>
          b.classList.contains("is-active") ||
          b.getAttribute("aria-selected") === "true"
      )
    );
  document
    .querySelectorAll(".raw-mode-row")
    .forEach((c) =>
      setupUnderline(c, ".raw-mode-chip", (b) => b.classList.contains("active"))
    );
  document
    .querySelectorAll(".wallet-filters")
    .forEach((c) =>
      setupUnderline(
        c,
        ".filter-chip",
        (b) =>
          b.classList.contains("is-on") ||
          b.getAttribute("aria-pressed") === "true"
      )
    );
}

/* ------------------------------------------------------------- confetti/fx */
function confettiFrom(el) {
  if (RM() || !el) return;
  const r = el.getBoundingClientRect();
  const cx = r.left + r.width / 2;
  const cy = r.top + r.height / 2;
  const cols = ["#6d5dfc", "#8b7bff", "#5fd39a", "#f5c84c", "#5b8def", "#ff8fb0"];
  for (let i = 0; i < 28; i++) {
    const p = document.createElement("div");
    p.className = "mx-confetti";
    p.style.background = cols[i % cols.length];
    p.style.left = cx + "px";
    p.style.top = cy + "px";
    document.body.appendChild(p);
    const a = Math.random() * Math.PI * 2;
    const d = 60 + Math.random() * 140;
    const dx = Math.cos(a) * d;
    const dy = Math.sin(a) * d;
    try {
      p.animate(
        [
          { transform: "translate(0,0) rotate(0) scale(1)", opacity: 1 },
          {
            transform:
              "translate(" +
              dx +
              "px," +
              (dy + 240) +
              "px) rotate(" +
              (Math.random() * 720 - 360) +
              "deg) scale(.5)",
            opacity: 0,
          },
        ],
        {
          duration: 1000 + Math.random() * 600,
          easing: "cubic-bezier(0.2,0.6,0.2,1)",
          fill: "forwards",
        }
      ).onfinish = () => p.remove();
    } catch (e) {
      p.remove();
    }
  }
}

function successBurst(el) {
  if (RM() || !el) return;
  const r = el.getBoundingClientRect();
  const wrap = document.createElement("div");
  wrap.className = "mx-success";
  wrap.style.left = r.right - 56 + "px";
  wrap.style.top = r.top + r.height / 2 - 20 + "px";
  wrap.innerHTML =
    '<svg width="40" height="40" viewBox="0 0 44 44">' +
    '<circle cx="22" cy="22" r="20" fill="none" stroke="#5fd39a" stroke-width="2.5" stroke-dasharray="126" stroke-dashoffset="126"/>' +
    '<path d="M13 23l6 6 12-13" fill="none" stroke="#5fd39a" stroke-width="3" stroke-linecap="round" stroke-linejoin="round" stroke-dasharray="40" stroke-dashoffset="40"/>' +
    "</svg>";
  document.body.appendChild(wrap);
  const c = wrap.querySelector("circle");
  const pth = wrap.querySelector("path");
  try {
    c.animate([{ strokeDashoffset: 126 }, { strokeDashoffset: 0 }], {
      duration: 450,
      easing: OUT,
      fill: "forwards",
    });
    pth.animate([{ strokeDashoffset: 40 }, { strokeDashoffset: 0 }], {
      duration: 400,
      delay: 350,
      easing: OUT,
      fill: "forwards",
    });
    wrap.animate(
      [
        { opacity: 0, transform: "scale(.6)" },
        { opacity: 1, transform: "scale(1)" },
      ],
      { duration: 400, easing: SPRING }
    );
    setTimeout(() => {
      wrap.animate([{ opacity: 1 }, { opacity: 0 }], {
        duration: 300,
        fill: "forwards",
      }).onfinish = () => wrap.remove();
    }, 2200);
  } catch (e) {
    wrap.remove();
  }
}

function shake(el) {
  if (RM() || !el) return;
  el.classList.remove("mx-shake");
  void el.offsetWidth;
  el.classList.add("mx-shake");
  setTimeout(() => el.classList.remove("mx-shake"), 600);
}

/* -------------------------------------------------------------------- init */
function init() {
  try {
    watchNumbers();
  } catch (e) {
    console.warn("[motion] numbers", e);
  }
  try {
    watchPhaseText();
  } catch (e) {
    console.warn("[motion] phase text", e);
  }
  try {
    watchPhaseBanner();
  } catch (e) {
    console.warn("[motion] phase banner", e);
  }
  try {
    watchTabs();
  } catch (e) {
    console.warn("[motion] tabs", e);
  }

  // Re-measure underlines when a page becomes visible (offsets are 0 while
  // the page is display:none) and on resize.
  try {
    document.querySelectorAll(".nav-item[data-page]").forEach((btn) =>
      btn.addEventListener("click", () => setTimeout(repositionAll, 90))
    );
    document.querySelectorAll("[data-goto]").forEach((btn) =>
      btn.addEventListener("click", () => setTimeout(repositionAll, 90))
    );
    window.addEventListener("resize", repositionAll);
  } catch (e) {
    /* ignore */
  }
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", init);
} else {
  init();
}
