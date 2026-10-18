"use strict";

const $ = (id) => document.getElementById(id);
let items = [];
let lastData = null;
let events = null;

function el(tag, attrs = {}, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") e.className = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v);
  }
  for (const c of children) e.append(c);
  return e;
}

let toastTimer;
function toast(msg, isError = false) {
  const t = $("toast");
  t.textContent = msg;
  t.className = "show" + (isError ? " error" : "");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.className = ""), isError ? 5000 : 2000);
}

async function api(path, body) {
  const res = await fetch(path, body === undefined
    ? { credentials: "same-origin" }
    : {
        method: "POST",
        credentials: "same-origin",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) {
    const err = new Error(data.error || `error ${res.status}`);
    err.status = res.status;
    throw err;
  }
  return data;
}

function ago(ms) {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return new Date(ms).toLocaleDateString();
}

// ------------------------------------------------------------ copying

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // Older browsers: copy through a temporary text area.
    const ta = el("textarea", { readonly: "" });
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.append(ta);
    ta.select();
    const ok = document.execCommand("copy");
    ta.remove();
    if (!ok) throw new Error("this browser blocked copying");
  }
}

async function copyItem(item) {
  try {
    if (item.kind === "image") {
      const blob = fetch(imageUrl(item)).then((r) => r.blob());
      await navigator.clipboard.write([new ClipboardItem({ "image/png": blob })]);
    } else {
      await copyText(item.text);
    }
    toast("Copied");
  } catch (e) {
    toast(item.kind === "image" ? "This browser can't copy images; use Save" : String(e.message || e), true);
  }
}

const imageUrl = (item) => `/api/entries/${item.id}/image`;

// ------------------------------------------------------------ rendering

function show(view) {
  $("login").hidden = view !== "login";
  $("app").hidden = view !== "app";
  $("live").hidden = view !== "app";
}

function itemBody(item, big) {
  if (item.kind === "image") {
    return el("img", { src: imageUrl(item), alt: `image ${item.width}×${item.height}`, loading: "lazy" });
  }
  return el("div", { class: big ? "text" : "main" }, item.text || "");
}

function render(data) {
  lastData = data;
  items = data.items;
  $("where").textContent = data.group ? `${data.group} · served by ${data.served_by}` : "Shared clipboard";

  const cur = items[0];
  const box = $("current");
  box.replaceChildren();
  if (!cur) {
    $("current-meta").textContent = "";
    box.append(el("p", { class: "empty" }, "Copy something on one of your PCs."));
  } else {
    $("current-meta").textContent = `${cur.origin} · ${ago(cur.at_ms)}`;
    const actions = el("div", { class: "actions" },
      el("button", { class: "primary", type: "button", onclick: () => copyItem(cur) }, "Copy"));
    if (cur.kind === "image") {
      actions.append(el("a", { class: "button", href: imageUrl(cur), download: "aura-image.png" }, "Save"));
    }
    box.append(itemBody(cur, true), actions);
  }

  const list = $("history");
  list.replaceChildren();
  $("history-empty").hidden = items.length > 1;
  for (const item of items.slice(1)) {
    list.append(el("li", {},
      el("div", { class: "grow" },
        itemBody(item, false),
        el("div", { class: "sub" }, `${item.origin} · ${ago(item.at_ms)}`)),
      el("button", { class: "small", type: "button", onclick: () => copyItem(item) }, "Copy")));
  }
}

async function load() {
  try {
    render(await api("/api/history"));
    show("app");
    listen();
  } catch (e) {
    if (e.status === 401) {
      stopListening();
      show("login");
    } else {
      toast(e.message, true);
    }
  }
}

// ------------------------------------------------------------ live updates

function listen() {
  if (events) return;
  events = new EventSource("/api/events");
  events.addEventListener("changed", () => load());
  // EventSource reconnects by itself (e.g. when another PC takes over serving).
  events.onerror = () => ($("live").textContent = "Reconnecting…");
  events.onopen = () => ($("live").textContent = "Live");
}

function stopListening() {
  if (events) events.close();
  events = null;
}

// ------------------------------------------------------------ actions

$("login-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const button = ev.target.querySelector("button");
  button.disabled = true;
  try {
    await api("/api/login", { passphrase: $("pass").value });
    $("pass").value = "";
    await load();
  } catch (e) {
    toast(e.message, true);
  } finally {
    button.disabled = false;
  }
});

$("send-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const text = $("send-text").value;
  if (!text.trim()) return;
  const button = ev.target.querySelector("button");
  button.disabled = true;
  try {
    await api("/api/send", { text });
    $("send-text").value = "";
    toast("Sent to your PCs");
  } catch (e) {
    toast(e.message, true);
  } finally {
    button.disabled = false;
  }
});

$("logout").addEventListener("click", async () => {
  await api("/api/logout", {}).catch(() => {});
  stopListening();
  show("login");
});

// A QR code from a PC carries a short-lived login token in the URL fragment.
async function start() {
  const token = new URLSearchParams(location.hash.slice(1)).get("t");
  if (token) {
    history.replaceState(null, "", "/");
    try {
      await api("/api/login", { qr_token: token });
    } catch (e) {
      toast(e.message, true);
    }
  }
  await load();
}

document.addEventListener("visibilitychange", () => {
  if (!document.hidden && !$("app").hidden) load();
});
// Keep the "x min ago" labels fresh.
setInterval(() => lastData && !$("app").hidden && render(lastData), 60000);
start();
