// Orcha Web UI client — vanilla JS, no build step, no framework.
//
// Routes by window.location.pathname:
//   /                            → 任务列表 + 状态统计
//   /tasks/:id                   → 任务详情（tabs: Artifacts / History / Memory）
//   /tasks/:id/history           → 全屏 round 时间线
//
// 所有数据通过 /api/* JSON 端点拉取。CSS 已内置 staggered reveal / pulse / spinner 动画。

(function () {
  "use strict";

  // ── DOM helpers ──────────────────────────────────────
  function el(tag, attrs, ...children) {
    const e = document.createElement(tag);
    if (attrs) {
      for (const k in attrs) {
        const v = attrs[k];
        if (v === null || v === undefined) continue;
        if (k === "class") e.className = v;
        else if (k === "html") e.innerHTML = v;
        else if (k.startsWith("on") && typeof v === "function") {
          e.addEventListener(k.slice(2).toLowerCase(), v);
        } else {
          e.setAttribute(k, v);
        }
      }
    }
    for (const c of children) {
      if (c === null || c === undefined || c === false) continue;
      e.appendChild(typeof c === "string" || typeof c === "number" ? document.createTextNode(String(c)) : c);
    }
    return e;
  }

  async function fetchJson(url) {
    const r = await fetch(url, { headers: { Accept: "application/json" } });
    if (!r.ok) throw new Error(`${url} → HTTP ${r.status}`);
    return r.json();
  }

  function escapeHtml(s) {
    if (s === null || s === undefined) return "";
    return String(s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  function fmtTime(iso) {
    if (!iso) return "—";
    const d = new Date(iso);
    if (isNaN(d.getTime())) return iso;
    const pad = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }

  function durationMs(started, finished) {
    const a = new Date(started).getTime();
    const b = new Date(finished).getTime();
    if (isNaN(a) || isNaN(b) || b < a) return null;
    return b - a;
  }

  function fmtDuration(ms) {
    if (ms === null) return "—";
    if (ms < 1000) return `${ms}ms`;
    const s = ms / 1000;
    if (s < 60) return `${s.toFixed(1)}s`;
    const m = Math.floor(s / 60);
    const rem = Math.floor(s % 60);
    return `${m}m${rem}s`;
  }

  // ── Shared chrome ────────────────────────────────────
  function topbar() {
    return el(
      "header",
      { class: "topbar" },
      el(
        "div",
        { class: "brand" },
        el("span", { class: "dot" }),
        el("span", { class: "name" }, "orcha"),
        el("span", { class: "tag" }, "console")
      ),
      el("nav", { class: "nav" }, el("a", { href: "/" }, "Tasks"))
    );
  }

  function statusPill(status) {
    const s = (status || "").toLowerCase();
    return el("span", { class: `pill ${s}` }, status || "—");
  }

  function errorBox(e) {
    return el("div", { class: "error" }, `Error: ${escapeHtml(e.message || String(e))}`);
  }

  // ── Index page: task list + stats ────────────────────
  async function renderIndex(app) {
    app.innerHTML = "";
    app.appendChild(topbar());
    const main = el("main", { class: "page-fade" });
    app.appendChild(main);

    main.appendChild(
      el(
        "div",
        { class: "page-head" },
        el("h1", {}, "Tasks"),
        el("div", { class: "sub" }, "All tasks across this Orcha home.")
      )
    );

    const statsHost = el("div", { class: "stats loading" }, "Loading");
    main.appendChild(statsHost);
    const listHost = el("ul", { class: "task-list" });
    main.appendChild(listHost);

    try {
      const [stats, tasks] = await Promise.all([
        fetchJson("/api/tasks/summary").catch(() => null),
        fetchJson("/api/tasks"),
      ]);

      if (stats) {
        statsHost.classList.remove("loading");
        statsHost.innerHTML = "";
        const cells = [
          ["", stats.total, "Total"],
          ["pending", stats.pending, "Pending"],
          ["running", stats.running, "Running"],
          ["blocked", stats.blocked, "Blocked"],
          ["done", stats.done, "Done"],
          ["failed", stats.failed, "Failed"],
        ];
        for (const [cls, n, label] of cells) {
          statsHost.appendChild(
            el(
              "div",
              { class: `stat ${cls}` },
              el("div", { class: "n" }, String(n)),
              el("div", { class: "l" }, label)
            )
          );
        }
      } else {
        statsHost.remove();
      }

      listHost.innerHTML = "";
      if (!tasks || tasks.length === 0) {
        main.appendChild(
          el(
            "div",
            { class: "empty" },
            el("div", { class: "glyph" }, "∅"),
            el("div", { class: "hint" }, "暂无任务。先跑一个："),
            el("div", { class: "cmd" }, 'orcha fix "创建 hello.py 输出 hello"')
          )
        );
        return;
      }
      for (const t of tasks) {
        listHost.appendChild(
          el(
            "li",
            { class: "task-row" },
            el(
              "a",
              { href: `/tasks/${encodeURIComponent(t.id)}` },
              el("div", { class: "desc" }, t.description || "(no description)"),
              el("div", { class: "id" }, t.id)
            ),
            statusPill(t.status),
            el("div", { class: "time" }, fmtTime(t.created_at))
          )
        );
      }
    } catch (e) {
      listHost.innerHTML = "";
      main.appendChild(errorBox(e));
    }
  }

  // ── Task detail page with tabs ───────────────────────
  async function renderTask(app, taskId) {
    app.innerHTML = "";
    app.appendChild(topbar());
    const main = el("main", { class: "page-fade" });
    app.appendChild(main);

    main.appendChild(el("a", { class: "back", href: "/" }, "← Tasks"));

    const head = el("div", { class: "page-head loading" }, "Loading task");
    main.appendChild(head);

    const tabsHost = el("div", {});
    main.appendChild(tabsHost);

    try {
      const data = await fetchJson(`/api/tasks/${encodeURIComponent(taskId)}`);
      const task = data.task;
      head.classList.remove("loading");
      head.innerHTML = "";
      head.appendChild(el("h1", {}, task.description || "(no description)"));
      head.appendChild(
        el(
          "div",
          { class: "sub" },
          el("span", { class: "mono" }, task.id),
          " · ",
          statusPill(task.status)
        )
      );

      // KV block
      const kv = el("dl", { class: "kv" });
      const rows = [
        ["Task ID", task.id],
        ["Status", task.status],
        ["Created", fmtTime(task.created_at)],
        ["Updated", fmtTime(task.updated_at)],
        ["Version", String(task.version ?? 0)],
        ["History rounds", String(data.history_count ?? 0)],
      ];
      for (const [k, v] of rows) {
        kv.appendChild(el("dt", {}, k));
        kv.appendChild(el("dd", {}, v));
      }
      tabsHost.appendChild(kv);

      // Tabs: Artifacts / History / Memory
      const tabs = el("div", { class: "tabs" });
      const panels = el("div", {});
      tabsHost.appendChild(tabs);
      tabsHost.appendChild(panels);

      const defs = [
        { id: "artifacts", label: "Artifacts" },
        { id: "history", label: "History" },
        { id: "memory", label: "Memory" },
      ];
      const panelEls = {};
      for (const d of defs) {
        const btn = el("button", { class: "tab", type: "button" }, d.label);
        const panel = el("div", { class: "tab-panel" });
        panelEls[d.id] = panel;
        btn.addEventListener("click", () => {
          for (const b of tabs.children) b.classList.remove("active");
          for (const p of panels.children) p.classList.remove("active");
          btn.classList.add("active");
          panel.classList.add("active");
          if (d.id === "history" && !panel.dataset.loaded) {
            loadHistoryPanel(panel, taskId);
            panel.dataset.loaded = "1";
          } else if (d.id === "memory" && !panel.dataset.loaded) {
            loadMemoryPanel(panel, taskId);
            panel.dataset.loaded = "1";
          }
        });
        tabs.appendChild(btn);
        panels.appendChild(panel);
      }

      // Artifacts panel is rendered immediately (data already in hand).
      renderArtifactsPanel(panelEls["artifacts"], data.artifacts || []);
      tabs.children[0].classList.add("active");
      panelEls["artifacts"].classList.add("active");

      // Deep-link to a tab via ?tab=history|memory
      const wanted = new URLSearchParams(window.location.search).get("tab");
      if (wanted && panelEls[wanted]) {
        panelEls[wanted].previousElementSibling; // no-op; trigger click instead
        const idx = defs.findIndex((d) => d.id === wanted);
        if (idx >= 0) tabs.children[idx].click();
      }
    } catch (e) {
      head.classList.remove("loading");
      head.innerHTML = "";
      head.appendChild(el("h1", {}, "Task not found"));
      main.appendChild(errorBox(e));
    }
  }

  function renderArtifactsPanel(panel, artifacts) {
    panel.innerHTML = "";
    const section = el("div", { class: "section" }, el("h2", {}, "Artifacts"));
    panel.appendChild(section);
    if (!artifacts || artifacts.length === 0) {
      section.appendChild(
        el(
          "div",
          { class: "empty" },
          el("div", { class: "hint" }, "尚无 artifact。跑 `orcha fix` 后这里会出现 patch / report。")
        )
      );
      return;
    }
    const ul = el("ul", { class: "artifact-list" });
    for (const a of artifacts) {
      ul.appendChild(
        el(
          "li",
          { class: "artifact" },
          el("span", { class: "type" }, a.type || "ARTIFACT"),
          el("span", { class: "patch" }, a.patch || a.commit_sha || a.url || a.artifact_id),
          el("span", { class: "url" }, a.artifact_id)
        )
      );
    }
    section.appendChild(ul);
  }

  async function loadHistoryPanel(panel, taskId) {
    panel.innerHTML = "";
    panel.appendChild(el("div", { class: "section" }, el("h2", {}, "Round History")));
    const loading = el("div", { class: "loading" }, "Loading");
    panel.appendChild(loading);
    try {
      const records = await fetchJson(`/api/tasks/${encodeURIComponent(taskId)}/history`);
      panel.removeChild(loading);
      panel.appendChild(renderTimeline(records, { withHeader: false }));
      panel.appendChild(
        el(
          "a",
          { class: "back", href: `/tasks/${encodeURIComponent(taskId)}/history`, style: "margin-top:16px;display:inline-flex;" },
          "Open full timeline →"
        )
      );
    } catch (e) {
      panel.removeChild(loading);
      panel.appendChild(errorBox(e));
    }
  }

  async function loadMemoryPanel(panel, taskId) {
    panel.innerHTML = "";
    panel.appendChild(el("div", { class: "section" }, el("h2", {}, "LLM Memory")));
    const loading = el("div", { class: "loading" }, "Loading");
    panel.appendChild(loading);
    try {
      const entries = await fetchJson(`/api/tasks/${encodeURIComponent(taskId)}/memory`);
      panel.removeChild(loading);
      if (!entries || entries.length === 0) {
        panel.appendChild(
          el(
            "div",
            { class: "empty" },
            el("div", { class: "hint" }, "尚无对话记忆。LLM 路径未启用或还未跑过（`orcha fix --llm`）。")
          )
        );
        return;
      }
      const ul = el("ul", { class: "memory-list" });
      for (const m of entries) {
        ul.appendChild(
          el(
            "li",
            { class: "mem" },
            el("div", { class: "round-col" }, `r${m.round}`),
            el("div", { class: `agent ${m.agent || ""}` }, m.agent || "?"),
            el("div", { class: "content" }, m.content || "")
          )
        );
      }
      panel.appendChild(ul);
    } catch (e) {
      panel.removeChild(loading);
      panel.appendChild(errorBox(e));
    }
  }

  // ── History timeline page ─────────────────────────────
  async function renderHistory(app, taskId) {
    app.innerHTML = "";
    app.appendChild(topbar());
    const main = el("main", { class: "page-fade" });
    app.appendChild(main);

    main.appendChild(
      el("a", { class: "back", href: `/tasks/${encodeURIComponent(taskId)}` }, "← Task detail")
    );
    main.appendChild(
      el(
        "div",
        { class: "page-head" },
        el("h1", {}, "Round Timeline"),
        el("div", { class: "sub" }, el("span", { class: "mono" }, taskId))
      )
    );

    const loading = el("div", { class: "loading" }, "Loading");
    main.appendChild(loading);

    try {
      const records = await fetchJson(`/api/tasks/${encodeURIComponent(taskId)}/history`);
      main.removeChild(loading);
      main.appendChild(renderTimeline(records, { withHeader: true }));
    } catch (e) {
      main.removeChild(loading);
      main.appendChild(errorBox(e));
    }
  }

  function renderTimeline(records, opts) {
    const wrap = el("div", { class: "section" });
    if (opts && opts.withHeader) {
      wrap.appendChild(el("h2", {}, `Rounds (${records ? records.length : 0})`));
    }
    if (!records || records.length === 0) {
      wrap.appendChild(
        el("div", { class: "empty" }, el("div", { class: "hint" }, "尚无 round 记录。"))
      );
      return wrap;
    }
    const ul = el("ul", { class: "timeline" });
    for (const r of records) {
      const anyFail = (r.steps || []).some((s) => !s.success);
      const cls = anyFail ? "round fail" : "round ok";
      const head = el(
        "div",
        { class: "head" },
        el("span", { class: "num" }, `Round ${r.round}`),
        el("span", { class: "dur" }, fmtDuration(durationMs(r.started_at, r.finished_at))),
        r.tokens_used ? el("span", { class: "tokens" }, `${r.tokens_used} tok`) : null
      );
      const stepsUl = el("ul", { class: "steps" });
      for (const s of r.steps || []) {
        stepsUl.appendChild(
          el(
            "li",
            { class: `step ${s.success ? "ok" : "fail"}` },
            el("span", { class: "icon" }, s.success ? "✓" : "✕"),
            el(
              "div",
              {},
              el("div", { class: "label" }, s.step_id || "(step)"),
              s.summary ? el("div", { class: "summary" }, s.summary) : null
            )
          )
        );
      }
      ul.appendChild(el("li", { class: cls }, head, stepsUl));
    }
    wrap.appendChild(ul);
    return wrap;
  }

  // ── Router ───────────────────────────────────────────
  function parsePath() {
    const path = window.location.pathname.replace(/\/+$/, "");
    return path.split("/").filter(Boolean);
  }

  function route() {
    const app = document.getElementById("app");
    if (!app) return;
    const segs = parsePath();
    if (segs.length === 0) {
      renderIndex(app);
    } else if (segs.length === 2 && segs[0] === "tasks") {
      renderTask(app, decodeURIComponent(segs[1]));
    } else if (segs.length === 3 && segs[0] === "tasks" && segs[2] === "history") {
      renderHistory(app, decodeURIComponent(segs[1]));
    } else {
      app.innerHTML = "";
      app.appendChild(topbar());
      app.appendChild(
        el(
          "main",
          {},
          el(
            "div",
            { class: "empty" },
            el("div", { class: "glyph" }, "404"),
            el("div", { class: "hint" }, `Unknown path: ${escapeHtml(window.location.pathname)}`)
          )
        )
      );
    }
  }

  document.addEventListener("DOMContentLoaded", route);
  // 支持浏览器 back/forward 时重渲染。
  window.addEventListener("popstate", route);
})();
