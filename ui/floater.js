const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const pill = document.getElementById("pill");
const name = document.getElementById("name");
const rename = document.getElementById("rename");
const measure = document.getElementById("measure");

/* Must match .pill's horizontal padding in style.css. */
const PAD = 14;

/* Reduce Motion turns the capsule's width morph into an instant resize. */
const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)");

let current = null;
let editing = false;
let shownId = null;
let requestedWidth = 0;

/* ------------------------------------------------------------------ render */

/* The pill is text-sized: measure the rendered name and ask the window to be
   exactly that wide. Rust clamps to sane bounds; past the cap, CSS ellipsis
   takes over. */
function fit(text) {
  measure.textContent = text;
  const width = Math.ceil(measure.getBoundingClientRect().width) + PAD * 2;
  if (width !== requestedWidth) {
    requestedWidth = width;
    invoke("resize_pill", { width, instant: reducedMotion.matches }).catch(
      () => {},
    );
  }
}

function render(state) {
  document.documentElement.dataset.theme = state.appearance;
  const session = state.focused;
  if (!session) return;
  current = session;

  if (editing) return;

  name.textContent = session.name;
  fit(session.name);

  // Crossfade only when the session actually changed, not on every tick.
  if (session.sessionId !== shownId) {
    shownId = session.sessionId;
    name.classList.remove("swap");
    void name.offsetWidth; // reflow, so the animation restarts
    name.classList.add("swap");
  }
}

/* ------------------------------------------------------------------ rename */

function beginEdit() {
  if (!current || editing) return;
  editing = true;
  // The pill normally never takes focus (accept_first_mouse delivers clicks
  // without it), so typing needs the window focused explicitly.
  invoke("focus_floater").catch(() => {});
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

/* --------------------------------------------------- drag, throw, and lock */

// Displacement from the anchored position. The Rust side adds this to wherever
// the pill is anchored, so the pill keeps tracking the terminal window even
// while it is being held aside. On release the throw's momentum picks the new
// anchor (commit_drag) and the spring carries the pill into it.
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
let lastMoveT = 0;

// Underdamped: critical damping for k=220 is ~29.7, so 24 gives a little
// overshoot — the rubber-band snap, without the wobble of a bouncier spring.
const STIFFNESS = 220;
const DAMPING = 24;
const THRESHOLD = 3; // px of travel before a click becomes a drag
const MAX_STEP = 1 / 30; // clamp dt so a stalled frame can't explode the spring
const MOMENTUM = 0.14; // seconds of release velocity projected into the throw

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

function springTo() {
  if (frame) cancelAnimationFrame(frame);
  settle.last = performance.now();
  frame = requestAnimationFrame(settle);
}

pill.addEventListener("pointerdown", (e) => {
  if (e.button !== 0 || editing) return;
  if (e.target === rename) return;

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
  lastMoveT = e.timeStamp;
  pill.setPointerCapture(e.pointerId);
});

pill.addEventListener("pointermove", (e) => {
  if (!armed) return;

  // Screen coordinates, not client: the window moves with the cursor, so the
  // cursor's position *within* the window barely changes while dragging.
  const dx = e.screenX - startX;
  const dy = e.screenY - startY;

  if (!dragging) {
    if (Math.abs(dx) < THRESHOLD && Math.abs(dy) < THRESHOLD) return;
    dragging = true;
    pill.classList.add("dragging");
  }

  const nx = baseX + dx;
  const ny = baseY + dy;

  // Track release velocity for the throw, smoothed against pointer jitter.
  const dt = (e.timeStamp - lastMoveT) / 1000;
  if (dt > 0 && dt < 0.1) {
    velX = velX * 0.2 + ((nx - offX) / dt) * 0.8;
    velY = velY * 0.2 + ((ny - offY) / dt) * 0.8;
  }
  lastMoveT = e.timeStamp;

  offX = nx;
  offY = ny;
  schedulePush();
});

function endDrag(e) {
  if (!armed) return;
  armed = false;
  if (pill.hasPointerCapture(e.pointerId)) pill.releasePointerCapture(e.pointerId);
  if (!dragging) return; // it was a click, not a drag
  dragging = false;
  pill.classList.remove("dragging");

  // Where the pill lands decides where it locks: Rust picks the anchor
  // nearest the throw's projected end point and answers with the pill's
  // displacement from that new anchor, which the spring then closes.
  invoke("commit_drag", {
    x: offX,
    y: offY,
    px: offX + velX * MOMENTUM,
    py: offY + velY * MOMENTUM,
  })
    .then((residual) => {
      offX = residual.x;
      offY = residual.y;
      springTo();
    })
    .catch(springTo);
}

pill.addEventListener("pointerup", endDrag);
pill.addEventListener("pointercancel", endDrag);

/* ------------------------------------------------------------------ wiring */

listen("lanyard://state", (event) => render(event.payload));
invoke("get_state").then(render).catch(() => {});
