const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const rows = {
  ax: {
    dot: document.getElementById("ax-dot"),
    sub: document.getElementById("ax-sub"),
    fix: document.getElementById("ax-fix"),
  },
  notif: {
    dot: document.getElementById("notif-dot"),
    sub: document.getElementById("notif-sub"),
    fix: document.getElementById("notif-fix"),
  },
  files: {
    dot: document.getElementById("files-dot"),
    sub: document.getElementById("files-sub"),
    fix: document.getElementById("files-fix"),
  },
};

/* One row: green dot and no button when granted; orange dot, a line saying
   what's wrong, and the button that fixes it when not. */
function set(row, ok, label, sub) {
  row.dot.className = `dot ${ok ? "ok" : "waiting"}`;
  row.fix.hidden = ok;
  row.fix.textContent = label;
  row.sub.textContent = ok ? "" : sub;
}

function render(state) {
  document.documentElement.dataset.theme = state.appearance;

  set(rows.ax, state.axTrusted, "Grant…", "Not granted — the pill is hidden.");

  const n = state.notifications;
  set(
    rows.notif,
    n === "granted",
    n === "denied" ? "Open Settings…" : "Allow…",
    n === "denied"
      ? "Denied — flip Lanyard on under System Settings › Notifications."
      : "Not decided yet — macOS is holding the consent prompt.",
  );

  const blocked = state.cwdBlocked;
  set(
    rows.files,
    blocked === 0,
    "Fix…",
    `${blocked} session folder${blocked === 1 ? "" : "s"} unreadable.`,
  );
}

rows.ax.fix.addEventListener("click", () =>
  invoke("request_accessibility").catch(() => {}),
);
rows.notif.fix.addEventListener("click", () =>
  invoke("request_notifications").catch(() => {}),
);
rows.files.fix.addEventListener("click", () =>
  invoke("request_file_access").catch(() => {}),
);

listen("lanyard://state", (event) => render(event.payload));
invoke("get_state").then(render).catch(() => {});
