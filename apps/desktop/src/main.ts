import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// ---------------------------------------------------------------------------
// Connection config (persisted in localStorage; handed to every invoke).
// ---------------------------------------------------------------------------

interface Conn {
  base: string;
  token: string;
}

const CONN_KEY = "eternai.conn";
const PERSONALITY_KEY = "eternai.personality";

interface PersonalitySetting {
  value: string;
  updated_at: number;
}

function loadConn(): Conn {
  try {
    const raw = localStorage.getItem(CONN_KEY);
    if (raw) {
      const c = JSON.parse(raw) as Partial<Conn>;
      return { base: (c.base ?? "").trim(), token: (c.token ?? "").trim() };
    }
  } catch {
    /* fall through to default */
  }
  return { base: "http://127.0.0.1:8090", token: "" };
}

let conn = loadConn();
let personality = loadPersonality();

function loadPersonality(): PersonalitySetting {
  try {
    const raw = localStorage.getItem(PERSONALITY_KEY);
    if (raw) {
      const saved = JSON.parse(raw) as Partial<PersonalitySetting>;
      return {
        value: typeof saved.value === "string" ? saved.value : "",
        updated_at: Number.isSafeInteger(saved.updated_at) ? Number(saved.updated_at) : 0,
      };
    }
  } catch {
    /* fall through to the empty local default */
  }
  return { value: "", updated_at: 0 };
}

function savePersonalityLocally(setting: PersonalitySetting) {
  personality = setting;
  localStorage.setItem(PERSONALITY_KEY, JSON.stringify(setting));
}

async function syncPersonality(): Promise<PersonalitySetting> {
  const canonical = (await invoke("personality_sync", {
    conn,
    value: personality.value,
    updatedAt: personality.updated_at,
  })) as PersonalitySetting;
  if (typeof canonical.value !== "string" || !Number.isSafeInteger(canonical.updated_at)) {
    throw new Error("gateway returned an invalid personality setting");
  }
  savePersonalityLocally(canonical);
  const input = document.getElementById("set-personality") as HTMLTextAreaElement | null;
  if (input && document.activeElement !== input) input.value = canonical.value;
  return canonical;
}

function saveConn() {
  localStorage.setItem(CONN_KEY, JSON.stringify(conn));
}

// ---------------------------------------------------------------------------
// The gateway is canonical; local turns remain available if recording fails.
// ---------------------------------------------------------------------------

interface ChatMessage {
  role: "system" | "user" | "assistant" | "tool";
  content: string;
  name?: string;
  tool_calls?: unknown;
  tool_call_id?: string;
}

interface StoredTurn {
  id: string;
  messages: Array<{
    role: ChatMessage["role"];
    content: string;
    name?: string | null;
    tool_calls?: string | null;
    tool_call_id?: string | null;
  }>;
}

const history: ChatMessage[] = [];
const pendingTurns = new Map<string, ChatMessage[]>();

// ---------------------------------------------------------------------------
// Tab switching
// ---------------------------------------------------------------------------

const tabs = Array.from(document.querySelectorAll<HTMLButtonElement>(".tab"));
const views = new Map<string, HTMLElement>(
  Array.from(document.querySelectorAll<HTMLElement>(".view")).map((v) => [
    v.id.replace("view-", ""),
    v,
  ]),
);

tabs.forEach((tab) =>
  tab.addEventListener("click", () => {
    tabs.forEach((t) => t.classList.toggle("active", t === tab));
    const name = tab.dataset.view ?? "chat";
    views.forEach((el, key) => el.classList.toggle("active", key === name));
    if (name === "journal") refreshJournal();
    if (name === "settings") {
      refreshDevices();
      void refreshPersonalityFromGateway();
    }
  }),
);

// ---------------------------------------------------------------------------
// Engine status chip (poll /health via the Rust shell)
// ---------------------------------------------------------------------------

const chip = document.getElementById("engine-chip")!;
const chipLabel = document.getElementById("engine-label")!;
const modelChip = document.getElementById("model-chip")!;

function setChip(cls: string, label: string) {
  chip.className = `chip ${cls}`;
  chipLabel.textContent = label;
}

async function pollStatus() {
  if (!conn.base) {
    setChip("chip-offline", "no gateway configured");
    modelChip.textContent = "—";
    return;
  }
  try {
    const h = (await invoke("gateway_health", { conn })) as {
      engine_ready?: boolean;
      engine_ready_s?: boolean;
      waking?: boolean;
      model?: string | null;
    };
    if (h.engine_ready_s) {
      setChip("chip-online", h.waking ? "engine waking" : "engine ready");
    } else if (h.engine_ready) {
      setChip("chip-degraded", "engine loading");
    } else {
      setChip("chip-offline", "engine down");
    }
    modelChip.textContent = h.model ?? "no model";
    modelChip.title = h.model ?? "No model loaded";
  } catch {
    setChip("chip-offline", "gateway unreachable");
    modelChip.textContent = "—";
  }
}

pollStatus();
setInterval(pollStatus, 5000);

// ---------------------------------------------------------------------------
// Chat
// ---------------------------------------------------------------------------

const chatLog = document.getElementById("chat-log")!;
const chatForm = document.getElementById("chat-form") as HTMLFormElement;
const chatInput = document.getElementById("chat-input") as HTMLTextAreaElement;
const chatSend = document.getElementById("chat-send") as HTMLButtonElement;

function addBubble(cls: string, text: string): HTMLElement {
  const div = document.createElement("div");
  div.className = `msg ${cls}`;
  div.textContent = text;
  chatLog.appendChild(div);
  chatLog.scrollTop = chatLog.scrollHeight;
  return div;
}

let streaming = false;
let sending = false;
let refreshingHistory = false;
let historySyncError = "";
let lastHistorySnapshot: string | null = null;

interface DeltaEvent {
  id: string;
  delta: string;
}
interface DoneEvent {
  id: string;
}
interface ErrorEvent {
  id: string;
  message: string;
}
interface ToolEvent {
  id: string;
  tool: string;
  ok: boolean;
  error?: string;
}

// Human-readable label for a tool step, so the chat shows what happened rather
// than the raw tool name.
function stepLabel(ev: ToolEvent): string {
  const verb: Record<string, string> = {
    read_file: "read a file",
    write_file: "wrote a file",
    list_dir: "listed a folder",
    web_search: "searched the web",
    remember: "saved a memory",
    recall: "searched memories",
    journal_read: "read the journal",
  };
  const did = verb[ev.tool] ?? `used ${ev.tool}`;
  return ev.ok ? did : `${did} failed`;
}

async function sendChat(text: string) {
  if (streaming || sending) return;
  const id = crypto.randomUUID();
  streaming = true;
  sending = true;
  chatSend.disabled = true;

  const pendingMessages: ChatMessage[] = [{ role: "user", content: text }];
  pendingTurns.set(id, pendingMessages);
  history.push(pendingMessages[0]);
  addBubble("msg-user", text);

  const bubble = addBubble("msg-assistant", "");
  let assistantText = "";
  let finished = false;

  // One-shot listeners for this stream's correlation id.
  const offs = await Promise.all([
    listen<DeltaEvent>("chat://delta", (e) => {
      if (e.payload.id !== id) return;
      assistantText += e.payload.delta;
      bubble.textContent = assistantText;
      // Tool chips may have landed under the reply; keep the prose last.
      if (chatLog.lastElementChild !== bubble) chatLog.appendChild(bubble);
      chatLog.scrollTop = chatLog.scrollHeight;
    }),
    listen<ToolEvent>("chat://tool", (e) => {
      if (e.payload.id !== id) return;
      const chip = addBubble("msg-tool", stepLabel(e.payload));
      if (!e.payload.ok) {
        chip.classList.add("failed");
        chip.title = e.payload.error ?? "the tool reported a failure";
      }
    }),
    listen<DoneEvent>("chat://done", (e) => {
      if (e.payload.id === id) done();
    }),
    listen<ErrorEvent>("chat://error", (e) => {
      if (e.payload.id !== id) return;
      addBubble("msg-error", e.payload.message);
      done();
    }),
  ]);

  const done = () => {
    if (finished) return;
    finished = true;
    offs.forEach((f) => f());
    if (assistantText) {
      const assistant: ChatMessage = { role: "assistant", content: assistantText };
      history.push(assistant);
      pendingMessages.push(assistant);
    }
    else if (bubble.parentElement) bubble.remove();
  };

  try {
    let activePersonality = personality.value;
    try {
      activePersonality = (await syncPersonality()).value;
    } catch (err) {
      const status = document.getElementById("set-personality-status");
      if (status) {
        status.className = "status err";
        status.textContent = `Using the saved local personality; sync failed: ${String(err)}`;
      }
    }
    await invoke("chat_send", {
      conn,
      req: { id, messages: history, personality: activePersonality },
    });
    done();
    streaming = false;
    sending = false;
    chatSend.disabled = false;
    chatInput.focus();
    await refreshChatHistory();
  } catch (err) {
    addBubble("msg-error", String(err));
    done();
    streaming = false;
    sending = false;
    chatSend.disabled = false;
    chatInput.focus();
    await refreshChatHistory();
  }
}

function storedMessage(message: StoredTurn["messages"][number]): ChatMessage {
  let toolCalls: unknown;
  if (message.tool_calls) {
    try {
      toolCalls = JSON.parse(message.tool_calls);
    } catch {
      addBubble("msg-notice", "A stored tool call could not be replayed.");
    }
  }
  return {
    role: message.role,
    content: message.content,
    ...(message.name ? { name: message.name } : {}),
    ...(toolCalls !== undefined ? { tool_calls: toolCalls } : {}),
    ...(message.tool_call_id ? { tool_call_id: message.tool_call_id } : {}),
  };
}

function renderStoredTurns(turns: StoredTurn[]) {
  const storedIds = new Set(turns.map((turn) => turn.id));
  for (const id of storedIds) pendingTurns.delete(id);

  const messages = turns.flatMap((turn) => turn.messages.map(storedMessage));
  for (const pending of pendingTurns.values()) messages.push(...pending);

  history.splice(0, history.length, ...messages);
  chatLog.replaceChildren();
  for (const message of messages) {
    if (message.role === "user") {
      addBubble("msg-user", message.content);
    } else if (message.role === "assistant" && message.content.trim()) {
      addBubble("msg-assistant", message.content);
    } else if (message.role === "tool") {
      const ok = !/"ok"\s*:\s*false/i.test(message.content);
      const chip = addBubble(
        "msg-tool",
        stepLabel({ id: "", tool: message.name ?? "tool", ok }),
      );
      if (!ok) {
        chip.classList.add("failed");
        chip.title = message.content;
      }
    }
  }
  chatLog.scrollTop = chatLog.scrollHeight;
}

async function refreshChatHistory() {
  if (streaming || sending || refreshingHistory || !conn.base) return;
  refreshingHistory = true;
  try {
    const response = (await invoke("chat_history", { conn })) as { turns: StoredTurn[] };
    const turns = response.turns ?? [];
    const snapshot = JSON.stringify(turns);
    if (snapshot !== lastHistorySnapshot) {
      renderStoredTurns(turns);
      lastHistorySnapshot = snapshot;
    }
    historySyncError = "";
  } catch (err) {
    const message = String(err);
    if (message !== historySyncError) {
      addBubble("msg-notice", `Chat history sync failed: ${message}`);
      historySyncError = message;
    }
  } finally {
    refreshingHistory = false;
  }
}

chatForm.addEventListener("submit", (e) => {
  e.preventDefault();
  if (streaming || sending) return;
  const text = chatInput.value.trim();
  if (!text) return;
  chatInput.value = "";
  sendChat(text);
});

void refreshChatHistory();
setInterval(refreshChatHistory, 3000);

// Non-fatal warnings from the shell (e.g. a reply that streamed fine but could
// not be recorded for the journal). Registered once, outside any single
// stream's listeners, because the shell emits these after `chat://done`.
void listen<{ id: string; message: string }>("chat://notice", (e) => {
  addBubble("msg-notice", e.payload.message);
});

chatInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && !e.shiftKey) {
    e.preventDefault();
    chatForm.requestSubmit();
  }
});

// ---------------------------------------------------------------------------
// Journal browser
// ---------------------------------------------------------------------------

interface JournalFile {
  name: string;
  kind: string;
  size_bytes: number;
  modified_ms: number;
}

const journalList = document.getElementById("journal-list") as HTMLUListElement;
const readerTitle = document.getElementById("journal-reader-title")!;
const readerBody = document.getElementById("journal-reader-body")!;

document.getElementById("journal-refresh")!.addEventListener("click", refreshJournal);

async function refreshJournal() {
  journalList.replaceChildren();
  try {
    const res = (await invoke("journal_list", { conn })) as {
      files: JournalFile[];
    };
    if (!res.files.length) {
      const li = document.createElement("li");
      li.textContent = "No journal entries yet.";
      journalList.appendChild(li);
      return;
    }
    res.files.forEach((f) => {
      const li = document.createElement("li");
      const name = document.createElement("div");
      name.className = "j-name";
      const kind = document.createElement("span");
      kind.className = "journal-kind";
      kind.textContent = f.kind === "daily" ? "\u{1F4C5}" : "\u{1F4F1}";
      name.appendChild(kind);
      name.appendChild(document.createTextNode(f.name));
      const meta = document.createElement("div");
      meta.className = "j-meta";
      const when = new Date(f.modified_ms).toLocaleString();
      meta.textContent = `${f.kind} · ${f.size_bytes.toLocaleString()} B · ${when}`;
      li.appendChild(name);
      li.appendChild(meta);
      li.addEventListener("click", () => {
        journalList
          .querySelectorAll("li.selected")
          .forEach((x) => x.classList.remove("selected"));
        li.classList.add("selected");
        openJournal(f.name);
      });
      journalList.appendChild(li);
    });
  } catch (err) {
    const li = document.createElement("li");
    li.textContent = String(err);
    journalList.appendChild(li);
  }
}

async function openJournal(name: string) {
  readerTitle.textContent = name;
  readerBody.textContent = "…";
  try {
    const res = (await invoke("journal_read", { conn, name })) as {
      content?: string;
      error?: { message: string };
    };
    readerBody.textContent = res.content ?? res.error?.message ?? "(empty)";
  } catch (err) {
    readerBody.textContent = String(err);
  }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

const setBase = document.getElementById("set-base") as HTMLInputElement;
const setToken = document.getElementById("set-token") as HTMLInputElement;
const setStatus = document.getElementById("set-status")!;
const setPersonality = document.getElementById("set-personality") as HTMLTextAreaElement;
const setPersonalityStatus = document.getElementById("set-personality-status")!;

setBase.value = conn.base;
setToken.value = conn.token;
setPersonality.value = personality.value;

async function refreshPersonalityFromGateway() {
  setPersonalityStatus.className = "status";
  setPersonalityStatus.textContent = "Syncing…";
  try {
    const setting = await syncPersonality();
    setPersonalityStatus.className = "status ok";
    setPersonalityStatus.textContent = setting.updated_at
      ? "Personality synced."
      : "No shared personality is set.";
  } catch (err) {
    setPersonalityStatus.className = "status err";
    setPersonalityStatus.textContent = `Using the saved local personality; sync failed: ${String(err)}`;
  }
}

document.getElementById("set-personality-save")!.addEventListener("click", async () => {
  const value = setPersonality.value.trim();
  if (Array.from(value).length > 2000 || new TextEncoder().encode(value).length > 8 * 1024) {
    setPersonalityStatus.className = "status err";
    setPersonalityStatus.textContent = "Personality must be 2000 characters or fewer.";
    return;
  }
  const updated_at = Math.max(Date.now(), personality.updated_at + 1);
  savePersonalityLocally({ value, updated_at });
  setPersonalityStatus.className = "status";
  setPersonalityStatus.textContent = "Saved locally; syncing…";
  try {
    const setting = await syncPersonality();
    setPersonalityStatus.className = "status ok";
    setPersonalityStatus.textContent = setting.updated_at === updated_at && setting.value === value
      ? "Personality saved and synced."
      : "A newer personality setting was already synced; using that setting.";
  } catch (err) {
    setPersonalityStatus.className = "status err";
    setPersonalityStatus.textContent = `Saved locally; sync pending: ${String(err)}`;
  }
});

document.getElementById("set-save")!.addEventListener("click", () => {
  conn = { base: setBase.value.trim().replace(/\/+$/, ""), token: setToken.value.trim() };
  saveConn();
  setStatus.className = "status ok";
  setStatus.textContent = "Saved.";
  pollStatus();
  void refreshChatHistory();
  void refreshPersonalityFromGateway();
});

document.getElementById("set-test")!.addEventListener("click", async () => {
  setStatus.className = "status";
  setStatus.textContent = "Testing…";
  const probe: Conn = {
    base: setBase.value.trim().replace(/\/+$/, ""),
    token: setToken.value.trim(),
  };
  try {
    const h = (await invoke("gateway_health", { conn: probe })) as {
      engine_ready_s?: boolean;
      model?: string | null;
    };
    setStatus.className = "status ok";
    setStatus.textContent = h.engine_ready_s
      ? `Gateway reachable, engine ready (${h.model ?? "no model"}).`
      : "Gateway reachable, but the engine is not ready yet.";
  } catch (err) {
    setStatus.className = "status err";
    setStatus.textContent = String(err);
  }
});

// ---------------------------------------------------------------------------
// Pairing (M6)
// ---------------------------------------------------------------------------

const pairStatus = document.getElementById("pair-status")!;
const pairQr = document.getElementById("pair-qr")!;
const pairPayload = document.getElementById("pair-payload")!;
const devicesTable = document.getElementById("devices-table") as HTMLTableElement;
const devicesBody = document.getElementById("devices-body")!;
const pairNetwork = document.getElementById("pair-network") as HTMLSelectElement;
const PAIR_NETWORK_KEY = "eternai.pair-network";

pairNetwork.value = localStorage.getItem(PAIR_NETWORK_KEY) === "tailscale" ? "tailscale" : "lan";
pairNetwork.addEventListener("change", () => {
  localStorage.setItem(PAIR_NETWORK_KEY, pairNetwork.value);
});

interface HealthInfo {
  lan_ip?: string | null;
  tailscale_ip?: string | null;
  tls_port?: number | null;
  cert_pin?: string | null;
}

interface DeviceRow {
  id: string;
  name: string;
  created_ts: number;
  revoked: boolean;
  token_prefix: string;
}

function setPairStatus(cls: string, text: string) {
  pairStatus.className = `status ${cls}`;
  pairStatus.textContent = text;
}

async function generatePairCode() {
  pairQr.hidden = true;
  pairPayload.hidden = true;
  setPairStatus("", "Generating…");
  let health: HealthInfo;
  try {
    health = (await invoke("gateway_health", { conn })) as HealthInfo;
  } catch (err) {
    setPairStatus("err", String(err));
    return;
  }
  const { tls_port: port, cert_pin: pin } = health;
  if (!port || !pin) {
    setPairStatus(
      "err",
      "This gateway has no TLS listener, so there is nothing secure to pair against.\n" +
        "Start the gateway with --tls-listen (see Settings notes) and reload.",
    );
    return;
  }
  const useTailscale = pairNetwork.value === "tailscale";
  const host = useTailscale ? health.tailscale_ip : health.lan_ip;
  if (!host) {
    setPairStatus(
      "err",
      useTailscale
        ? "No Tailscale IPv4 address was detected. Make sure Tailscale is installed and connected on the desktop, then restart the gateway."
        : "No local network address was detected for this gateway.",
    );
    return;
  }
  let pairing: { code: string; expires_ts: number };
  try {
    pairing = (await invoke("pair_create", { conn })) as {
      code: string;
      expires_ts: number;
    };
  } catch (err) {
    setPairStatus("err", String(err));
    return;
  }
  const payload = JSON.stringify({ host, port, cert_pin: pin, code: pairing.code });
  try {
    const qr = (await invoke("pair_qr", { payload })) as { svg: string };
    pairQr.replaceChildren();
    pairQr.insertAdjacentHTML("beforeend", qr.svg);
    pairPayload.textContent = payload;
    pairQr.hidden = false;
    pairPayload.hidden = false;
    const expires = new Date(pairing.expires_ts * 1000).toLocaleTimeString();
    const network = useTailscale ? "Tailscale" : "local Wi-Fi";
    setPairStatus("ok", `Code ${pairing.code} valid until ${expires}. Scan it over ${network}.`);
  } catch (err) {
    setPairStatus("err", String(err));
  }
}

async function refreshDevices() {
  if (!conn.base || !conn.token) return;
  try {
    const res = (await invoke("devices_list", { conn })) as { devices: DeviceRow[] };
    devicesBody.replaceChildren();
    if (!res.devices.length) {
      devicesTable.hidden = true;
      return;
    }
    res.devices.forEach((d) => {
      const tr = document.createElement("tr");
      const name = document.createElement("td");
      name.textContent = d.name;
      if (d.revoked) name.className = "revoked";
      const token = document.createElement("td");
      token.textContent = `${d.token_prefix}…`;
      const when = document.createElement("td");
      when.textContent = new Date(d.created_ts * 1000).toLocaleDateString();
      const action = document.createElement("td");
      if (!d.revoked) {
        const btn = document.createElement("button");
        btn.className = "ghost";
        btn.textContent = "Revoke";
        btn.addEventListener("click", async () => {
          try {
            await invoke("device_revoke", { conn, id: d.id });
            refreshDevices();
          } catch (err) {
            setPairStatus("err", String(err));
          }
        });
        action.appendChild(btn);
      }
      tr.append(name, token, when, action);
      devicesBody.appendChild(tr);
    });
    devicesTable.hidden = false;
  } catch {
    devicesTable.hidden = true;
  }
}

document.getElementById("pair-generate")!.addEventListener("click", generatePairCode);
document.getElementById("devices-refresh")!.addEventListener("click", refreshDevices);

// ---------------------------------------------------------------------------
// Danger zone
// ---------------------------------------------------------------------------

const clearStatus = document.getElementById("history-clear-status")!;

function setClearStatus(cls: string, text: string) {
  clearStatus.className = `status ${cls}`;
  clearStatus.textContent = text;
}

document.getElementById("history-clear")!.addEventListener("click", async () => {
  if (!conn.base) {
    setClearStatus("err", "Save a gateway connection first.");
    return;
  }
  if (!confirm("Delete every stored conversation turn? Journal entries are kept. This cannot be undone.")) {
    return;
  }
  setClearStatus("", "Clearing…");
  try {
    const res = (await invoke("history_clear", { conn })) as {
      cleared?: number;
      error?: { message: string };
    };
    if (res.cleared === undefined) {
      setClearStatus("err", res.error?.message ?? "the gateway did not clear anything");
      return;
    }
    // The in-memory transcript is a copy of what the gateway held; drop it too
    // or the next prompt would resend the turns we just deleted.
    history.length = 0;
    chatLog.replaceChildren();
    setClearStatus("ok", `Deleted ${res.cleared} turn(s). Reconnect the phone so it pulls the empty window.`);
  } catch (err) {
    setClearStatus("err", String(err));
  }
});
