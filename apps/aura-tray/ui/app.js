const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
let current = null;

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
  toastTimer = setTimeout(() => (t.className = ""), isError ? 5000 : 2500);
}

async function run(button, label, fn) {
  const old = button.textContent;
  button.disabled = true;
  button.textContent = label;
  try {
    await fn();
  } catch (e) {
    toast(String(e), true);
  } finally {
    button.disabled = false;
    button.textContent = old;
  }
}

function ago(ms) {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return new Date(ms).toLocaleDateString();
}

function render(s) {
  current = s;
  const where = s.lan_ip ? `${s.lan_ip}:${s.listen_port}` : `port ${s.listen_port}`;
  $("device").textContent = `${s.device_name} · ${where}`;
  const inGroup = !!s.group;
  $("setup").hidden = inGroup;
  $("group").hidden = !inGroup;

  const pill = $("pill");
  if (inGroup) {
    const others = s.members.filter((m) => !m.is_self);
    const online = others.filter((m) => m.online).length;
    pill.textContent = s.paused ? "Paused" : `${online}/${others.length} online`;
    pill.className = "pill" + (online > 0 && !s.paused ? " ok" : "");
    renderGroup(s);
  } else {
    pill.textContent = "No group";
    pill.className = "pill";
    renderFound(s);
  }
}

function renderFound(s) {
  const box = $("found");
  const selected = box.querySelector("input:checked")?.value;
  box.replaceChildren();
  if (s.discovered_groups.length === 0) {
    box.append(el("div", { class: "empty" }, "Looking for groups on the network…"));
    return;
  }
  for (const g of s.discovered_groups) {
    const input = el("input", { type: "radio", name: "group", value: g.group_id });
    if (g.group_id === selected || s.discovered_groups.length === 1) input.checked = true;
    box.append(el("label", {}, input, g.name, el("small", {}, g.devices.join(", "))));
  }
}

function renderGroup(s) {
  $("group-name").textContent = s.group.name;
  $("rotate-info").textContent =
    `Set ${ago(s.group.passphrase_changed_ms)} by ${s.group.passphrase_changed_by}. ` +
    "PCs already in the group stay in; new PCs will need the new passphrase.";
  $("pause").checked = s.paused;

  const members = $("members");
  members.replaceChildren();
  const sorted = [...s.members].sort((a, b) => b.is_self - a.is_self || a.name.localeCompare(b.name));
  for (const m of sorted) {
    const right = m.is_self
      ? el("span", { class: "badge" }, "this PC")
      : el(
          "button",
          {
            class: "small danger subtle",
            onclick: (ev) => {
              if (!confirm(`Remove ${m.name} from the group? It will stop receiving clipboard entries.`)) return;
              run(ev.target, "Removing…", async () => {
                await invoke("remove_device", { deviceId: m.device_id });
                // The removed PC still knows the passphrase; nudge towards changing it.
                $("rotate").open = true;
                $("rotate-pass").focus();
                toast(`${m.name} removed. Change the passphrase so it cannot join again.`);
              });
            },
          },
          "Remove",
        );
    members.append(
      el(
        "li",
        {},
        el("span", { class: "dot" + (m.online ? " on" : "") }),
        el("div", { class: "grow" }, el("div", { class: "main" }, m.name), el("div", { class: "sub" }, m.online ? "online" : "offline")),
        right,
      ),
    );
  }

  const hist = $("history");
  hist.replaceChildren();
  $("history-empty").hidden = s.history.length > 0;
  for (const h of s.history) {
    hist.append(
      el(
        "li",
        {},
        el(
          "div",
          { class: "grow" },
          el("div", { class: "main" }, h.preview || "(empty)"),
          el("div", { class: "sub" }, `${h.origin} · ${ago(h.at_ms)}`),
        ),
        el(
          "button",
          {
            class: "small",
            title: "Put this back on the clipboard of every PC",
            onclick: (ev) => run(ev.target, "…", async () => {
              await invoke("recopy", { id: h.id });
              toast("Copied");
            }),
          },
          "Copy",
        ),
      ),
    );
  }
}

$("join-btn").addEventListener("click", (ev) => {
  const groupId = $("found").querySelector("input:checked")?.value ?? null;
  const address = $("join-address").value.trim() || null;
  const passphrase = $("join-pass").value;
  run(ev.target, "Joining…", async () => {
    // A typed address wins over the selected group.
    await invoke("join_group", { groupId: address ? null : groupId, address, passphrase });
    $("join-pass").value = "";
    toast("Joined!");
  });
});

$("create-btn").addEventListener("click", (ev) => {
  const name = $("create-name").value.trim();
  const pass = $("create-pass").value;
  if (pass !== $("create-pass2").value) return toast("Passphrases do not match", true);
  run(ev.target, "Creating…", async () => {
    await invoke("create_group", { name, passphrase: pass });
    $("create-pass").value = $("create-pass2").value = "";
    toast("Group created");
  });
});

$("pause").addEventListener("change", (ev) => invoke("set_paused", { paused: ev.target.checked }).catch((e) => toast(String(e), true)));

$("rotate-btn").addEventListener("click", (ev) => {
  const pass = $("rotate-pass").value;
  if (pass !== $("rotate-pass2").value) return toast("Passphrases do not match", true);
  run(ev.target, "Changing…", async () => {
    await invoke("rotate_passphrase", { passphrase: pass });
    $("rotate-pass").value = $("rotate-pass2").value = "";
    $("rotate").open = false;
    toast("Passphrase changed on every PC");
  });
});

$("leave-btn").addEventListener("click", (ev) => {
  if (!confirm("Leave this group? This PC will stop syncing and its history will be cleared.")) return;
  run(ev.target, "Leaving…", () => invoke("leave_group"));
});

listen("status", (e) => render(e.payload));
invoke("get_status").then(render);
// Keep "x min ago" fresh.
setInterval(() => current && current.group && renderGroup(current), 30000);
