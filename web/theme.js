// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

// A separate script without defer, so the page never flashes in the wrong theme.
try { const t = localStorage.getItem("mnemo_theme"); if (t === "light" || t === "dark") document.documentElement.dataset.theme = t; } catch {}
