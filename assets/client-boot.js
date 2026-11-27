(() => {
  const root = document.documentElement;
  root.classList.add("js-enabled");
  const fallback = root.dataset.theme;
  const key = "rustpost-theme";
  let select;

  function apply(preference) {
    const explicit = preference === "light" || preference === "dark" ? preference : "";
    root.dataset.theme = explicit || fallback;
    if (select) select.value = explicit;
  }

  function readPreference() {
    try {
      return localStorage.getItem(key);
    } catch {
      return null;
    }
  }

  // This script runs before the styles are parsed, using the existing same-origin CSP.
  apply(readPreference());

  document.addEventListener("DOMContentLoaded", () => {
    select = document.getElementById("browser-theme");
    if (!select) return;

    // Re-read in case another tab changed the preference while this page loaded.
    apply(readPreference());
    select.parentElement.hidden = false;

    select.addEventListener("change", () => {
      const preference = select.value;
      apply(preference);
      try {
        if (preference) localStorage.setItem(key, preference);
        else localStorage.removeItem(key);
      } catch {
        // Keep the choice on this page when the browser disallows storage.
      }
    });
  });

  window.addEventListener("storage", event => {
    if (event.key === key || event.key === null) {
      try {
        if (event.storageArea === localStorage) apply(readPreference());
      } catch {
        // The browser may restrict access to storage.
      }
    }
  });

  window.addEventListener("pageshow", event => {
    if (event.persisted) apply(readPreference());
  });
})();
