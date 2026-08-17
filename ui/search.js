const { describe, statusOf } = window.lanyard;

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const root = document.getElementById("root");
const query = document.getElementById("query");
const list = document.getElementById("list");
const count = document.getElementById("count");
const empty = document.getElementById("empty");

let sessions = [];
let results = [];
let selected = 0;

/* ------------------------------------------------------------------ fuzzy */

/* Subsequence match with positional bonuses, tuned for short names — "ju de"
   should land on "justin06lee.dev" ahead of anything merely containing the
   letters. Returns matched indices for highlighting, or null on a miss. */
const SEPARATORS = new Set([" ", "-", "_", "/", ".", ":"]);

function fuzzyMatch(q, target) {
  if (!q) return { score: 0, indices: [] };

  const needle = q.toLowerCase();
  const t = target.toLowerCase();
  const indices = [];

  let score = 0;
  let ti = 0;
  let lastMatch = -2;

  for (let qi = 0; qi < needle.length; qi++) {
    const ch = needle[qi];
    // Spaces in the query are separators, not literals.
    if (ch === " ") continue;

    let found = -1;
    for (let i = ti; i < t.length; i++) {
      if (t[i] === ch) {
        found = i;
        break;
      }
    }
    if (found === -1) return null;

    if (found === 0) score += 20;
    else if (SEPARATORS.has(t[found - 1])) score += 14;
    if (found === lastMatch + 1) score += 10;

    // Mild penalty for gaps, so tight matches float without swamping bonuses.
    score -= Math.min(found - ti, 6);

    indices.push(found);
    lastMatch = found;
    ti = found + 1;
  }

  const compact = needle.replace(/\s+/g, "");
  if (t === compact) score += 60;
  else if (t.startsWith(compact)) score += 30;
  else if (t.includes(compact)) score += 15;

  // Shorter targets are a better fit for the same match.
  score -= t.length * 0.1;

  return { score, indices };
}

/* ------------------------------------------------------------------- rank */

/* Empty query lists everything, the sessions that need you first; typing
   matches the name (highlighted), falling back to repo path, Claude's summary
   and the working directory so a session is findable by what it's doing, not
   only by what it's called. */
function rank(q) {
  if (!q.trim()) {
    const urgency = (s) => {
      const status = statusOf(s);
      return status === "waiting" ? 0 : status === "busy" ? 1 : 2;
    };
    return sessions
      .map((session, i) => ({ session, indices: [], order: urgency(session) * 1000 + i }))
      .sort((a, b) => a.order - b.order);
  }
  const scored = [];
  for (const session of sessions) {
    const onName = fuzzyMatch(q, session.name);
    if (onName) {
      scored.push({ session, indices: onName.indices, score: onName.score + 25 });
      continue;
    }
    const haystack = [
      session.repo,
      session.subpath,
      session.aiTitle ?? "",
      session.claudeName ?? "",
      session.cwd,
    ].join(" ");
    const loose = fuzzyMatch(q, haystack);
    if (loose) scored.push({ session, indices: [], score: loose.score - 20 });
  }
  return scored.sort((a, b) => b.score - a.score);
}

/* ----------------------------------------------------------------- render */

function highlight(text, indices) {
  const span = document.createElement("span");
  if (indices.length === 0) {
    span.textContent = text;
    return span;
  }
  const set = new Set(indices);
  for (let i = 0; i < text.length; i++) {
    const node = set.has(i) ? document.createElement("mark") : document.createElement("span");
    node.textContent = text[i];
    span.appendChild(node);
  }
  return span;
}

function jump(session) {
  invoke("raise_session", { pid: session.pid }).catch(() => {});
  invoke("hide_search").catch(() => {});
}

function buildRow({ session, indices }, i) {
  const li = document.createElement("li");
  li.className = "s-row";
  li.dataset.selected = i === selected;

  const status = statusOf(session);
  const dot = document.createElement("div");
  dot.className = `dot ${status}`;
  dot.title = status;
  li.appendChild(dot);

  const text = document.createElement("div");
  text.className = "s-text";

  const name = document.createElement("div");
  name.className = "s-name";
  name.appendChild(highlight(session.name, indices));

  const desc = document.createElement("div");
  desc.className = "s-desc";
  desc.textContent = describe(session);

  text.append(name, desc);
  li.appendChild(text);

  li.addEventListener("mousemove", () => {
    if (selected !== i) {
      selected = i;
      render();
    }
  });
  li.addEventListener("click", () => jump(session));
  return li;
}

function render() {
  results = rank(query.value);
  if (selected >= results.length) selected = Math.max(0, results.length - 1);

  list.replaceChildren(...results.map(buildRow));
  list.hidden = results.length === 0;

  count.hidden = !query.value;
  count.textContent = results.length;

  empty.hidden = results.length > 0;
  empty.textContent = sessions.length === 0 ? "No sessions running." : "Nothing matches.";

  list
    .querySelector('[data-selected="true"]')
    ?.scrollIntoView({ block: "nearest" });
}

/* The window is exactly as tall as its content: measure and ask, the same
   contract the pill uses for its width. Rust clamps; past the cap the list
   scrolls inside itself. */
let requestedHeight = 0;
const sync = () => {
  const height = Math.ceil(root.getBoundingClientRect().height);
  if (height !== requestedHeight) {
    requestedHeight = height;
    invoke("resize_search", { height }).catch(() => {});
  }
};
new ResizeObserver(sync).observe(root);

/* ------------------------------------------------------------------- keys */

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.preventDefault();
    invoke("hide_search").catch(() => {});
    return;
  }

  const move = (delta) => {
    e.preventDefault();
    if (results.length === 0) return;
    selected = (selected + delta + results.length) % results.length;
    render();
  };

  if (e.key === "ArrowDown" || (e.ctrlKey && e.key === "n")) move(1);
  else if (e.key === "ArrowUp" || (e.ctrlKey && e.key === "p")) move(-1);
  else if (e.key === "Enter") {
    e.preventDefault();
    const current = results[selected];
    if (current) jump(current.session);
  }
});

query.addEventListener("input", () => {
  selected = 0;
  render();
});

/* ------------------------------------------------------------------ wiring */

listen("lanyard://state", (event) => {
  document.documentElement.dataset.theme = event.payload.appearance;
  sessions = event.payload.sessions;
  render();
});

/* Summoning resets the palette before it becomes visible, so the previous
   query never flashes. */
listen("lanyard://search-open", () => {
  query.value = "";
  selected = 0;
  render();
  query.focus();
});

invoke("get_state")
  .then((state) => {
    document.documentElement.dataset.theme = state.appearance;
    sessions = state.sessions;
    render();
    query.focus();
  })
  .catch(() => {});
