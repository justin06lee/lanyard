// Presentation helpers shared by the floater and the sessions panel.
// A classic script rather than an ES module, so it works the same under Tauri's
// asset protocol without depending on module MIME handling.
window.lanyard = (() => {
  /** Collapses Claude Code's status into the three we render differently. */
  function statusOf(session) {
    const status = session.status;
    return status === "busy" || status === "waiting" ? status : "idle";
  }

  /** Single character for the leading tile: first letter or digit of the name. */
  function monogram(name) {
    const match = (name || "").match(/[a-z0-9]/i);
    return match ? match[0].toUpperCase() : "•";
  }

  /** Claude's own summary of the work, else where the session is running. */
  function describe(session) {
    if (session.aiTitle) return session.aiTitle;
    const path = session.subpath
      ? `${session.repo}/${session.subpath}`
      : session.repo;
    return path === session.name ? "" : path;
  }

  return { statusOf, monogram, describe };
})();
