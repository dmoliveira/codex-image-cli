(function () {
  const key = "codex-image-theme";
  const button = document.querySelector("[data-theme-toggle]");
  let saved = null;
  try { saved = window.localStorage.getItem(key); } catch (_) { /* Storage can be unavailable in hardened browsers. */ }
  if (saved === "dark" || saved === "light") document.body.dataset.theme = saved;
  if (!button) return;
  const update = function () {
    const dark = document.body.dataset.theme === "dark";
    button.setAttribute("aria-pressed", String(dark));
    button.textContent = dark ? "☀ Light" : "◐ Dark";
  };
  button.addEventListener("click", function () {
    const next = document.body.dataset.theme === "dark" ? "light" : "dark";
    document.body.dataset.theme = next;
    try { window.localStorage.setItem(key, next); } catch (_) { /* The toggle still works for this page view. */ }
    update();
  });
  update();
}());
