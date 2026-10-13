// The Relation view: callers, callees or references of one function, as a
// tree or as a graph. The extension answers `expand` with a node's children;
// everything shown is built from those answers.
(function () {
  const vscode = acquireVsCodeApi();
  const root = document.getElementById("root");
  const message = document.getElementById("message");
  const KIND = { function: "ƒ", macro: "#", struct: "S", union: "S", enum: "E", enum_const: "e", typedef: "T", global: "v", field: "·" };

  let mode = "callees"; // callees | callers | references
  let view = "tree"; // tree | graph
  let top = null; // the root node
  let refs = null; // references mode: [{file, uri, items}]
  let nodes = new Map(); // id -> node
  let nextId = 1;

  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

  function node(entry, parent, extra) {
    const n = { id: nextId++, entry, parent, children: null, open: false, loading: false, ...extra };
    nodes.set(n.id, n);
    return n;
  }

  // A function stored in a member or variable hangs under that member: the
  // call goes to the member, and which function is there is a second step.
  function build(entries, parent) {
    const out = [];
    const slots = new Map();
    for (const e of entries) {
      if (mode === "callees" && e.through) {
        if (!slots.has(e.through)) {
          const s = node(null, parent, { slot: e.through, lines: e.lines, children: [], open: true });
          slots.set(e.through, s);
          out.push(s);
        }
        slots.get(e.through).children.push(node(e, slots.get(e.through)));
      } else {
        out.push(node(e, parent));
      }
    }
    return out;
  }

  // Where a click goes: the line the call is on. A caller's call is in the
  // caller's file; a callee's is in the file of the function making it.
  function site(n) {
    if (n.slot) {
      const owner = owning(n);
      return owner ? { uri: owner.entry.uri, line: (n.lines?.[0] ?? owner.entry.line) - 1, character: 0 } : null;
    }
    const e = n.entry;
    if (!n.parent) return { uri: e.uri, line: e.range.start.line, character: e.range.start.character };
    const line = (e.lines?.[0] ?? e.line) - 1;
    if (mode === "callers") return { uri: e.uri, line, character: 0 };
    const owner = owning(n);
    return owner ? { uri: owner.entry.uri, line, character: 0 } : null;
  }

  function owning(n) {
    let p = n.parent;
    while (p && p.slot) p = p.parent;
    return p;
  }

  function definition(n) {
    return n.entry ? { uri: n.entry.uri, line: n.entry.range.start.line, character: n.entry.range.start.character } : site(n);
  }

  function badges(n) {
    const e = n.entry;
    if (n.slot) return '<span class="badge ptr">函式指標</span>';
    let b = "";
    if (e.through && mode === "callers") b += '<span class="badge ptr">經 ' + esc(e.through) + "</span>";
    if (e.pointer) b += '<span class="badge ptr">經指標變數</span>';
    if (e.viaMacro) b += '<span class="badge mac">經巨集 ' + esc(e.viaMacro) + "</span>";
    if (e.certain === false) b += '<span class="badge amb">不確定</span>';
    return b;
  }

  function toggle(n) {
    if (n.slot) {
      n.open = !n.open;
      return render();
    }
    if (n.children) {
      n.open = !n.open;
      return render();
    }
    n.loading = true;
    render();
    vscode.postMessage({ expand: n.entry.data, id: n.id });
  }

  // ---------------------------------------------------------------- tree

  function treeRows(n, depth, out) {
    const leaf = n.children && n.children.length === 0;
    const twisty = leaf ? "" : n.open ? "▾" : "▸";
    const e = n.entry;
    const where = n.slot ? (n.lines?.length ? "第 " + n.lines.join("、") + " 行" : "") : e.file + ":" + e.line;
    const calls = !n.slot && n.parent && e.lines?.length ? ' <span class="at">· 第 ' + e.lines.slice(0, 4).join("、") + (e.lines.length > 4 ? "…" : "") + " 行</span>" : "";
    out.push(
      '<div class="row' + (n.parent ? "" : " top") + '" data-id="' + n.id + '" style="padding-left:' + (8 + depth * 16) + 'px" tabindex="0">' +
        '<span class="tw" data-act="toggle">' + (n.loading ? "…" : twisty) + "</span>" +
        '<span class="k k-' + (n.slot ? "field" : e.kind) + '">' + (n.slot ? "·" : KIND[e.kind] ?? "?") + "</span>" +
        '<span class="name" data-act="name" title="' + (n.slot ? "到呼叫處" : "跳到這個函式") + '">' + esc(n.slot ?? e.name) + "</span>" + badges(n) +
        '<span class="loc" title="到呼叫處">' + esc(where) + calls + "</span>" +
        "</div>",
    );
    if (n.open && n.children) for (const c of n.children) treeRows(c, depth + 1, out);
  }

  function renderRefs() {
    const out = [];
    refs.forEach((g, gi) => {
      out.push('<div class="row top" style="padding-left:8px"><span class="tw">▾</span><span class="name">' + esc(g.file) + '</span><span class="loc">' + g.items.length + " 處</span></div>");
      g.items.forEach((it, ii) => {
        out.push('<div class="row" data-ref="' + gi + "," + ii + '" style="padding-left:28px" tabindex="0"><span class="ln">' + (it.line + 1) + '</span><span class="name text">' + esc(it.text) + "</span></div>");
      });
    });
    root.innerHTML = out.join("") || '<div class="empty">沒有參照</div>';
  }

  // ---------------------------------------------------------------- graph

  const ROW = 34, GAP = 48, H = 24;

  function width(n) {
    const text = n.slot ?? n.entry.name;
    return Math.min(260, Math.max(64, text.length * 7.2 + 26));
  }

  function renderGraph() {
    // Tidy tree: leaves one per row, parents centred on their children.
    const cols = [];
    let row = 0;
    (function place(n, depth) {
      n.depth = depth;
      cols[depth] = Math.max(cols[depth] ?? 0, width(n) + 20);
      const kids = n.open && n.children ? n.children : [];
      if (!kids.length) {
        n.y = row++ * ROW;
      } else {
        kids.forEach((c) => place(c, depth + 1));
        n.y = (kids[0].y + kids[kids.length - 1].y) / 2;
      }
    })(top, 0);
    const xs = [];
    let x = 8;
    cols.forEach((w, d) => {
      xs[d] = x;
      x += w + GAP;
    });
    const total = x - GAP + 8;
    // Callers read right to left: the function is on the right.
    const left = (n) => (mode === "callers" ? total - xs[n.depth] - cols[n.depth] : xs[n.depth]);
    const shapes = [], edges = [];
    (function draw(n) {
      const w = width(n), nx = mode === "callers" ? left(n) + cols[n.depth] - w : left(n), ny = n.y + 6;
      const e = n.entry;
      const cls = n.slot ? "slot" : !n.parent ? "rootnode" : e.through || e.pointer ? "ptr" : e.viaMacro ? "mac" : "";
      // The knob beside a node grows or folds the graph; the node itself
      // is the function, and clicking it goes there.
      const leaf = n.children && n.children.length === 0;
      const kx = mode === "callers" ? nx - 11 : nx + w + 11, ky = ny + H / 2;
      const more = leaf ? "" :
        '<g class="knob" data-act="toggle"><title>' + (n.open ? "收合" : "展開") + '</title><circle cx="' + kx + '" cy="' + ky + '" r="7"></circle>' +
        '<text x="' + kx + '" y="' + (ky + 4) + '" text-anchor="middle">' + (n.loading ? "…" : n.open ? "−" : "+") + "</text></g>";
      shapes.push(
        '<g class="gn ' + cls + (e && e.certain === false ? " amb" : "") + '" data-id="' + n.id + '"><title>' + esc(n.slot ? "結構成員（函式指標）" : e.file + ":" + e.line) + "</title>" +
          '<rect x="' + nx + '" y="' + ny + '" width="' + w + '" height="' + H + '" rx="' + (n.slot ? 12 : 3) + '"></rect>' +
          '<text x="' + (nx + 10) + '" y="' + (ny + 16) + '">' + esc(n.slot ?? e.name) + "</text>" + more + "</g>",
      );
      // Edges leave from beyond the knob.
      n.box = { x: mode === "callers" ? nx - (leaf ? 0 : 20) : nx, y: ny + H / 2, w: w + (leaf ? 0 : 20) };
      if (n.open && n.children)
        for (const c of n.children) {
          draw(c);
          const a = n.box, b = c.box;
          const from = mode === "callers" ? a.x : a.x + a.w, to = mode === "callers" ? b.x + b.w : b.x, mid = (from + to) / 2;
          const kind = c.slot || n.slot ? "ptr" : c.entry.viaMacro ? "mac" : c.entry.pointer ? "ptr" : "";
          edges.push('<path class="ge ' + kind + '" d="M' + from + " " + a.y + " C" + mid + " " + a.y + " " + mid + " " + b.y + " " + to + " " + b.y + '"></path>');
        }
    })(top);
    root.innerHTML =
      '<div class="graph"><svg width="' + Math.max(total, 200) + '" height="' + (row * ROW + 12) + '">' + edges.join("") + shapes.join("") + "</svg></div>" +
      '<div class="hint">點函式名稱跳到該函式（Ctrl＋點開在旁邊）；點旁邊的 ＋／− 展開或收合。</div>';
  }

  // ---------------------------------------------------------------- common

  function render() {
    document.querySelectorAll("[data-mode]").forEach((b) => b.setAttribute("aria-selected", String(b.dataset.mode === mode)));
    document.querySelectorAll("[data-view]").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.view === view)));
    document.getElementById("views").hidden = mode === "references";
    if (mode === "references") return refs ? renderRefs() : (root.innerHTML = "");
    if (!top) {
      root.innerHTML = '<div class="empty">把游標放在函式上，這裡會顯示它的呼叫關係。</div>';
      return;
    }
    if (view === "graph") return renderGraph();
    const out = [];
    treeRows(top, 0, out);
    root.innerHTML = out.join("");
  }

  document.getElementById("bar").addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b) return;
    if (b.dataset.mode) vscode.postMessage({ mode: b.dataset.mode });
    if (b.dataset.view) {
      view = b.dataset.view;
      vscode.setState({ view });
      render();
    }
  });

  function act(e, dbl) {
    const beside = e.ctrlKey || e.metaKey;
    const ref = e.target.closest("[data-ref]");
    if (ref) {
      const [g, i] = ref.dataset.ref.split(",").map(Number);
      return vscode.postMessage({ open: { uri: refs[g].uri, line: refs[g].items[i].line, character: refs[g].items[i].character }, beside });
    }
    const el = e.target.closest("[data-id]");
    if (!el) return;
    const n = nodes.get(+el.dataset.id);
    const what = e.target.closest("[data-act]")?.dataset.act;
    if (dbl) return;
    if (what === "toggle") return toggle(n);
    // The name is the function: go to it. The rest of a row in the tree is
    // the call: go to the line it is made on.
    const t = view === "graph" || what === "name" ? definition(n) : site(n);
    if (t) vscode.postMessage({ open: t, beside });
  }
  root.addEventListener("click", (e) => act(e, false));
  root.addEventListener("dblclick", (e) => act(e, true));
  root.addEventListener("keydown", (e) => {
    const el = e.target.closest("[data-id]");
    if (!el) return;
    const n = nodes.get(+el.dataset.id);
    if (e.key === "ArrowRight" && !n.open) toggle(n);
    else if (e.key === "ArrowLeft" && n.open) toggle(n);
    else if (e.key === "Enter") {
      const t = site(n);
      if (t) vscode.postMessage({ open: t, beside: e.ctrlKey || e.metaKey });
    }
  });

  window.addEventListener("message", (ev) => {
    const d = ev.data;
    if (d.type === "root") {
      mode = d.mode;
      nodes = new Map();
      refs = null;
      message.textContent = d.message ?? "";
      top = d.root ? node(d.root, null) : null;
      if (top) {
        top.children = build(d.entries, top);
        top.open = true;
      }
    } else if (d.type === "children") {
      const n = nodes.get(d.id);
      if (n) {
        n.loading = false;
        n.children = build(d.entries, n);
        n.open = true;
      }
    } else if (d.type === "refs") {
      mode = "references";
      refs = d.groups;
      top = null;
      message.textContent = d.message ?? "";
    } else if (d.type === "view") {
      view = d.view;
    }
    render();
  });

  view = vscode.getState()?.view ?? "tree";
  render();
  vscode.postMessage({ ready: true });
})();
