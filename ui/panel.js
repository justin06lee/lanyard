const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const rows = document.getElementById("rows");
const count = document.getElementById("count");
const empty = document.getElementById("empty");
const notice = document.getElementById("notice");
const grant = document.getElementById("grant");

let editing = null; // sessionId currently being renamed

function describe(session) {
  const path = session.subpath
    ? `${session.repo}/${session.subpath}`
    : session.repo;
  return session.renamed ? path : path === session.name ? "" : path;
}

function buildRow(session) {
  const li = document.createElement("li");
  li.className = `row${session.focused ? " is-focused" : ""}`;

  const dot = document.createElement("span");
  dot.className = `dot ${session.status === "busy" ? "busy" : "idle"}`;
  li.appendChild(dot);

  const text = document.createElement("div");
  text.className = "text";

  const name = document.createElement("div");
  name.className = "name";
  name.textContent = session.name;
  name.title = `${session.cwd}\nDouble-click to rename`;

  const meta = document.createElement("div");
  meta.className = "meta";
  meta.textContent = describe(session);

  text.append(name, meta);
  li.appendChild(text);

  const pid = document.createElement("span");
  pid.className = "pid";
  pid.textContent = session.pid;
  li.appendChild(pid);

  name.addEventListener("dblclick", () => startEdit(text, name, meta, session));
  return li;
}

function startEdit(text, name, meta, session) {
  if (editing) return;
  editing = session.sessionId;

  const input = document.createElement("input");
  input.className = "rename";
  input.value = session.name;
  input.spellcheck = false;

  name.hidden = true;
  meta.hidden = true;
  text.prepend(input);
  input.focus();
  input.select();

  const finish = (commit) => {
    if (editing !== session.sessionId) return;
    editing = null;
    const value = input.value;
    input.remove();
    name.hidden = false;
    meta.hidden = false;
    if (commit) {
      invoke("rename", {
        sessionId: session.sessionId,
        cwd: session.cwd,
        name: value,
      }).catch(() => {});
    }
  };

  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      finish(true);
    } else if (e.key === "Escape") {
      e.preventDefault();
      finish(false);
    }
  });
  input.addEventListener("blur", () => finish(false));
}

function render(state) {
  notice.hidden = state.axTrusted;

  // Never yank the input out from under someone mid-rename.
  if (editing) return;

  rows.replaceChildren(...state.sessions.map(buildRow));
  count.textContent = state.sessions.length
    ? `${state.sessions.length} running`
    : "";
  empty.hidden = state.sessions.length > 0;
}

grant.addEventListener("click", () =>
  invoke("request_accessibility").catch(() => {}),
);

listen("gru://state", (event) => render(event.payload));
invoke("get_state").then(render).catch(() => {});
