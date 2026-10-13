// The Context view: the file a name is defined in, whole, at the line of
// the definition. Read-only; it scrolls, colours the code as the theme
// does, and passes hovering and Ctrl+click on to the editor's language
// features.
(function () {
  const vscode = acquireVsCodeApi();
  const head = document.getElementById("head");
  const scroll = document.getElementById("scroll");
  const space = document.getElementById("space");
  const rows = document.getElementById("rows");
  const tip = document.getElementById("tip");
  const empty = document.getElementById("empty");

  let entry = null;      // what the file is shown for
  let key = "";          // uri and version of the text in hand
  let lines = [];        // the text, by line
  let tokens = [];       // per line: [start, end, class] in order
  let target = 0;        // the definition's line (0-based) and its last
  let last = 0;
  let marked = "";       // the word whose occurrences are marked
  let LH = 18;           // line height, measured

  const esc = (s) => s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));

  // ---------------------------------------------------------------- colouring

  const KEYWORDS = new Set(("break case continue default do else for goto if return switch while sizeof typedef " +
    "_Alignof _Static_assert _Generic alignof static_assert").split(" "));
  const TYPES = new Set(("auto char const double enum extern float inline int long register restrict short signed static struct " +
    "union unsigned void volatile _Bool _Complex _Atomic _Noreturn _Thread_local _Alignas bool size_t ssize_t ptrdiff_t " +
    "int8_t int16_t int32_t int64_t uint8_t uint16_t uint32_t uint64_t intptr_t uintptr_t wchar_t FILE").split(" "));
  const isWord = (c) => (c >= 48 && c <= 57) || (c >= 65 && c <= 90) || (c >= 97 && c <= 122) || c === 95 || c > 127;

  // One pass over the file: comments and continued directives run over
  // lines, so a line cannot be coloured on its own.
  function tokenize(all) {
    const out = all.map(() => []);
    let comment = false;   // in a /* */ that began on a line above
    let directive = false; // in a preprocessor line continued with a backslash
    for (let n = 0; n < all.length; n++) {
      const s = all[n], t = out[n];
      let i = 0;
      const push = (a, b, c) => { if (b > a) t.push([a, b, c]); };
      if (comment) {
        const e = s.indexOf("*/");
        if (e < 0) { push(0, s.length, "c"); continue; }
        push(0, e + 2, "c");
        i = e + 2;
        comment = false;
      }
      let inDirective = directive;
      if (!inDirective) {
        const m = /^\s*#\s*[A-Za-z_]*/.exec(s.slice(i));
        if (m && s.slice(0, i).trim() === "") {
          push(i, i + m[0].length, "p");
          i += m[0].length;
          inDirective = true;
          // `#include <x.h>`: the name is a string.
          if (/#\s*include/.test(m[0])) {
            const a = s.indexOf("<", i), b = s.indexOf(">", a + 1);
            if (a >= 0 && b > a) { push(a, b + 1, "s"); }
          }
        }
      }
      directive = inDirective && /\\\s*$/.test(s);
      while (i < s.length) {
        const c = s.charCodeAt(i);
        if (c === 47 && s.charCodeAt(i + 1) === 47) { push(i, s.length, "c"); break; }
        if (c === 47 && s.charCodeAt(i + 1) === 42) {
          const e = s.indexOf("*/", i + 2);
          if (e < 0) { push(i, s.length, "c"); comment = true; break; }
          push(i, e + 2, "c");
          i = e + 2;
          continue;
        }
        if (c === 34 || c === 39) {
          let j = i + 1;
          while (j < s.length && s.charCodeAt(j) !== c) j += s.charCodeAt(j) === 92 ? 2 : 1;
          j = Math.min(j + 1, s.length);
          // (An include's <name> was taken above; do not colour it twice.)
          if (!t.some((x) => x[0] <= i && x[1] > i)) push(i, j, "s");
          i = j;
          continue;
        }
        if (c >= 48 && c <= 57 && !(i > 0 && isWord(s.charCodeAt(i - 1)))) {
          let j = i + 1;
          while (j < s.length && (isWord(s.charCodeAt(j)) || s[j] === "." || ((s[j] === "+" || s[j] === "-") && /[eEpP]/.test(s[j - 1])))) j++;
          push(i, j, "n");
          i = j;
          continue;
        }
        if (isWord(c)) {
          let j = i + 1;
          while (j < s.length && isWord(s.charCodeAt(j))) j++;
          const w = s.slice(i, j);
          if (!t.some((x) => x[0] <= i && x[1] > i)) {
            if (KEYWORDS.has(w)) push(i, j, "k");
            else if (TYPES.has(w)) push(i, j, "t");
            else {
              let k = j;
              while (k < s.length && (s[k] === " " || s[k] === "\t")) k++;
              push(i, j, s[k] === "(" ? "f" : inDirective && /#\s*(define|undef|ifdef|ifndef)\s*$/.test(s.slice(0, i)) ? "m" : "i");
            }
          }
          i = j;
          continue;
        }
        i++;
      }
      t.sort((a, b) => a[0] - b[0]);
    }
    return out;
  }

  function lineHtml(n) {
    const s = lines[n], t = tokens[n] || [];
    let out = "", at = 0;
    for (const [a, b, c] of t) {
      if (a < at) continue;
      if (a > at) out += esc(s.slice(at, a));
      const w = s.slice(a, b);
      // Names are what can be hovered and followed.
      const name = c === "i" || c === "f" || c === "m" || c === "t";
      out += '<span class="' + c + (name && w === marked ? " hit" : "") + '"' + (name ? ' data-c="' + a + '"' : "") + ">" + esc(w) + "</span>";
      at = b;
    }
    return out + esc(s.slice(at));
  }

  // ---------------------------------------------------------------- the rows in sight

  let from = -1, to = -1;
  function render(force) {
    if (!lines.length) return;
    const top = scroll.scrollTop, h = scroll.clientHeight || 200;
    const a = Math.max(0, Math.floor(top / LH) - 30), b = Math.min(lines.length, Math.ceil((top + h) / LH) + 30);
    if (!force && a === from && b === to) return;
    from = a;
    to = b;
    const digits = String(lines.length).length;
    let html = "";
    for (let n = a; n < b; n++) {
      const cls = n === target ? " at" : n > target && n <= last ? " in" : "";
      html += '<div class="l' + cls + '" style="top:' + n * LH + 'px" data-n="' + n + '"><span class="no">' + String(n + 1).padStart(digits) + "</span>" + lineHtml(n) + "</div>";
    }
    rows.innerHTML = html;
  }

  function measure() {
    const probe = document.createElement("div");
    probe.className = "l";
    probe.style.position = "static";
    probe.textContent = "X";
    rows.appendChild(probe);
    const h = probe.getBoundingClientRect().height;
    probe.remove();
    if (h > 4) LH = h;
  }

  function show(d) {
    entry = d.entry;
    tip.hidden = true;
    if (!d.entry) {
      empty.hidden = false;
      empty.textContent = d.message || "";
      head.hidden = scroll.hidden = true;
      return;
    }
    empty.hidden = true;
    head.hidden = scroll.hidden = false;
    const more = d.entry.candidates > 1 ? '<span class="dim">' + d.entry.candidates + " candidates</span>" : "";
    head.innerHTML = "<strong>" + esc(d.entry.name) + "</strong><span>" + esc(d.kindName) + '</span><span class="dim">' + esc(d.entry.file) + ":" + d.entry.line + "</span>" + more;
    if (d.text !== undefined) {
      key = d.key;
      lines = d.text.split(/\r?\n/);
      tokens = tokenize(lines);
      measure();
      // Wide enough for the longest line, so that the view scrolls sideways as far as it has to.
      let longest = 0;
      for (const l of lines) if (l.length > longest) longest = l.length;
      space.style.height = lines.length * LH + "px";
      space.style.width = "calc(" + (longest + String(lines.length).length + 4) + "ch + 16px)";
    }
    target = Math.max(0, d.entry.line - 1);
    last = Math.max(target, (d.entry.endLine || d.entry.line) - 1);
    marked = d.entry.name;
    // The definition a few lines from the top: what leads to it is in sight.
    scroll.scrollTop = Math.max(0, (target - 3) * LH);
    scroll.scrollLeft = 0;
    render(true);
  }

  // ---------------------------------------------------------------- pointing at names

  const place = (el) => ({ line: +el.closest(".l").dataset.n, character: +el.dataset.c });
  let hoverTimer, hoverAt = "", hoverSeq = 0;

  rows.addEventListener("mousemove", (e) => {
    const el = e.target.closest("[data-c]");
    const now = el ? el.closest(".l").dataset.n + ":" + el.dataset.c : "";
    if (now === hoverAt) return;
    hoverAt = now;
    clearTimeout(hoverTimer);
    if (!el) return;
    hoverTimer = setTimeout(() => {
      const seq = ++hoverSeq;
      const r = el.getBoundingClientRect();
      vscode.postMessage({ hover: place(el), seq, x: r.left, y: r.bottom, top: r.top });
    }, 350);
  });
  rows.addEventListener("mouseleave", () => {
    hoverAt = "";
    clearTimeout(hoverTimer);
    hoverSeq++;
    // Left for the tip itself? It stays while the pointer is on it.
    setTimeout(() => { if (!tip.matches(":hover")) tip.hidden = true; }, 200);
  });
  tip.addEventListener("mouseleave", () => { tip.hidden = true; });
  scroll.addEventListener("scroll", () => { tip.hidden = true; render(false); });
  new ResizeObserver(() => render(false)).observe(scroll);

  // What a language feature said, as text with code set apart.
  function tipHtml(parts) {
    return parts.map((p) => {
      let html = "", code = false;
      for (const piece of p.split(/```[a-zA-Z+]*\n?/)) {
        if (piece.trim()) html += code ? "<pre>" + esc(piece.replace(/\n+$/, "")) + "</pre>" : "<div>" + esc(piece.trim()).replace(/`([^`]+)`/g, "<code>$1</code>").replace(/\n{2,}/g, "<br><br>").replace(/\n/g, " ") + "</div>";
        code = !code;
      }
      return html;
    }).join("<hr>");
  }

  rows.addEventListener("click", (e) => {
    const el = e.target.closest("[data-c]");
    if (el && (e.ctrlKey || e.metaKey)) {
      // As in the editor: to the definition of what was clicked.
      vscode.postMessage({ definition: place(el), beside: e.altKey });
      return;
    }
    if (el && !String(getSelection())) {
      marked = el.textContent;
      render(true);
    }
  });
  rows.addEventListener("dblclick", (e) => {
    // The line itself, in the editor.
    const l = e.target.closest(".l");
    if (l && !e.target.closest("[data-c]")) vscode.postMessage({ line: +l.dataset.n });
  });
  rows.addEventListener("click", (e) => {
    const no = e.target.closest(".no");
    if (no) vscode.postMessage({ line: +no.closest(".l").dataset.n, beside: e.ctrlKey || e.metaKey });
  });
  head.addEventListener("click", (e) => { if (entry) vscode.postMessage({ open: entry, beside: e.ctrlKey || e.metaKey }); });

  window.addEventListener("message", (e) => {
    const d = e.data;
    if (d.type === "colors") {
      for (const [k, v] of Object.entries(d.colors)) {
        if (v) document.documentElement.style.setProperty("--tok-" + k, v); else document.documentElement.style.removeProperty("--tok-" + k);
      }
    } else if (d.type === "hover") {
      if (d.seq !== hoverSeq || !d.parts.length) return;
      tip.innerHTML = tipHtml(d.parts);
      tip.hidden = false;
      // Under the name, or above it when there is no room below.
      const w = document.documentElement.clientWidth, h = document.documentElement.clientHeight;
      const r = tip.getBoundingClientRect();
      tip.style.left = Math.max(4, Math.min(d.x, w - r.width - 8)) + "px";
      tip.style.top = (d.y + r.height + 6 > h && d.top - r.height - 4 > 0 ? d.top - r.height - 4 : d.y + 2) + "px";
    } else if (d.type === "show") {
      show(d);
    }
  });
  vscode.postMessage({ ready: true, key });
})();
