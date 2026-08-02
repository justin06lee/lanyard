const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const dot = document.getElementById("dot");
const name = document.getElementById("name");
const meta = document.getElementById("meta");
const rename = document.getElementById("rename");
const anchor = document.getElementById("anchor");

let current = null;
let editing = false;

const STATUS = {
  busy: ["busy", "working"],
  waiting: ["waiting", "waiting for you"],
};

/** Maps Claude Code's status string to a dot class and a tooltip. */
function statusOf(status) {
  return STATUS[status] || ["idle", "idle"];
}

/** Secondary line: whatever the name itself doesn't already tell you. */
function describe(session) {
  if (session.renamed) {
    return session.subpath ? `${session.repo}/${session.subpath}` : session.repo;
  }
  return session.subpath || session.claudeName || "";
}

function render(state) {
  const session = state.focused;
  if (!session) return;
  current = session;
  if (editing) return;

  name.textContent = session.name;
  meta.textContent = describe(session);
  const [cls, label] = statusOf(session.status);
  dot.className = `dot ${cls}`;
  dot.title = label;
}

function beginEdit() {
  if (!current || editing) return;
  editing = true;
  rename.value = current.name;
  rename.hidden = false;
  name.hidden = true;
  meta.hidden = true;
  rename.focus();
  rename.select();
}

function endEdit(commit) {
  if (!editing) return;
  const value = rename.value;
  editing = false;
  rename.hidden = true;
  name.hidden = false;
  meta.hidden = false;

  if (commit && current) {
    invoke("rename", {
      sessionId: current.sessionId,
      cwd: current.cwd,
      name: value,
    }).catch(() => {});
  }
}

name.addEventListener("dblclick", beginEdit);
name.addEventListener("keydown", (e) => {
  if (e.key === "Enter" || e.key === "F2") beginEdit();
});

rename.addEventListener("keydown", (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    endEdit(true);
  } else if (e.key === "Escape") {
    e.preventDefault();
    endEdit(false);
  }
});
rename.addEventListener("blur", () => endEdit(false));

anchor.addEventListener("click", () => invoke("cycle_anchor").catch(() => {}));

listen("gru://state", (event) => render(event.payload));
invoke("get_state").then(render).catch(() => {});
