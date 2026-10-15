// The Relation view: what a function calls, what calls it, where a name is
// referenced, and where a function pointer is assigned, as a tree or (for
// calls) a graph. The extension answers `expand` with a node's children;
// everything shown is built from those answers.
(function () {
  const vscode = acquireVsCodeApi();
  const root = document.getElementById("root");
  const message = document.getElementById("message");
  const ICON = {
    function: "symbol-function", macro: "symbol-constant", struct: "symbol-struct", union: "symbol-struct", enum: "symbol-enum",
    enum_const: "symbol-enum-member", typedef: "symbol-class", global: "symbol-variable", field: "symbol-field", local: "symbol-variable", parameter: "symbol-parameter",
    class: "symbol-class", namespace: "symbol-namespace" };

  let mode = "callees"; // callees | callers | references | assignments
  let view = "tree"; // tree | graph
  let top = null; // the root node
  let refs = null; // references: [{file, uri, items}]
  let assigned = null; // assignments: {by, target, rows}
  let nodes = new Map(); // id -> node
  let nextId = 1;

  const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
  const base = (file) => String(file).split("/").pop();
  const icon = (kind) => '<span class="codicon codicon-' + (ICON[kind] ?? "symbol-misc") + " k-" + kind + '"></span>';

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

  function owning(n) {
    let p = n.parent;
    while (p && p.slot) p = p.parent;
    return p;
  }

  // The lines a node's calls are on, and the file they are in: a caller's
  // calls are in the caller's file, a callee's in the file of who calls it.
  function calls(n) {
    if (!n.parent) return { uri: null, lines: [] };
    const lines = (n.slot ? n.lines : n.entry.lines) ?? [];
    if (!n.slot && mode === "callers") return { uri: n.entry.uri, lines };
    const owner = owning(n);
    return { uri: owner ? owner.entry.uri : null, lines };
  }

  function definition(n) {
    if (n.entry) return { uri: n.entry.uri, line: n.entry.range.start.line, character: n.entry.range.start.character };
    const c = calls(n);
    return c.uri && c.lines.length ? { uri: c.uri, line: c.lines[0] - 1, character: 0 } : null;
  }

  function badges(n) {
    const e = n.entry;
    if (n.slot) return '<span class="badge ptr">function pointer</span>';
    let b = "";
    if (e.through && mode === "callers") b += '<span class="badge ptr">through ' + esc(e.through) + "</span>";
    if (e.pointer) b += '<span class="badge ptr">through a pointer</span>';
    if (e.viaMacro) b += '<span class="badge mac">via macro ' + esc(e.viaMacro) + "</span>";
    if (e.certain === false) b += '<span class="badge amb">uncertain</span>';
    // Every one of its lines is under an #if the conditions cannot decide.
    if (e.onlyIf && e.onlyIf.length) {
      const why = e.onlyIf.map((c) => "line " + c.line + ": " + c.text).join("\n");
      b += '<span class="badge cond" title="Compiled only if ' + esc(why) + '\nNorn cannot tell whether this is compiled: define or undefine the macro in Compile Conditions to decide.">under' + esc(e.onlyIf.map((c) => c.text).join(", ")) + "</span>";
    }
    return b;
  }

  function toggle(n) {
    if (n.slot || n.children) {
      n.open = !n.open;
      return render();
    }
    n.loading = true;
    render();
    vscode.postMessage({ expand: n.entry.data, id: n.id });
  }

  // Each line a call is on is a link of its own.
  function lineLinks(n) {
    const c = calls(n);
    if (!c.uri || !c.lines.length) return "";
    return '<span class="lines">' + c.lines.map((l) => '<span class="ln" data-line="' + l + '" title="Go to the call on line ' + l + '">' + l + "</span>").join("") + "</span>";
  }

  // ---------------------------------------------------------------- tree

  function treeRows(n, depth, out) {
    const leaf = n.children && n.children.length === 0;
    const twisty = leaf ? "" : n.open ? "▾" : "▸";
    const e = n.entry;
    const where = n.slot ? "" : '<span class="loc" title="' + esc(e.file + ":" + e.line) + '">' + esc(base(e.file)) + ":" + e.line + "</span>";
    out.push(
      '<div class="row' + (n.parent ? "" : " top") + '" data-id="' + n.id + '" style="padding-left:' + (8 + depth * 16) + 'px" tabindex="0">' +
        '<span class="tw" data-act="toggle">' + (n.loading ? "…" : twisty) + "</span>" +
        icon(n.slot ? "field" : e.kind) +
        '<span class="name" data-act="name" title="' + (n.slot ? "Go to the call" : "Go to this function") + '">' + esc(n.slot ?? e.name) + "</span>" + badges(n) +
        lineLinks(n) + where +
        "</div>",
    );
    if (n.open && n.children) for (const c of n.children) treeRows(c, depth + 1, out);
  }

  function renderRefs() {
    const out = [];
    refs.forEach((g, gi) => {
      out.push('<div class="row top" style="padding-left:8px" title="' + esc(g.file) + '"><span class="tw">▾</span><span class="codicon codicon-file"></span><span class="name">' + esc(base(g.file)) + '</span><span class="loc">' + g.items.length + "</span></div>");
      g.items.forEach((it, ii) => {
        out.push('<div class="row" data-ref="' + gi + "," + ii + '" style="padding-left:28px" tabindex="0"><span class="ln plain">' + (it.line + 1) + '</span><span class="name text">' + esc(it.text) + "</span></div>");
      });
    });
    root.innerHTML = out.join("") || '<div class="empty">No references</div>';
  }

  // Where functions are stored in a member or variable (or, for a function,
  // which members it is stored in): one row for each place.
  function renderAssigned() {
    if (!assigned || !assigned.rows.length) {
      root.innerHTML = '<div class="empty">' + (assigned && assigned.target ? "Nothing is stored here that the index can see." : "Put the cursor on a function pointer member, a variable, or a function.") + "</div>";
      return;
    }
    const out = [];
    assigned.rows.forEach((r, i) => {
      const [shown, other] = assigned.by === "member" ? [r.function, r.member] : [r.member, r.function];
      out.push(
        '<div class="row" data-assign="' + i + '" style="padding-left:8px" tabindex="0">' + icon(shown.kind) +
          '<span class="name" data-act="name" title="Go to ' + esc(shown.name) + '">' + esc(shown.name) + "</span>" +
          '<span class="loc">' + (assigned.by === "member" ? "stored in " : "holds ") + esc(other.name) + "</span>" +
          '<span class="lines"><span class="ln" data-act="site" title="Go to where it is assigned">' + esc(base(r.file)) + ":" + r.line + "</span></span>" +
          (r.inside ? '<span class="loc">in ' + esc(r.inside) + "</span>" : "") +
          "</div>",
      );
    });
    root.innerHTML = out.join("");
  }

  // ---------------------------------------------------------------- graph

  const ROW = 46, GAP = 48, H = 24;

  // Text is measured in the font it is drawn in (the editor's, whatever
  // the user has set): a guess per character is too narrow for some fonts
  // and for names in other scripts, and the name then runs out of its box.
  const pen = document.createElement("canvas").getContext("2d");
  let family = "monospace";
  const measured = new Map();
  function measure(text, size) {
    const key = size + "|" + text;
    let w = measured.get(key);
    if (w === undefined) {
      pen.font = size + "px " + family;
      w = pen.measureText(text).width;
      measured.set(key, w);
    }
    return w;
  }
  const PAD = 10, WIDEST = 420;

  // The name as drawn: whole, or cut short with an ellipsis when it would
  // make a column wider than is useful (the tooltip has all of it).
  function label(n) {
    const text = n.slot ?? n.entry.name;
    if (measure(text, 12) + 2 * PAD <= WIDEST) return text;
    let k = text.length - 1;
    while (k > 4 && measure(text.slice(0, k) + "…", 12) + 2 * PAD > WIDEST) k--;
    return text.slice(0, k) + "…";
  }

  function width(n) {
    return Math.max(64, Math.ceil(measure(label(n), 12)) + 2 * PAD);
  }

  // The line numbers under a node, as far as they are shown.
  function linesUnder(n) {
    const c = calls(n);
    const shown = c.uri ? c.lines.slice(0, 6) : [];
    const more = c.lines.length > shown.length;
    return { shown, more, width: shown.length ? Math.ceil(measure(shown.join(" · ") + (more ? " …" : ""), 10.5)) + 8 : 0 };
  }

  function renderGraph() {
    // Tidy tree: leaves one per row, parents centred on their children.
    const now = getComputedStyle(document.body).getPropertyValue("--vscode-editor-font-family").trim() || "monospace";
    if (now !== family) {
      family = now;
      measured.clear();
    }
    const cols = [];
    let row = 0;
    (function place(n, depth) {
      n.depth = depth;
      // A column is as wide as its widest node, or the line numbers under one.
      cols[depth] = Math.max(cols[depth] ?? 0, Math.max(width(n), linesUnder(n).width) + 20);
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
        '<g class="knob" data-act="toggle"><title>' + (n.open ? "Collapse" : "Expand") + '</title><circle cx="' + kx + '" cy="' + ky + '" r="7"></circle>' +
        '<text x="' + kx + '" y="' + (ky + 4) + '" text-anchor="middle">' + (n.loading ? "…" : n.open ? "−" : "+") + "</text></g>";
      // Under the node, the lines its calls are on, each a link.
      // (Towards the side the node is flush with, so that they stay in its column.)
      const lu = linesUnder(n);
      const under = lu.shown.length
        ? '<text class="glines" ' + (mode === "callers" ? 'text-anchor="end" x="' + (nx + w - 4) : 'x="' + (nx + 4)) + '" y="' + (ny + H + 12) + '">' +
          lu.shown.map((l) => '<tspan class="gl" data-line="' + l + '">' + l + "</tspan>").join('<tspan class="sep"> · </tspan>') +
          (lu.more ? '<tspan class="sep"> …</tspan>' : "") + "</text>"
        : "";
      const full = n.slot ?? e.name;
      shapes.push(
        '<g class="gn ' + cls + (e && (e.certain === false || (e.onlyIf && e.onlyIf.length)) ? " amb" : "") + '" data-id="' + n.id + '">' +
          '<g data-act="name"><title>' + esc(n.slot ? full + "\nstruct member (function pointer)" : full + "\n" + e.file + ":" + e.line + (e.onlyIf && e.onlyIf.length ? "\nunder " + e.onlyIf.map((c) => c.text).join(", ") : "")) + "</title>" +
          '<rect x="' + nx + '" y="' + ny + '" width="' + w + '" height="' + H + '" rx="' + (n.slot ? 12 : 3) + '"></rect>' +
          '<text x="' + (nx + PAD) + '" y="' + (ny + 16) + '">' + esc(label(n)) + "</text></g>" + under + more + "</g>",
      );
      // Edges leave from beyond the knob.
      n.box = { x: mode === "callers" ? nx - (leaf ? 0 : 20) : nx, y: ny + H / 2, w: w + (leaf ? 0 : 20) };
      if (n.open && n.children)
        for (const ch of n.children) {
          draw(ch);
          const a = n.box, b = ch.box;
          const from = mode === "callers" ? a.x : a.x + a.w, to = mode === "callers" ? b.x + b.w : b.x, mid = (from + to) / 2;
          const kind = ch.slot || n.slot ? "ptr" : ch.entry.viaMacro ? "mac" : ch.entry.pointer ? "ptr" : "";
          edges.push('<path class="ge ' + kind + '" d="M' + from + " " + a.y + " C" + mid + " " + a.y + " " + mid + " " + b.y + " " + to + " " + b.y + '"></path>');
        }
    })(top);
    root.innerHTML =
      '<div class="graph"><svg width="' + Math.max(total, 200) + '" height="' + (row * ROW + 12) + '">' + edges.join("") + shapes.join("") + "</svg></div>" +
      '<div class="hint">Click a name to go to the function, a line number to go to that call (Ctrl+click opens beside); the knob expands or collapses.</div>';
  }

  // ---------------------------------------------------------------- common

  function render() {
    document.querySelectorAll("[data-mode]").forEach((b) => b.setAttribute("aria-selected", String(b.dataset.mode === mode)));
    document.querySelectorAll("[data-view]").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.view === view)));
    document.getElementById("views").hidden = mode === "references" || mode === "assignments";
    if (mode === "references") return refs ? renderRefs() : (root.innerHTML = "");
    if (mode === "assignments") return renderAssigned();
    if (!top) {
      root.innerHTML = '<div class="empty">Put the cursor on a function to see what it calls and what calls it, or on a variable, type or member to see what uses it.</div>';
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

  function act(e) {
    const beside = e.ctrlKey || e.metaKey;
    const open = (t) => t && vscode.postMessage({ open: t, beside });
    const ref = e.target.closest("[data-ref]");
    if (ref) {
      const [g, i] = ref.dataset.ref.split(",").map(Number);
      const at = { uri: refs[g].uri, line: refs[g].items[i].line, character: refs[g].items[i].character };
      // One click: in the Context view. Two, or Ctrl: in the editor.
      if (e.type === "dblclick" || beside) return open(at);
      return vscode.postMessage({ show: at });
    }
    const what = e.target.closest("[data-act]")?.dataset.act;
    const row = e.target.closest("[data-assign]");
    if (row) {
      const r = assigned.rows[+row.dataset.assign];
      const shown = assigned.by === "member" ? r.function : r.member;
      // The name is the symbol; anything else on the row is the assignment.
      if (what === "name") return open({ uri: shown.uri, line: shown.range.start.line, character: shown.range.start.character });
      return open({ uri: r.uri, line: r.range.start.line, character: r.range.start.character });
    }
    const el = e.target.closest("[data-id]");
    if (!el) return;
    const n = nodes.get(+el.dataset.id);
    if (what === "toggle") return toggle(n);
    // One line of several: that call.
    const line = e.target.closest("[data-line]");
    if (line) return open({ uri: calls(n).uri, line: +line.dataset.line - 1, character: 0 });
    // The name is the function: go to it. The rest of a row is the call:
    // go to (the first of) the lines it is made on.
    if (what === "name" || view === "graph") return open(definition(n));
    const c = calls(n);
    open(c.uri && c.lines.length ? { uri: c.uri, line: c.lines[0] - 1, character: 0 } : definition(n));
  }
  root.addEventListener("click", act);
  root.addEventListener("dblclick", (e) => {
    if (e.target.closest("[data-ref]")) act(e);
  });
  root.addEventListener("keydown", (e) => {
    // Enter on a reference opens it, as a double click does.
    if (e.key === "Enter" && e.target.closest("[data-ref]")) return act({ type: "dblclick", target: e.target, ctrlKey: e.ctrlKey, metaKey: e.metaKey });
    const el = e.target.closest("[data-id]");
    if (!el) return;
    const n = nodes.get(+el.dataset.id);
    if (e.key === "ArrowRight" && !n.open) toggle(n);
    else if (e.key === "ArrowLeft" && n.open) toggle(n);
    else if (e.key === "Enter") {
      const t = definition(n);
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
    } else if (d.type === "assignments") {
      mode = "assignments";
      assigned = d;
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
