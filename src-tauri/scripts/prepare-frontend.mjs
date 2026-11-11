// Prepares the renderer for the Tauri runtime without modifying the original UI.
//
// 1. Copies temp_fix/out/renderer -> dist-tauri (same HTML/JS/CSS, byte for byte)
// 2. Widens the CSP meta tag so Tauri IPC and the thumbnail protocol are allowed
// 3. Generates src-tauri/generated/bridge.js from the Electron preload, so
//    `window.api` keeps the exact same namespaces, methods and read cache;
//    only the transport changes (ipcRenderer -> Tauri invoke/events)
// 4. Writes docs/rust-port-coverage.md: which IPC channels Rust already serves
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const tauriDir = path.resolve(here, "..");
const repo = path.resolve(tauriDir, "..");
const rendererSrc = path.join(repo, "temp_fix", "out", "renderer");
const preloadSrc = path.join(repo, "temp_fix", "out", "preload", "index.js");
const dist = path.join(repo, "dist-tauri");
const generatedDir = path.join(tauriDir, "generated");

// ---------------------------------------------------------------- 1. copy UI
fs.rmSync(dist, { recursive: true, force: true });
fs.cpSync(rendererSrc, dist, { recursive: true });

// ---------------------------------------------------------------- 2. CSP
const indexPath = path.join(dist, "index.html");
let html = fs.readFileSync(indexPath, "utf8");
const widen = (directive, extra) => {
  const re = new RegExp(`${directive} ([^;"]*)`);
  if (!re.test(html)) throw new Error(`CSP directive ${directive} not found`);
  html = html.replace(re, (_m, value) => `${directive} ${value} ${extra}`);
};
widen("connect-src", "ipc: http://ipc.localhost");
widen("img-src", "http://lbx-thumb.localhost");
widen("img-src", "http://lbx-capture.localhost");
widen("media-src", "http://lbx-capture.localhost");
fs.writeFileSync(indexPath, html);

// Renderer-built thumbnail URLs use the Electron-only `lbx-thumb://` scheme;
// WebView2 reaches the Rust protocol at `http://lbx-thumb.localhost` instead.
for (const file of fs.readdirSync(path.join(dist, "assets"))) {
  if (!file.endsWith(".js")) continue;
  const p = path.join(dist, "assets", file);
  const js = fs.readFileSync(p, "utf8");
  const patched = js.replace("`${THUMBNAIL_PROTOCOL_SCHEME}://${OUTFIT_THUMBNAIL_HOST}/", "`http://lbx-thumb.localhost/${OUTFIT_THUMBNAIL_HOST}/");
  if (patched !== js) fs.writeFileSync(p, patched);
}

// ---------------------------------------------------------------- 3. bridge
let preload = fs.readFileSync(preloadSrc, "utf8").replace(/\r\n/g, "\n");
const start = preload.indexOf("function handler()");
if (start < 0) throw new Error("preload: handler() not found");
preload = preload
  .slice(start)
  .replace(/\/\/# sourceMappingURL=.*$/m, "");

const transport = String.raw`
const __guguInvoke = (cmd, payload) => window.__TAURI_INTERNALS__.invoke(cmd, payload);
const __guguListeners = new Map();
let __guguSubscribed = false;
const __guguSubscribe = () => {
  if (__guguSubscribed || !window.__TAURI__) return;
  __guguSubscribed = true;
  window.__TAURI__.event.listen("gugu:ipc-event", (event) => {
    const { channel, payload } = event.payload;
    const handlers = __guguListeners.get(channel);
    if (!handlers) return;
    for (const handler of [...handlers]) {
      try { handler(null, payload); } catch (error) { console.error(error); }
    }
  });
};
// Rejections keep Electron's message format so renderer error handling is unchanged.
const __guguWrapError = (channel) => (error) => {
  const message = typeof error === "string" ? error : (error && error.message) || String(error);
  const wrapped = new Error("Error invoking remote method '" + channel + "': Error: " + message);
  if (error && error.code) wrapped.code = error.code;
  throw wrapped;
};
// Typed arrays cross the JSON bridge as { __lbxTyped, b64 } (Electron's IPC
// structured clone carried them natively).
const __guguTypedArrays = { Uint8Array, Uint16Array, Uint32Array, Int32Array, Float32Array, Float64Array };
const __guguBytes = (b64) => {
  if (Uint8Array.fromBase64) return Uint8Array.fromBase64(b64);
  const binary = atob(b64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
};
const __guguRevive = (value) => {
  if (Array.isArray(value)) {
    for (let i = 0; i < value.length; i++) value[i] = __guguRevive(value[i]);
    return value;
  }
  if (value && typeof value === "object") {
    const Typed = typeof value.__lbxTyped === "string" && __guguTypedArrays[value.__lbxTyped];
    if (Typed) {
      const bytes = __guguBytes(value.b64);
      return Typed === Uint8Array ? bytes : new Typed(bytes.buffer, 0, bytes.byteLength / Typed.BYTES_PER_ELEMENT);
    }
    for (const key of Object.keys(value)) value[key] = __guguRevive(value[key]);
  }
  return value;
};
const electron = {
  ipcRenderer: {
    invoke: (channel, ...args) =>
      __guguInvoke("ipc_invoke", { channel, args }).then(__guguRevive, __guguWrapError(channel)),
    on: (channel, handler) => {
      __guguSubscribe();
      let handlers = __guguListeners.get(channel);
      if (!handlers) __guguListeners.set(channel, (handlers = new Set()));
      handlers.add(handler);
    },
    removeListener: (channel, handler) => { __guguListeners.get(channel)?.delete(handler); }
  },
  contextBridge: { exposeInMainWorld: (key, value) => { window[key] = value; } }
};
const preload = {
  electronAPI: {
    ipcRenderer: electron.ipcRenderer,
    process: { platform: "win32", versions: {}, env: {} }
  }
};
const process = { contextIsolated: true };
`;

// Frameless window: Electron honoured "-webkit-app-region: drag"; Tauri needs an
// explicit startDragging() call, so the same CSS classes drive it here.
const dragRegions = String.raw`
document.addEventListener("mousedown", (event) => {
  if (event.button !== 0) return;
  for (let el = event.target; el && el !== document.documentElement; el = el.parentElement) {
    if (el.matches && el.matches("button, a, input, textarea, select, [role=button]")) return;
    const cls = typeof el.className === "string" ? el.className : "";
    if (cls.includes("app-region:no-drag")) return;
    if (cls.includes("app-region:drag")) {
      event.preventDefault();
      if (event.detail === 2) {
        window.api.window.toggleMaximize();
      } else {
        window.__TAURI__?.window.getCurrentWindow().startDragging();
      }
      return;
    }
  }
}, true);
`;

const bridge = `// GENERATED by src-tauri/scripts/prepare-frontend.mjs — do not edit.
(() => {
  if (window.__GUGU_BRIDGE__) return;
  window.__GUGU_BRIDGE__ = true;
${transport}
${preload}
${dragRegions}
})();
`;
fs.mkdirSync(generatedDir, { recursive: true });
fs.writeFileSync(path.join(generatedDir, "bridge.js"), bridge);

// ---------------------------------------------------------------- 4. coverage
const schemaStart = preload.indexOf("const IPC_HANDLER_SCHEMA");
const schemaEnd = preload.indexOf("const HANDLER_METHODS");
const schemaSrc = preload.slice(schemaStart, schemaEnd).replace("const IPC_HANDLER_SCHEMA =", "return");
const schema = new Function("handler", schemaSrc)(() => null);
const channels = Object.entries(schema).flatMap(([ns, methods]) =>
  Object.keys(methods).map((m) => `${ns}:${m}`)
);

const rustSources = [];
const walk = (dir) => {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) walk(full);
    else if (entry.name.endsWith(".rs")) rustSources.push(fs.readFileSync(full, "utf8"));
  }
};
walk(path.join(tauriDir, "src"));
const rust = rustSources.join("\n");
const served = channels.filter((c) => rust.includes(`"${c}"`));
const byNs = new Map();
for (const c of channels) {
  const ns = c.split(":")[0];
  if (!byNs.has(ns)) byNs.set(ns, { done: [], todo: [] });
  byNs.get(ns)[served.includes(c) ? "done" : "todo"].push(c.split(":")[1]);
}
let md = `# Rust port — IPC coverage\n\nGenerated by \`src-tauri/scripts/prepare-frontend.mjs\`.\n\n`;
md += `**${served.length} / ${channels.length}** channels served by Rust.\n\n| Namespace | Done | Pending |\n|---|---|---|\n`;
for (const [ns, { done, todo }] of byNs) {
  md += `| \`${ns}\` | ${done.length ? done.join(", ") : "—"} | ${todo.length ? todo.join(", ") : "—"} |\n`;
}
fs.mkdirSync(path.join(repo, "docs"), { recursive: true });
fs.writeFileSync(path.join(repo, "docs", "rust-port-coverage.md"), md);

console.log(`[prepare-frontend] UI copied, bridge generated, ${served.length}/${channels.length} channels in Rust.`);
