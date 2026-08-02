const { describe, monogram, statusOf } = window.gru;

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const card = document.getElementById("card");
const tile = document.getElementById("tile");
const text = document.getElementById("text");
const name = document.getElementById("name");
const desc = document.getElementById("desc");
const rename = document.getElementById("rename");
const anchor = document.getElementById("anchor");

let current = null;
let editing = false;
let shownId = null;

/* ------------------------------------------------------------------ render */

function render(state) {
  const session = state.focused;
  if (!session) return;
  current = session;

  const status = statusOf(session);
  tile.className = `tile ${status}`;
  tile.title = status;

  if (editing) return;

  tile.textContent = monogram(session.name);
  name.textContent = session.name;
  desc.textContent = describe(session);

  // Crossfade only when the session actually changed, not on every status tick.
  if (session.sessionId !== shownId) {
    shownId = session.sessionId;
    text.classList.remove("swap");
    void text.offsetWidth; // reflow, so the animation restarts
    text.classList.add("swap");
  }
}

/* ------------------------------------------------------------------ rename */

function beginEdit() {
  if (!current || editing) return;
  editing = true;
  rename.value = current.name;
  rename.hidden = false;
  name.hidden = true;
  rename.focus();
  rename.select();
}

function endEdit(commit) {
  if (!editing) return;
  const value = rename.value;
  editing = false;
  rename.hidden = true;
  name.hidden = false;

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

/* -------------------------------------------------- drag, then rubber-band */

// Displacement from the anchored position. The Rust side adds this to wherever
// the floater is anchored, so the card keeps tracking the terminal window even
// while it is being held aside.
let offX = 0;
let offY = 0;
let velX = 0;
let velY = 0;

let dragging = false;
let armed = false; // pointer down, but not yet past the drag threshold
let startX = 0;
let startY = 0;
let baseX = 0;
let baseY = 0;
let frame = 0;

// Underdamped: critical damping for k=220 is ~29.7, so 24 gives a little
// overshoot — the rubber-band snap, without the wobble of a bouncier spring.
const STIFFNESS = 220;
const DAMPING = 24;
const THRESHOLD = 3; // px of travel before a click becomes a drag
const MAX_STEP = 1 / 30; // clamp dt so a stalled frame can't explode the spring

function push() {
  invoke("set_drag_offset", { x: offX, y: offY }).catch(() => {});
}

function schedulePush() {
  if (frame) return;
  frame = requestAnimationFrame(() => {
    frame = 0;
    push();
  });
}

function settle(now) {
  let dt = (now - settle.last) / 1000;
  settle.last = now;
  if (dt > MAX_STEP) dt = MAX_STEP;

  // Hooke's law with viscous damping, integrated semi-implicitly. Scalars only:
  // this runs every frame, so nothing is allocated here.
  velX += (-STIFFNESS * offX - DAMPING * velX) * dt;
  velY += (-STIFFNESS * offY - DAMPING * velY) * dt;
  offX += velX * dt;
  offY += velY * dt;

  const atRest =
    Math.abs(offX) < 0.3 &&
    Math.abs(offY) < 0.3 &&
    Math.abs(velX) < 5 &&
    Math.abs(velY) < 5;

  if (atRest) {
    offX = offY = velX = velY = 0;
    frame = 0;
    push();
    return;
  }
  push();
  frame = requestAnimationFrame(settle);
}

function springBack() {
  if (frame) cancelAnimationFrame(frame);
  settle.last = performance.now();
  frame = requestAnimationFrame(settle);
}

card.addEventListener("pointerdown", (e) => {
  if (e.button !== 0 || editing) return;
  if (e.target === rename || e.target === anchor) return;

  // Grabbing mid-flight takes over from the spring rather than fighting it.
  if (frame) {
    cancelAnimationFrame(frame);
    frame = 0;
  }
  velX = 0;
  velY = 0;

  armed = true;
  startX = e.screenX;
  startY = e.screenY;
  baseX = offX;
  baseY = offY;
  card.setPointerCapture(e.pointerId);
});

card.addEventListener("pointermove", (e) => {
  if (!armed) return;

  // Screen coordinates, not client: the window moves with the cursor, so the
  // cursor's position *within* the window barely changes while dragging.
  const dx = e.screenX - startX;
  const dy = e.screenY - startY;

  if (!dragging) {
    if (Math.abs(dx) < THRESHOLD && Math.abs(dy) < THRESHOLD) return;
    dragging = true;
    card.classList.add("dragging");
  }

  offX = baseX + dx;
  offY = baseY + dy;
  schedulePush();
});

function endDrag(e) {
  if (!armed) return;
  armed = false;
  if (card.hasPointerCapture(e.pointerId)) card.releasePointerCapture(e.pointerId);
  if (!dragging) return; // it was a click, not a drag
  dragging = false;
  card.classList.remove("dragging");
  springBack();
}

card.addEventListener("pointerup", endDrag);
card.addEventListener("pointercancel", endDrag);

/* ------------------------------------------------------------------ wiring */

listen("gru://state", (event) => render(event.payload));
invoke("get_state").then(render).catch(() => {});
