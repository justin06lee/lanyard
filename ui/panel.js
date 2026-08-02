const { describe, monogram, statusOf } = window.gru;

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const rows = document.getElementById("rows");
const count = document.getElementById("count");
const empty = document.getElementById("empty");
const notice = document.getElementById("notice");
const grant = document.getElementById("grant");
const setup = document.getElementById("setup");
const setupTitle = document.getElementById("setup-title");
const setupText = document.getElementById("setup-text");

let editing = null; // sessionId currently being renamed

function buildRow(session) {
  const li = document.createElement("li");
  li.className = `row${session.focused ? " is-focused" : ""}`;

  const status = statusOf(session);
  const tile = document.createElement("div");
  tile.className = `tile ${status}`;
  tile.textContent = monogram(session.name);
  tile.title = status;
  li.appendChild(tile);

  const text = document.createElement("div");
  text.className = "text";

  const name = document.createElement("div");
  name.className = "name";
  name.textContent = session.name;
  name.title = `${session.cwd}\nDouble-click to rename`;

  const desc = document.createElement("div");
  desc.className = "desc";
  desc.textContent = describe(session);

  text.append(name, desc);
  li.appendChild(text);

  const pid = document.createElement("span");
  pid.className = "pid";
  pid.textContent = session.pid;
  li.appendChild(pid);

  name.addEventListener("dblclick", () => startEdit(text, name, session));
  return li;
}

function startEdit(text, name, session) {
  if (editing) return;
  editing = session.sessionId;

  const input = document.createElement("input");
  input.className = "rename";
  input.value = session.name;
  input.spellcheck = false;

  name.hidden = true;
  text.prepend(input);
  input.focus();
  input.select();

  const finish = (commit) => {
    if (editing !== session.sessionId) return;
    editing = null;
    const value = input.value;
    input.remove();
    name.hidden = false;
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

  setup.hidden = state.titleConflicts === 0;
  if (state.titleConflicts > 0) {
    const n = state.titleConflicts;
    setupTitle.textContent = `${n} session${n === 1 ? "" : "s"} still control their own title.`;
    setupText.textContent =
      " gru has to keep reclaiming it, which you may notice in Mission Control." +
      " Add this to your shell profile and restart them:";
  }

  // Never yank the input out from under someone mid-rename.
  if (editing) return;

  rows.replaceChildren(...state.sessions.map(buildRow));
  rows.hidden = state.sessions.length === 0;
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
