// A separate script without defer, so the page never flashes in the wrong theme.
try { const t = localStorage.getItem("mnemo_theme"); if (t === "light" || t === "dark") document.documentElement.dataset.theme = t; } catch {}
