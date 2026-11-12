// @ts-check
import { DeveloperView } from './developer.js';
import { UpdateBanner, UpdatePreferences } from './updater.js';
import { guardRepeatedActivation } from './clicks.js';
// Apparition's page: the install wizard and the Companion view. Every action is a Rust command
// (see src/flow.rs and src/companion.rs); this page only shows steps and collects answers. Passwords are read from their field, sent with the
// command and cleared from the field at once; nothing here stores them.

guardRepeatedActivation(window);

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const STEPS = ["welcome", "headset", "pair", "signing", "install", "done"];

const state = {
  step: "welcome",
  /** @type {any} */ info: {},
  /** @type {any[]} */ paired: [],
  /** @type {any[]} */ found: [],
  /** iPhones and iPads usbmuxd sees. @type {any[]} */ usb: [],
  /** @type {any} */ selected: null,
  /** The headset installed to: {udid, name, address, osVersion?}. @type {any} */ device: null,
  pairSkipped: false,
  /** What Apparition is used for: "install" or "develop" (unset until chosen). @type {"install"|"develop"|undefined} */ useMode: undefined,
  mode: "appleId",
  /** @type {any} */ signing: null,
  /** @type {string|null} */ p12: null,
  /** @type {string[]} */ profiles: [],
  what: "latest",
  /** @type {string|null} */ localPath: null,
  /** What the backend has prepared to install: "latest" or a local path. @type {string|null} */ prepared: null,
  /** @type {any} */ release: null,
  /** @type {any} */ result: null,
  handOff: true,
  /** @type {any} */ settings: {},
  /** This Mac gives its own anisette (never off a Mac). */ builtinAnisette: false,
  /** Apparition can make its own Android anisette here (core's `ANDROID_LIBRARIES` is on; macOS and Linux). */ androidAnisette: false,
  /** This Mac's anisette VM is set up (Companion tab). */ vmAnisette: false,
  /** The anisette VM's state (vm_status), while the Companion tab shows it. @type {any} */ vm: null,
  /** The Mac this computer is paired with for anisette: {macName, address, paired}, or null. @type {any} */ pairedMac: null,
  /** What the sign-in form has chosen: "builtin", "paired" or a server's address; null until it is open. @type {string|null} */ anisetteChoice: null,
  /** The latest companion status, for the header's badge. @type {any} */ companion: null,
  busy: false,
};

const panel = /** @type {HTMLElement} */ (document.getElementById("panel"));
const updateDialog = /** @type {HTMLDialogElement} */ (document.getElementById("update-consent"));
const updatePreferences = new UpdatePreferences(document, updateDialog, invoke);

// ------------------------------------------------------------ helpers

/** @param {ParentNode} root @param {string} role @returns {any} */
const $ = (root, role) => root.querySelector(`[data-role="${role}"]`);

/** @param {string} id @returns {DocumentFragment} */
const template = (id) => /** @type {HTMLTemplateElement} */ (document.getElementById(id)).content.cloneNode(true);

/** The message of a failed command: a Failure ({message, slot, sharedCertificate, cancelled}) or a string. */
function messageOf(/** @type {any} */ error) {
  if (typeof error === "string") return error;
  return error?.message ?? String(error);
}

/** @param {HTMLElement} el @param {string} text @param {"error"|"good"|""} [kind] */
function setStatus(el, text, kind = "") {
  el.hidden = !text;
  el.textContent = text;
  el.className = `status ${kind}`.trim();
  // A step can be taller than the window: bring a new error into view. The Companion tab redraws its
  // statuses every two seconds, so an error that is still the same one must not scroll the page again.
  const shown = kind === "error" && text ? text : "";
  if (shown && el.dataset.scrolledTo !== shown) el.scrollIntoView({ block: "nearest" });
  if (shown) el.dataset.scrolledTo = shown;
  else delete el.dataset.scrolledTo;
}

/** @param {HTMLButtonElement} button @param {boolean} busy */
function setBusy(button, busy) {
  button.disabled = busy;
  button.setAttribute("aria-busy", String(busy));
}

const basename = (/** @type {string} */ path) => path.split(/[\\/]/).pop() ?? path;
const day = (/** @type {string|null|undefined} */ iso) => (iso ? new Date(iso).toLocaleDateString(undefined, { dateStyle: "medium" }) : "unknown");
const megabytes = (/** @type {number} */ bytes) =>
  bytes < 1e6 ? `${Math.max(1, Math.round(bytes / 1e3))} KB` : `${(bytes / 1e6).toFixed(bytes > 1e8 ? 0 : 1)} MB`;

/**
 * Waits for Apparition on a nearby device or computer to send this computer its certificate key
 * (Share Signing there); the code comparison comes up as a prompt. `root` holds the
 * nearby-receive button, nearby-stop, nearby-hint and nearby-status. Resolves with the import
 * ({appleId, from, replaced}), or null when stopped or failed.
 * @param {ParentNode} root
 */
async function receiveNearby(root) {
  const button = /** @type {HTMLButtonElement} */ ($(root, "nearby-receive"));
  const stop = $(root, "nearby-stop");
  const hint = $(root, "nearby-hint");
  const status = $(root, "nearby-status");
  setBusy(button, true);
  stop.hidden = false;
  setStatus(status, "");
  const unlisten = await listen("nearby-listening", (/** @type {any} */ e) => {
    const { name, address, port, advertised } = e.payload;
    let text = `Waiting as “${name}”. In Apparition on the device that already signs with this Apple ID, choose Settings > Share Signing with Another Device, then “${name}” under Nearby. Both screens then show a six-digit code.`;
    if (address) text += advertised ? ` Not listed there? Type ${address}:${port}.` : ` This computer couldn't advertise itself, so type ${address}:${port} there.`;
    hint.hidden = false;
    hint.textContent = text;
  });
  const onStop = () => invoke("nearby_stop");
  stop.addEventListener("click", onStop);
  try {
    const imported = await invoke("nearby_receive");
    if (imported) setStatus(status, `Done: signing as ${imported.appleId} here uses the same certificate as ${imported.from}.`, "good");
    return imported ?? null;
  } catch (error) {
    setStatus(status, messageOf(error), "error");
    return null;
  } finally {
    unlisten();
    stop.removeEventListener("click", onStop);
    setBusy(button, false);
    stop.hidden = true;
    hint.hidden = true;
  }
}

/**
 * The third-party anisette server's address a sign-in choice names, or "" when it names none ("builtin",
 * "vm", "paired", nothing). Docker's old default isn't carried over: Apparition no longer provides a server.
 */
function serverAddressOf(/** @type {string|null|undefined} */ choice) {
  const text = (choice ?? "").trim();
  return !text || text === "builtin" || text === "vm" || text === "android" || text === "paired" || /^https?:\/\/localhost:6969\/?$/i.test(text) ? "" : text;
}

/** The host of a server's address, for a badge. */
function serverHost(/** @type {string} */ address) {
  try {
    return new URL(address).host || address;
  } catch {
    return address;
  }
}

/** What the anisette choice `choice` is, in a word or three. */
function anisetteName(/** @type {string} */ choice) {
  if (choice === "builtin") return "this Mac's own";
  if (choice === "android") return "Apparition's own (Android libraries)";
  if (choice === "vm") return state.vmAnisette ? "this Mac's anisette VM" : "anisette VM (not set up)";
  if (choice === "paired") return state.pairedMac ? `paired Mac (${state.pairedMac.macName})` : "paired Mac (not paired)";
  return choice.trim() ? `third-party server (${serverHost(choice.trim())})` : "none";
}

/** A choice in a word, for the header badge. */
function anisetteLabel(/** @type {string} */ choice) {
  if (choice === "builtin") return "Mac";
  if (choice === "android") return "Android";
  if (choice === "vm") return "VM";
  if (choice === "paired") return "Paired Mac";
  return choice?.trim() ? serverHost(choice.trim()) : "none";
}

/** What the companion offers (`companion_status`'s `anisette.offered`), in a word or three. */
function offeredName(/** @type {string} */ offered, /** @type {string} */ server = "") {
  return offered === "vm" ? "this Mac's anisette VM"
    : offered === "android" ? "Apparition's own (Android libraries)"
    : offered === "server" ? (server ? `third-party server (${serverHost(server)})` : "a third-party server")
    : "this Mac's own";
}

/**
 * Apparition uses one anisette source for this computer's sign-ins and for what the companion offers devices, so
 * the two never disagree. What the companion offers, as a sign-in choice ("builtin", "vm", "android" or a server's
 * address), or null when it offers nothing.
 */
function choiceOfOffer(/** @type {any} */ anisette) {
  const mode = anisette?.mode;
  if (mode === "server") return anisette.server || null;
  return mode === "builtin" || mode === "vm" || mode === "android" ? mode : null;
}

/** The companion mode (and server address) a sign-in choice stands for; null for the paired Mac's, which can't be offered on. */
function offerOfChoice(/** @type {string} */ choice) {
  const text = (choice ?? "").trim();
  if (!text || text === "paired") return null;
  if (text === "builtin" || text === "vm" || text === "android") return { mode: text };
  return serverAddressOf(text) ? { mode: "server", server: text } : null;
}

let reconciling = false;
/** What the companion offers is what sign-ins use: follows a change made on the Companion tab (the paired Mac's choice stays). */
async function followOffer(/** @type {any} */ status) {
  const want = choiceOfOffer(status?.anisette);
  const saved = state.settings?.anisette ?? null;
  if (!want || reconciling || saved === want || saved === "paired") return;
  reconciling = true;
  try {
    await invoke("save_anisette_choice", { choice: want });
    state.settings = { ...state.settings, anisette: want };
    drawSetupBadge();
  } catch {} finally {
    reconciling = false;
  }
}

/** A sign-in with `choice` is what the companion offers from now on, if it offers anything. */
async function offerChoice(/** @type {string} */ choice) {
  const offer = offerOfChoice(choice);
  const now = state.companion?.anisette;
  if (!offer || !now || now.mode === "off" || choiceOfOffer(now) === choice.trim()) return;
  try {
    await invoke("companion_set_anisette", offer);
  } catch {}
  refreshCompanion();
}

/** The anisette a sign-in uses when nothing was chosen: this Mac's own, else its anisette VM, else the paired Mac's. */
function defaultAnisette() {
  return state.builtinAnisette ? "builtin" : state.vmAnisette ? "vm" : state.pairedMac ? "paired" : state.androidAnisette ? "android" : "";
}

/**
 * The badge in the header's corner: where this computer's sign-in gets anisette, and what the
 * companion offers devices. On every screen, so a screenshot shows how Apparition is set up.
 */
async function drawSetupBadge() {
  const badge = /** @type {HTMLElement} */ (document.getElementById("setup-badge"));
  const text = /** @type {HTMLElement} */ (document.getElementById("setup-badge-text"));
  const choice = state.anisetteChoice ?? state.settings?.anisette ?? defaultAnisette();
  const anisette = state.companion?.anisette;
  const offered = state.companion?.relay?.running ? anisette?.offered : null;
  // Short on the badge (a screenshot aid, and the header must not grow), the sentence in its tooltip.
  const same = !!offered && choiceOfOffer({ mode: offered, server: anisette?.server }) === choice.trim();
  const mac = choice === "builtin" || (choice === "vm" && state.vmAnisette) || (choice === "paired" && !!state.pairedMac);
  text.textContent = `Anisette · ${offered && !same ? `${anisetteLabel(choice)} ≠ ${anisetteLabel(offered === "server" ? anisette?.server : offered)}` : anisetteLabel(choice)}`;
  badge.title = same ? `Sign-ins and your devices use ${anisetteName(choice)}`
    : offered ? `Sign-ins use ${anisetteName(choice)}; the companion offers ${offeredName(offered, anisette?.server)} to your devices`
    : `Sign-ins use ${anisetteName(choice)}; not offered to your devices`;
  badge.className = `setup-badge ${offered && !same ? "bad" : mac ? "ok" : "warn"}${offered ? "" : " solo"}`;
  badge.hidden = false;
}

/** Forgets what the step being left kept for a retry (the signing step sets it). */
let leaveStep = () => {};

function go(/** @type {string} */ step) {
  // A wait for another device's key belongs to the step that started it.
  invoke("nearby_stop").catch(() => {});
  leaveStep();
  leaveStep = () => {};
  state.step = step;
  render();
  panel.scrollTop = 0;
  panel.focus({ preventScroll: true });
}

// ------------------------------------------------------------ trail

function renderTrail() {
  const current = STEPS.indexOf(state.step);
  document.querySelectorAll("#trail li").forEach((li) => {
    const item = /** @type {HTMLElement} */ (li);
    const index = STEPS.indexOf(item.dataset.step ?? "");
    let mark = index < current ? "done" : index === current ? "current" : "todo";
    const note = /** @type {HTMLElement} */ (item.querySelector(".note"));
    if (item.dataset.step === "pair") {
      note.textContent = state.pairSkipped && index < current ? "Already paired" : "";
      if (state.pairSkipped && index < current) mark = "skipped";
    }
    if (item.dataset.step === "signing") {
      note.textContent = state.signing && index <= current ? signingLabel() : "";
    }
    item.dataset.state = mark;
    if (mark === "current") item.setAttribute("aria-current", "step");
    else item.removeAttribute("aria-current");
  });
}

function signingLabel() {
  if (!state.signing) return "";
  return state.signing.mode === "appleId" ? state.signing.email : state.signing.identity?.name ?? "Your certificate";
}

const actionsBar = /** @type {HTMLElement} */ (document.getElementById("actions"));

function render() {
  renderTrail();
  panel.replaceChildren();
  actionsBar.replaceChildren();
  ({ welcome, headset, pair, signing, install, done })[state.step]();
  // The step's buttons live below the scrolling panel (their listeners move with them).
  const actions = panel.querySelector(".actions");
  if (actions) actionsBar.append(actions);
}

// ------------------------------------------------------------ 1. welcome

function welcome() {
  const view = template("t-welcome");
  panel.append(view);
  // Developing needs a device set up too, so the first screen says what this one is for.
  if (state.useMode === "develop") $(panel, "welcome-title").textContent = "Set up a device for development.";
  else if (state.useMode === "install") $(panel, "welcome-title").textContent = "Install apps on your Apple device.";
  let starting = false;
  panel.querySelector('[data-go="headset"]')?.addEventListener("click", async () => {
    // A second press while the update prompt is open must not advance twice.
    if (starting) return;
    starting = true;
    try {
      await updatePreferences.beforeStart();
      go("headset");
    } finally {
      starting = false;
    }
  });
}

// ------------------------------------------------------------ 2. find the headset

/** A paired device seen on the network as a paired RemotePairing service. */
function online(/** @type {any} */ device) {
  return state.found.some((f) => !f.readyToPair && (f.addresses.includes(device.address) || f.name === device.name));
}

function headset() {
  panel.append(template("t-headset"));
  const list = $(panel, "list");
  const continueButton = $(panel, "continue");
  const searching = $(panel, "searching");
  const searchButton = $(panel, "search");

  const choose = (/** @type {any} */ choice) => {
    state.selected = choice;
    continueButton.disabled = !choice;
  };

  const draw = () => {
    list.replaceChildren();
    const rows = [
      ...state.paired.map((device) => ({ kind: "paired", device, key: `paired:${device.udid}` })),
      ...state.found
        .filter((f) => f.readyToPair)
        .map((f) => {
          const address = f.addresses.find((/** @type {string} */ a) => !a.includes(":")) ?? f.addresses[0];
          return { kind: "pair", name: f.name, address, port: f.port, key: `pair:${f.name}:${f.port}` };
        }),
    ];
    const usbRows = state.usb
      .filter((d) => !d.paired)
      .map((d) => ({ kind: "usb", name: "iPhone or iPad", udid: d.udid, usb: d.usb, key: `usb:${d.udid}` }));
    const usbList = $(panel, "usb-list");
    usbList.replaceChildren();
    for (const row of [...rows, ...usbRows]) {
      const item = template("t-device");
      const li = /** @type {HTMLElement} */ (item.querySelector("li"));
      const input = /** @type {HTMLInputElement} */ (item.querySelector("input"));
      const badge = /** @type {HTMLElement} */ (item.querySelector(".badge"));
      const title = /** @type {HTMLElement} */ (item.querySelector(".choice-title"));
      const meta = /** @type {HTMLElement} */ (item.querySelector(".choice-meta"));
      input.value = row.key;
      input.checked = state.selected?.key === row.key;
      if (row.kind === "paired") {
        title.textContent = row.device.name;
        meta.textContent = `Paired with this computer, at ${row.device.address}`;
        const seen = online(row.device);
        badge.textContent = seen ? "On this network" : "Not seen yet";
        badge.classList.toggle("online", seen);
        const forget = /** @type {HTMLButtonElement} */ (item.querySelector(".forget"));
        forget.hidden = false;
        forget.addEventListener("click", () => confirmForget(li, row.device, refreshPaired));
      } else if (row.kind === "usb") {
        title.textContent = row.name;
        meta.textContent = `${row.usb ? "On a cable" : "Trusted, on the network"}, ${row.udid}`;
        badge.textContent = "Ready to pair";
        badge.classList.add("ready");
      } else {
        title.textContent = row.name;
        meta.textContent = `${row.address}, pairing port ${row.port}`;
        badge.textContent = "Ready to pair";
        badge.classList.add("ready");
      }
      input.addEventListener("change", () => choose(row));
      (row.kind === "usb" ? usbList : list).append(item);
    }
    rows.push(...usbRows);
    $(panel, "empty").hidden = rows.length > 0 || !searching.hidden;
    const hint = $(panel, "mdns-hint");
    const nothing = state.found.length === 0 && searching.hidden;
    hint.hidden = !(nothing && state.info.platform === "macos-universal");
    hint.textContent =
      "If macOS asked whether Apparition may find devices on your local network, allow it. You can change that in System Settings > Privacy & Security > Local Network.";
    if (state.selected && !rows.some((r) => r.key === state.selected.key)) choose(null);
    else continueButton.disabled = !state.selected;
  };

  const refreshPaired = async () => {
    state.paired = await invoke("paired_devices");
    draw();
  };

  const search = async () => {
    searching.hidden = false;
    searchButton.disabled = true;
    draw();
    try {
      state.found = await invoke("discover");
    } catch (error) {
      state.found = [];
      const hint = $(panel, "mdns-hint");
      hint.hidden = false;
      hint.textContent = `Couldn't look on this network: ${messageOf(error)}`;
    }
    if (state.step !== "headset") return;
    searching.hidden = true;
    searchButton.disabled = false;
    draw();
  };

  const usbButton = $(panel, "usb-search");
  const searchUsb = async () => {
    usbButton.disabled = true;
    const note = $(panel, "usb-note");
    try {
      state.usb = await invoke("usb_devices");
      note.hidden = state.usb.some((d) => !d.paired);
      note.textContent = state.usb.length
        ? "Every connected iPhone or iPad is already paired: it's listed above."
        : "None connected. Plug one in and unlock it, then look again.";
    } catch (error) {
      state.usb = [];
      note.hidden = false;
      note.textContent = messageOf(error);
    }
    if (state.step !== "headset") return;
    usbButton.disabled = false;
    draw();
  };
  usbButton.addEventListener("click", searchUsb);
  searchButton.addEventListener("click", search);
  $(panel, "manual").addEventListener("submit", (/** @type {SubmitEvent} */ event) => {
    event.preventDefault();
    const form = /** @type {HTMLFormElement} */ (event.target);
    const address = form.address.value.trim();
    const port = Number(form.port.value.trim());
    if (!address || !Number.isInteger(port) || port < 1 || port > 65535) {
      form.port.setCustomValidity("Enter the pairing port, a number");
      form.port.reportValidity();
      return;
    }
    state.selected = { kind: "pair", name: address, address, port, key: `manual:${address}:${port}` };
    go("pair");
  });
  $(panel, "manual").port.addEventListener("input", (/** @type {Event} */ e) => /** @type {HTMLInputElement} */ (e.target).setCustomValidity(""));

  panel.querySelector('[data-go="welcome"]')?.addEventListener("click", () => go("welcome"));
  continueButton.addEventListener("click", () => {
    const choice = state.selected;
    if (!choice) return;
    if (choice.kind === "paired") {
      state.device = choice.device;
      state.pairSkipped = true;
      go("signing");
    } else {
      go("pair");
    }
  });

  refreshPaired().then(() => Promise.all([search(), searchUsb()]));
}

/** Replaces a row's Forget link with an inline confirmation. */
function confirmForget(/** @type {HTMLElement} */ li, /** @type {any} */ device, /** @type {() => void} */ done) {
  const forget = /** @type {HTMLElement} */ (li.querySelector(".forget"));
  forget.hidden = true;
  const box = document.createElement("span");
  box.className = "confirm-inline";
  const text = document.createElement("span");
  text.textContent = "Forget it?";
  const yes = document.createElement("button");
  yes.type = "button";
  yes.className = "button quiet small";
  yes.textContent = "Forget";
  const no = document.createElement("button");
  no.type = "button";
  no.className = "link";
  no.textContent = "Keep";
  box.append(text, yes, no);
  li.append(box);
  no.addEventListener("click", () => {
    box.remove();
    forget.hidden = false;
  });
  yes.addEventListener("click", async () => {
    await invoke("forget_device", { udid: device.udid });
    if (state.selected?.device?.udid === device.udid) state.selected = null;
    if (state.device?.udid === device.udid) state.device = null;
    done();
    const hint = $(panel, "mdns-hint");
    hint.hidden = false;
    hint.textContent = `Forgot ${device.name}. On a Vision Pro or Apple TV, also remove this computer from its pairing screen in Settings.`;
  });
}

// ------------------------------------------------------------ 3. pair

function pair() {
  panel.append(template("t-pair"));
  const target = state.selected;
  $(panel, "name").textContent = target?.name ?? "the device";
  const start = /** @type {HTMLButtonElement} */ ($(panel, "start"));
  const back = /** @type {HTMLButtonElement} */ (panel.querySelector('[data-go="headset"]'));
  const status = $(panel, "status");
  back.addEventListener("click", () => go("headset"));

  if (state.device && !state.pairSkipped && (target?.kind === "usb" ? state.device.udid === target.udid : state.device.address === target?.address)) {
    setStatus(status, `Paired with ${state.device.name}.`, "good");
    start.textContent = "Continue";
    start.onclick = () => go("signing");
    return;
  }

  if (target?.kind === "usb") {
    $(panel, "name").textContent = "your iPhone or iPad";
    panel.querySelector(".lede").textContent =
      "Pairing lets this computer install apps on it. It happens once over the cable; after that it's reached over Wi-Fi.";
    const howto = /** @type {HTMLElement} */ (panel.querySelector(".howto"));
    howto.replaceChildren();
    for (const text of [
      "Keep it plugged into this computer, and unlocked.",
      "Keep it on the same Wi-Fi as this computer, then select Start pairing. No code is needed.",
      "If it asks whether to trust this computer, tap Trust and enter its passcode.",
    ]) {
      const li = document.createElement("li");
      li.textContent = text;
      howto.append(li);
    }
  }

  start.onclick = async () => {
    setBusy(start, true);
    back.disabled = true;
    const usb = target?.kind === "usb";
    setStatus(status, usb ? "Pairing over the cable, then looking for it on Wi-Fi…" : "Waiting for the device to show its code…");
    // Over the cable, the device may first ask whether to trust this computer.
    const stop = usb ? await listen("pair-status", (/** @type {any} */ e) => setStatus(status, `${e.payload} Pairing continues once it's trusted…`)) : null;
    try {
      const device = usb
        ? await invoke("pair_usb_device", { udid: target.udid })
        : await invoke("pair_device", { address: target.address, port: target.port });
      state.device = device;
      state.pairSkipped = false;
      const os = device.osVersion ? ` (${device.platform} ${device.osVersion})` : "";
      setStatus(status, `Paired with ${device.name}${os}. This computer stays paired for next time.`, "good");
      start.textContent = "Continue";
      start.onclick = () => go("signing");
    } catch (error) {
      setStatus(status, pairingError(error), error?.cancelled ? "" : "error");
      start.textContent = "Try again";
    } finally {
      stop?.();
      setBusy(start, false);
      back.disabled = false;
    }
  };
}

function pairingError(/** @type {any} */ error) {
  const message = messageOf(error);
  if (error?.cancelled) return message;
  if (/srp|proof/i.test(message)) return `The code didn't match. Check the code the device shows, and try again.\n\n${message}`;
  if (/trust|usbmuxd|lockdown|isn't connected/i.test(message)) return message;
  if (/refused|reset|unreachable|timed out|reach/i.test(message))
    return `Couldn't reach the device. Keep its pairing screen open (the pairing port changes when it reopens), and check both are on the same network.\n\n${message}`;
  return message;
}

// ------------------------------------------------------------ 4. signing

function signing() {
  panel.append(template("t-signing"));
  const appleForm = /** @type {HTMLFormElement} */ ($(panel, "apple-id"));
  const certForm = /** @type {HTMLFormElement} */ ($(panel, "certificate"));
  const keyForm = /** @type {HTMLFormElement} */ ($(panel, "key-import"));
  const signed = $(panel, "signed");
  const status = $(panel, "status");
  const recovery = $(panel, "recovery");
  const continueButton = /** @type {HTMLButtonElement} */ ($(panel, "continue"));
  const manageIds = /** @type {HTMLButtonElement} */ ($(panel, "manage-app-ids"));
  const idManager = $(panel, "app-id-manager");
  manageIds.addEventListener("click", () => showSlots(idManager, "appIds", () => {}, undefined, true));
  const modes = /** @type {NodeListOf<HTMLInputElement>} */ (panel.querySelectorAll('input[name="mode"]'));

  $(panel, "back").addEventListener("click", () => go(state.pairSkipped ? "headset" : "pair"));
  continueButton.addEventListener("click", () => go("install"));
  $(panel, "help").addEventListener("click", () => invoke("open_help"));

  const draw = () => {
    modes.forEach((m) => {
      m.checked = m.value === state.mode;
      m.disabled = !!state.signing || state.busy;
    });
    const done = !!state.signing;
    appleForm.hidden = done || state.mode !== "appleId";
    keyForm.hidden = appleForm.hidden;
    certForm.hidden = done || state.mode !== "certificate";
    signed.hidden = !done;
    // The status and its recovery buttons sit right under what is being used, not below the
    // key-file form, which can push them out of the window.
    (done ? signed : certForm.hidden ? appleForm : certForm).after(status, recovery);
    manageIds.hidden = state.signing?.mode !== "appleId";
    if (manageIds.hidden) idManager.hidden = true;
    continueButton.disabled = !done;
    if (done) drawSigned();
    renderTrail();
  };

  const drawSigned = () => {
    const title = $(panel, "signed-title");
    const list = $(panel, "signed-list");
    list.replaceChildren();
    const add = (/** @type {string} */ text, warn = false) => {
      const li = document.createElement("li");
      li.textContent = text;
      if (warn) li.className = "warn";
      list.append(li);
    };
    $(panel, "nearby-send").hidden = state.signing.mode !== "appleId";
    if (state.signing.mode === "appleId") {
      title.textContent = `Signed in as ${state.signing.email}`;
      const team = state.signing.team?.name ? `${state.signing.team.name} (${state.signing.teamId})` : state.signing.teamId;
      add(`Team ${team}`);
    } else {
      const identity = state.signing.identity;
      title.textContent = identity.name;
      add(`Expires ${day(identity.expires)}`);
      let usable = 0;
      for (const profile of state.signing.profiles) {
        const forDevice = platformMatches(profile.platforms);
        const fits = profile.includesDevice === true && forDevice && !profile.expired;
        if (fits) usable++;
        const why = profile.expired
          ? "has expired"
          : !forDevice
            ? `isn't for ${state.device?.platform ?? "visionOS"}`
            : profile.includesDevice === false
              ? `doesn't list ${state.device?.name ?? "this device"}`
              : `lists ${state.device?.name ?? "this device"}`;
        add(`${profile.name}: ${why}`, !fits);
      }
      if (!usable) add("None of these profiles can sign for this device. Add one that lists it.", true);
    }
  };

  modes.forEach((m) =>
    m.addEventListener("change", () => {
      state.mode = m.value;
      setStatus(status, "");
      draw();
    }),
  );

  // Apple ID
  appleForm.email.value = state.settings.email ?? "";
  const savedAnisette = state.settings.anisette;
  appleForm.anisette.value = serverAddressOf(savedAnisette);
  // A server set up on the Companion tab comes here too, so it isn't typed twice.
  if (!appleForm.anisette.value) appleForm.anisette.value = state.companion?.anisette?.server ?? "";
  const sourceSelect = /** @type {HTMLSelectElement} */ (appleForm.elements.namedItem("source"));
  const builtinOption = /** @type {HTMLOptionElement} */ ($(appleForm, "source-builtin"));
  // This Mac's own is offered only where macOS gives it.
  builtinOption.hidden = builtinOption.disabled = !state.builtinAnisette;
  // The anisette VM, once the Companion tab has set it up.
  const vmOption = /** @type {HTMLOptionElement} */ ($(appleForm, "source-vm"));
  vmOption.hidden = vmOption.disabled = !state.vmAnisette;
  // Apparition's own Android anisette, where the build has it switched on.
  const androidOption = /** @type {HTMLOptionElement} */ ($(appleForm, "source-android"));
  androidOption.hidden = androidOption.disabled = !state.androidAnisette;
  // Nothing saved comes back as null, so `!= null` (not `!== undefined`).
  const initial = serverAddressOf(savedAnisette) ? "server"
    : savedAnisette === "builtin" && state.builtinAnisette ? "builtin"
    : savedAnisette === "vm" && state.vmAnisette ? "vm"
    : savedAnisette === "android" && state.androidAnisette ? "android"
    : savedAnisette === "paired" ? "paired"
    // With no Mac of any kind set up, a server the Companion tab already has is the way to go on.
    : defaultAnisette() || (appleForm.anisette.value ? "server" : "paired");
  sourceSelect.value = initial === "server" ? "server" : initial;
  const macStatus = $(appleForm, "paired-mac-status");
  const macPairRow = $(appleForm, "paired-mac-pair");
  const macActions = $(appleForm, "paired-mac-actions");
  const macCode = $(appleForm, "mac-pairing-code");
  const macPairingStatus = $(appleForm, "mac-pairing-status");
  const drawPairedMac = (/** @type {boolean} */ pairingAnother = false) => {
    const paired = state.pairedMac;
    macStatus.hidden = !paired;
    if (paired) macStatus.textContent = `Paired with ${paired.macName} (${paired.address}).`;
    macPairRow.hidden = !!paired && !pairingAnother;
    macActions.hidden = !paired || pairingAnother;
    if (!appleForm.macAddress.value && paired) appleForm.macAddress.value = paired.address;
  };
  const drawSource = () => {
    const source = sourceSelect.value;
    $(appleForm, "anisette-builtin-hint").hidden = source !== "builtin";
    $(appleForm, "anisette-vm-hint").hidden = source !== "vm";
    $(appleForm, "anisette-android-hint").hidden = source !== "android";
    $(appleForm, "anisette-paired").hidden = source !== "paired";
    for (const role of ["anisette-server", "anisette-server-hint", "anisette-server-check-row", "anisette-server-status"]) $(appleForm, role).hidden = source !== "server";
    if (source !== "server") setStatus($(appleForm, "anisette-server-status"), "");
    appleForm.anisette.required = source === "server";
    drawPairedMac();
  };
  const chooseAnisette = () => {
    state.anisetteChoice = sourceSelect.value === "server" ? appleForm.anisette.value.trim() : sourceSelect.value;
    drawSetupBadge();
  };
  sourceSelect.addEventListener("change", () => {
    drawSource();
    chooseAnisette();
  });
  const serverStatus = $(appleForm, "anisette-server-status");
  appleForm.anisette.addEventListener("input", () => {
    setStatus(serverStatus, "");
    chooseAnisette();
  });
  $(appleForm, "anisette-server-check").addEventListener("click", (event) =>
    checkAnisetteServer(appleForm.anisette.value, /** @type {HTMLButtonElement} */ (event.currentTarget), serverStatus));
  // No Mac: the way on is a third-party server.
  $(appleForm, "use-server").addEventListener("click", () => {
    sourceSelect.value = "server";
    drawSource();
    chooseAnisette();
    appleForm.anisette.focus();
  });
  drawSource();
  chooseAnisette();
  // Pairing with a Mac: the code comes as an event while the Mac's person allows this computer.
  const pairButton = /** @type {HTMLButtonElement} */ ($(appleForm, "pair-mac"));
  pairButton.addEventListener("click", async () => {
    const address = appleForm.macAddress.value.trim();
    if (!address) {
      setStatus(macPairingStatus, "Type the Mac's address: its name, LAN or Tailscale address.", "error");
      return;
    }
    setBusy(pairButton, true);
    setStatus(macPairingStatus, `Asking ${address} to pair…`);
    const stop = await listen("mac-pairing-code", (/** @type {any} */ e) => {
      macCode.textContent = String(e.payload);
      macCode.hidden = false;
      setStatus(macPairingStatus, "On the Mac, allow this computer if it shows the same code.");
    });
    try {
      state.pairedMac = await invoke("pair_mac", { address, developer: appleForm.macDeveloper?.checked ?? false });
      setStatus(macPairingStatus, "Paired. Sign in above.", "good");
      drawPairedMac();
      chooseAnisette();
    } catch (error) {
      setStatus(macPairingStatus, messageOf(error), "error");
    } finally {
      stop();
      macCode.hidden = true;
      setBusy(pairButton, false);
    }
  });
  $(appleForm, "repair-mac").addEventListener("click", () => {
    appleForm.macAddress.value = "";
    drawPairedMac(true);
  });
  $(appleForm, "forget-mac").addEventListener("click", async () => {
    await invoke("forget_paired_mac");
    state.pairedMac = null;
    setStatus(macPairingStatus, "Forgot this Mac here. Unpair this computer in the Mac's Apparition too.", "");
    drawPairedMac();
    chooseAnisette();
  });
  // After a failure that trying again may cure (a connection), the sign-in's details are kept
  // here, in memory only and for a few minutes, so Try again needs no retyping. Cleared by the
  // next sign-in, by success, and when this step is left.
  /** @type {{email: string, password: string, anisette: string}|null} */ let retry = null;
  /** @type {number|undefined} */ let retryTimer;
  const clearRecovery = () => {
    if (retry) retry.password = "";
    retry = null;
    clearTimeout(retryTimer);
    recovery.hidden = true;
  };
  leaveStep = clearRecovery;
  const attempt = async (/** @type {{email: string, password: string, anisette: string}} */ args) => {
    const submit = /** @type {HTMLButtonElement} */ (appleForm.querySelector('[type="submit"]'));
    clearRecovery();
    setBusy(submit, true);
    state.busy = true;
    draw();
    setStatus(status, "Signing in to Apple…");
    try {
      const result = await invoke("apple_id_sign_in", args);
      state.signing = { mode: "appleId", ...result };
      state.settings = { email: result.email, anisette: args.anisette.trim() };
      offerChoice(args.anisette);
      setStatus(status, "");
      args.password = "";
      drawSetupBadge();
    } catch (error) {
      setStatus(status, signInError(error, args.anisette), error?.cancelled ? "" : "error");
      if (error?.kind === "network" || error?.kind === "pairedMac") {
        retry = { ...args };
        retryTimer = setTimeout(clearRecovery, 5 * 60 * 1000);
        recovery.hidden = false;
        // The error and what can be done about it, together, rather than the error alone.
        status.scrollIntoView({ block: "start" });
      }
      args.password = "";
    } finally {
      setBusy(submit, false);
      state.busy = false;
      draw();
    }
  };
  appleForm.addEventListener("submit", (event) => {
    event.preventDefault();
    const source = sourceSelect.value;
    if (source === "paired" && !state.pairedMac) {
      setStatus(status, "Pair this computer with your Mac first, below the password.", "error");
      return;
    }
    const args = { email: appleForm.email.value, password: appleForm.password.value, anisette: source === "server" ? appleForm.anisette.value : source };
    appleForm.password.value = "";
    attempt(args);
  });
  $(panel, "retry").addEventListener("click", () => {
    if (retry) attempt({ ...retry });
    else setStatus(status, "Type your password again, then sign in.", "error");
  });
  drawSetupBadge();

  // Another device's certificate key, before signing in: over the network, or as a file
  $(keyForm, "nearby-receive").addEventListener("click", async () => {
    const imported = await receiveNearby(keyForm);
    if (imported && !appleForm.email.value) appleForm.email.value = imported.appleId;
  });
  /** @type {string|null} */ let keyPath = null;
  /** @type {string|null} */ let keyFrom = null;
  const keyStatus = $(keyForm, "key-status");
  const drawKey = () => {
    $(keyForm, "key-name").textContent = keyPath ? basename(keyPath) : "No key file chosen";
    $(keyForm, "key-words-field").hidden = !keyPath || !keyFrom;
    $(keyForm, "key-words-label").textContent = keyFrom ? `The five words ${keyFrom} showed` : "The five words";
    $(keyForm, "key-submit-row").hidden = !keyPath;
  };
  drawKey();
  $(keyForm, "pick-key").addEventListener("click", async () => {
    const [path] = await invoke("pick_file", { kind: "key" });
    if (!path) return;
    try {
      keyFrom = await invoke("signing_key_from", { path });
      keyPath = path;
      setStatus(keyStatus, "");
    } catch (error) {
      setStatus(keyStatus, messageOf(error), "error");
    }
    drawKey();
    if (keyFrom) keyForm.words.focus();
  });
  keyForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (!keyPath) return;
    const submit = /** @type {HTMLButtonElement} */ (keyForm.querySelector('[type="submit"]'));
    setBusy(submit, true);
    setStatus(keyStatus, "Opening the key file…");
    try {
      const imported = await invoke("import_signing_key", { path: keyPath, words: keyFrom ? keyForm.words.value : null });
      keyForm.words.value = "";
      keyPath = null;
      keyFrom = null;
      let text = `Done: signing as ${imported.appleId} here uses the same certificate as ${imported.from}. Sign in above to continue.`;
      if (imported.replaced) text += " This computer's own certificate for it stays on the account until you revoke it.";
      setStatus(keyStatus, text, "good");
      if (!appleForm.email.value) appleForm.email.value = imported.appleId;
    } catch (error) {
      setStatus(keyStatus, messageOf(error), "error");
    } finally {
      setBusy(submit, false);
      drawKey();
    }
  });

  // Your own certificate
  const showFiles = () => {
    $(panel, "p12-name").textContent = state.p12 ? basename(state.p12) : "No certificate chosen";
    $(panel, "profile-names").textContent = state.profiles.length ? state.profiles.map(basename).join(", ") : "No profiles yet";
  };
  showFiles();
  $(panel, "pick-p12").addEventListener("click", async () => {
    const [path] = await invoke("pick_file", { kind: "p12" });
    if (path) state.p12 = path;
    showFiles();
  });
  $(panel, "pick-profiles").addEventListener("click", async () => {
    const paths = await invoke("pick_file", { kind: "profile" });
    for (const path of paths) if (!state.profiles.includes(path)) state.profiles.push(path);
    showFiles();
  });
  certForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    if (!state.p12) return setStatus(status, "Choose your .p12 first.", "error");
    if (!state.profiles.length) return setStatus(status, "Add at least one provisioning profile that lists the device.", "error");
    const submit = /** @type {HTMLButtonElement} */ (certForm.querySelector('[type="submit"]'));
    const args = { p12: state.p12, password: certForm.password.value, profiles: state.profiles, udid: state.device?.udid ?? "" };
    certForm.password.value = "";
    setBusy(submit, true);
    state.busy = true;
    setStatus(status, "Opening the certificate…");
    try {
      const result = await invoke("import_certificate", args);
      state.signing = { mode: "certificate", ...result };
      setStatus(status, "");
    } catch (error) {
      setStatus(status, messageOf(error), "error");
    } finally {
      args.password = "";
      setBusy(submit, false);
      state.busy = false;
      draw();
    }
  });

  $(panel, "change").addEventListener("click", async () => {
    if (state.signing?.mode === "appleId") await invoke("sign_out");
    state.signing = null;
    draw();
  });

  // This computer's certificate key, to a nearby Apparition waiting for it
  const nearbyList = $(panel, "nearby-list");
  const nearbyStatus = $(panel, "nearby-send-status");
  const send = async (/** @type {string} */ address, /** @type {string} */ name) => {
    setStatus(nearbyStatus, `Connecting to ${name}…`);
    try {
      const peer = await invoke("nearby_send", { address });
      setStatus(nearbyStatus, `${peer} now signs with this computer's certificate.`, "good");
    } catch (error) {
      setStatus(nearbyStatus, messageOf(error), error?.cancelled ? "" : "error");
    }
  };
  $(panel, "nearby-find").addEventListener("click", async () => {
    const find = /** @type {HTMLButtonElement} */ ($(panel, "nearby-find"));
    setBusy(find, true);
    setStatus(nearbyStatus, "Looking for Apparition waiting for a key…");
    try {
      const found = await invoke("nearby_browse");
      nearbyList.replaceChildren();
      for (const receiver of found) {
        const li = document.createElement("li");
        const button = document.createElement("button");
        button.type = "button";
        button.className = "button quiet";
        button.textContent = `Send to ${receiver.name}`;
        button.disabled = !receiver.address;
        button.addEventListener("click", () => send(receiver.address, receiver.name));
        li.append(button);
        nearbyList.append(li);
      }
      nearbyList.hidden = !found.length;
      $(panel, "nearby-address-row").hidden = false;
      setStatus(nearbyStatus, found.length ? "" : "None found. On the other device, open Get Signing from Another Device first, or type the address it shows.");
    } catch (error) {
      setStatus(nearbyStatus, messageOf(error), "error");
    } finally {
      setBusy(find, false);
    }
  });
  $(panel, "nearby-send-address").addEventListener("click", () => {
    const address = /** @type {HTMLInputElement} */ ($(panel, "nearby-address")).value.trim();
    if (address) send(address, address);
  });

  draw();
}

/** Whether a profile's Platform list covers the device's platform. */
function platformMatches(/** @type {string[]|undefined} */ platforms) {
  const names = { visionOS: ["xrOS", "visionOS"], iOS: ["iOS"], tvOS: ["tvOS"] }[state.device?.platform ?? "visionOS"] ?? [];
  return (platforms ?? []).some((p) => names.includes(p));
}

/**
 * Asks the third-party anisette server at `address` what it is (no Apple ID is used) and says so in `status`.
 * Whether Apple accepts what it serves is Apple's to say, and it changes that without notice: only a sign-in settles it.
 */
async function checkAnisetteServer(/** @type {string} */ address, /** @type {HTMLButtonElement} */ button, /** @type {HTMLElement} */ status) {
  if (!address.trim()) return setStatus(status, "Type the anisette server's address first.", "error");
  setBusy(button, true);
  setStatus(status, "Asking the server…");
  try {
    const found = await invoke("check_anisette_server", { address });
    const serves = /** @type {Record<string, string>} */ ({ mac: "a Mac's anisette", android: "Android's anisette", windows: "Windows' anisette" })[found.platform] ?? found.platform;
    const mac = found.platform === "mac";
    const note = mac ? "Apple accepts a Mac's for developer access." : "Apple may refuse it for developer access; signing in is the only way to know.";
    setStatus(status, `It answers, and serves ${serves}${found.model ? ` (${found.model})` : ""}. ${note}`, mac ? "good" : "");
  } catch (error) {
    setStatus(status, messageOf(error), "error");
  } finally {
    setBusy(button, false);
  }
}

function signInError(/** @type {any} */ error, /** @type {string} */ anisette = "") {
  const message = messageOf(error);
  if (error?.cancelled) return message;
  // core's REFUSED_ANISETTE and the paired Mac's failures already explain themselves.
  if (/-80045|-80046/.test(message) || error?.kind === "pairedMac") return message;
  const usingServer = anisette !== "builtin" && anisette !== "paired" && anisette !== "vm" && anisette !== "android";
  if (usingServer && (/anisette/i.test(message) || (/connect|dns|resolve|refused/i.test(message) && !/\.apple\.com/i.test(message))))
    return `Couldn't sign in. Check that the anisette server is running and its address is right.\n\n${message}`;
  return message;
}

// ------------------------------------------------------------ 5. install

/** Install stages, in order. */
const STAGES = ["release", "download", "connecting", "signing", "installing", "watch", "handoff"];

function install() {
  panel.append(template("t-install"));
  const start = /** @type {HTMLButtonElement} */ ($(panel, "start"));
  const back = /** @type {HTMLButtonElement} */ ($(panel, "back"));
  const status = $(panel, "status");
  const stages = $(panel, "stages");
  const slots = $(panel, "slots");
  const manageIds = /** @type {HTMLButtonElement} */ ($(panel, "manage-app-ids"));
  manageIds.hidden = state.signing?.mode !== "appleId";
  manageIds.addEventListener("click", () => showSlots(slots, "appIds", () => {}, undefined, true));
  const advanced = $(panel, "advanced");
  const what = /** @type {NodeListOf<HTMLInputElement>} */ (panel.querySelectorAll('input[name="what"]'));
  const handoff = /** @type {HTMLInputElement} */ ($(panel, "handoff"));

  back.addEventListener("click", () => go("signing"));
  handoff.checked = state.handOff;
  handoff.addEventListener("change", () => (state.handOff = handoff.checked));

  const describe = () => {
    what.forEach((w) => (w.checked = w.value === state.what));
    const local = state.what === "local" && state.localPath;
    panel.querySelector("h1").textContent = local ? `Install ${basename(state.localPath ?? "")}` : "Install Apparition";
    $(panel, "lede").textContent = local
      ? `Signs this app for ${state.device?.name ?? "the device"} and installs it.`
      : `The latest official release, checked against Apparition's release signature before it's used, signed for ${state.device?.name ?? "the device"}.`;
    $(panel, "local-name").textContent = state.localPath ? basename(state.localPath) : "An .ipa, or a .deb such as Kodi's, on this computer.";
    if (state.what === "local") advanced.open = true;
  };
  what.forEach((w) =>
    w.addEventListener("change", async () => {
      if (w.value === "local") {
        const [path] = await invoke("pick_file", { kind: "ipa" });
        if (path) state.localPath = path;
        state.what = state.localPath ? "local" : "latest";
      } else {
        state.what = "latest";
      }
      describe();
    }),
  );
  describe();

  /** @param {string} name @param {"active"|"done"|"failed"|"skipped"|""} value @param {string} [detail] */
  const mark = (name, value, detail) => {
    const li = stages.querySelector(`[data-stage="${name}"]`);
    if (!li) return;
    li.dataset.state = value;
    if (detail !== undefined) li.querySelector(".stage-detail").textContent = detail;
  };
  /** Marks `name` active and every earlier stage that was active as done. */
  const reach = (/** @type {string} */ name, /** @type {string} */ detail = "") => {
    for (const earlier of STAGES.slice(0, STAGES.indexOf(name))) {
      const li = stages.querySelector(`[data-stage="${earlier}"]`);
      if (li.dataset.state === "active") li.dataset.state = "done";
    }
    mark(name, "active", detail);
  };

  const unlisten = [];
  const cleanup = () => unlisten.splice(0).forEach((stop) => stop());

  const run = async () => {
    setBusy(start, true);
    manageIds.disabled = true;
    back.disabled = true;
    advanced.hidden = true;
    slots.hidden = true;
    stages.hidden = false;
    setStatus(status, "");
    const key = state.what === "local" ? state.localPath : "latest";
    STAGES.forEach((s) => mark(s, ""));
    const meter = stages.querySelector(".meter");
    const fill = stages.querySelector(".meter-fill");

    unlisten.push(
      await listen("release", (/** @type {any} */ e) => {
        state.release = e.payload;
        mark("release", "done", `Version ${e.payload.version}, signed by ${e.payload.signer}`);
        reach("download", e.payload.size ? megabytes(e.payload.size) : "");
        meter.hidden = false;
      }),
      await listen("download", (/** @type {any} */ e) => {
        const { received, total } = e.payload;
        fill.style.width = `${Math.min(100, (received / Math.max(total, 1)) * 100)}%`;
        mark("download", "active", `${megabytes(received)} of ${megabytes(total)}`);
      }),
      await listen("stage", (/** @type {any} */ e) => {
        const { stage, detail } = e.payload;
        const text =
          stage === "connecting" ? detail : stage === "signing" ? signingLabel() : stage === "installing" ? (detail.update ? "Updating" : "")
          : stage === "watch" ? (detail.trust ? `Tap Trust on ${detail.name}` : detail.name) : "";
        reach(stage, text);
      }),
    );

    try {
      if (state.prepared !== key) {
        state.prepared = null;
        if (key === "latest") {
          reach("release", "apparition.illixion.com");
          await invoke("prepare_latest", { udid: state.device.udid });
          meter.hidden = true;
          mark("download", "done", `${megabytes(state.release?.size ?? 0)}, hash checked`);
        } else {
          const local = await invoke("prepare_local", { path: key });
          mark("release", "skipped", "Your own file");
          mark("download", "skipped", local.converted ? `${local.converted} from ${local.name}, ${megabytes(local.size)}` : `${local.name}, ${megabytes(local.size)}`);
        }
        state.prepared = key;
      } else if (key === "latest") {
        mark("release", "done", state.release ? `Version ${state.release.version}, signed by ${state.release.signer}` : "");
        mark("download", "done", "Already downloaded");
      } else {
        mark("release", "skipped", "Your own file");
        mark("download", "skipped", basename(key ?? ""));
      }

      const result = await invoke("install", {
        options: { udid: state.device.udid, handOff: state.handOff },
      });
      for (const s of ["connecting", "signing", "installing"]) mark(s, "done");
      const watches = result.watches ?? [];
      mark("watch", !watches.length ? "skipped" : watches.every((w) => !w.error) ? "done" : "failed",
        watches.map((w) => (w.error ? `${w.name}: ${w.error}` : w.name)).join(", "));
      mark("handoff", result.handoff ? "done" : result.handoffError ? "failed" : "skipped");
      state.result = { ...result, label: key === "latest" ? "Apparition" : basename(key ?? "") };
      cleanup();
      go("done");
      return;
    } catch (error) {
      const active = stages.querySelector('[data-state="active"]');
      if (active) active.dataset.state = "failed";
      meter.hidden = true;
      setStatus(status, installError(error), error?.cancelled ? "" : "error");
      if (error?.sharedCertificate && state.signing?.mode === "appleId") showShared(slots, run);
      else if (error?.slot && state.signing?.mode === "appleId") showSlots(slots, error.slot, run);
      start.textContent = "Try again";
      advanced.hidden = false;
    } finally {
      cleanup();
      setBusy(start, false);
      manageIds.disabled = false;
      back.disabled = false;
    }
  };
  start.addEventListener("click", run);
}

function installError(/** @type {any} */ error) {
  const message = messageOf(error);
  if (error?.sharedCertificate) return "Signing stopped before making a second certificate: Apparition on another device already has one for this Apple ID.";
  if (error?.slot === "certificates") return `The Apple ID has no room for another development certificate.\n\n${message}`;
  if (error?.slot === "appIds") return `The Apple ID can't make another App ID right now.\n\n${message}`;
  if (/reach|refused|reset|timed out|unreachable/i.test(message))
    return `Couldn't reach ${state.device?.name ?? "the device"}. Check that it's awake and on the same network as this computer.\n\n${message}`;
  return message;
}

/**
 * Signing stopped rather than make a second certificate: receive the other Apparition's key, or
 * make one here anyway, then `retry`.
 * @param {HTMLElement} box @param {() => void} retry
 */
function showShared(box, retry) {
  box.replaceChildren(template("t-shared"));
  box.hidden = false;
  $(box, "nearby-receive").addEventListener("click", async () => {
    if (await receiveNearby(box)) {
      box.hidden = true;
      retry();
    }
  });
  $(box, "new").addEventListener("click", async () => {
    try {
      await invoke("allow_new_certificate");
      box.hidden = true;
      retry();
    } catch (error) {
      setStatus($(box, "nearby-status"), messageOf(error), "error");
    }
  });
  box.scrollIntoView({ block: "nearest", behavior: "smooth" });
}

/**
 * Lists what fills the slot and frees one the user picks, only after they type the word.
 * @param {HTMLElement} box @param {"certificates"|"appIds"} slot @param {() => void} retry
 * @param {any} [sample] a listing to show instead of asking the account (debug previews)
 * @param {boolean} [manager] Open directly, without an install failure or automatic retry.
 */
async function showSlots(box, slot, retry, sample, manager = false) {
  const certificates = slot === "certificates";
  const word = certificates ? "revoke" : "delete";
  box.replaceChildren(template("t-slots"));
  box.hidden = false;
  box.classList.toggle("account-manager", manager);
  $(box, "title").textContent = certificates ? "Free a certificate slot" : "App IDs";
  $(box, "close").addEventListener("click", () => { box.hidden = true; });
  $(box, "refresh").addEventListener("click", () => showSlots(box, slot, retry, sample, manager));
  $(box, "word").textContent = word;
  if (certificates) {
    $(box, "share").hidden = false;
    $(box, "nearby-receive").addEventListener("click", async () => {
      if (await receiveNearby(box)) {
        box.hidden = true;
        retry();
      }
    });
  }
  const button = /** @type {HTMLButtonElement} */ ($(box, "do"));
  button.textContent = certificates ? "Revoke certificate" : "Delete App ID";
  const typed = /** @type {HTMLInputElement} */ ($(box, "typed"));
  const confirm = $(box, "confirm");
  const list = $(box, "list");
  const explain = $(box, "explain");
  const filter = /** @type {HTMLInputElement} */ ($(box, "filter"));
  $(box, "filter-field").hidden = certificates;
  box.scrollIntoView({ block: "nearest", behavior: "smooth" });
  explain.textContent = "Looking at the account…";
  /** @type {any} */ let chosen = null;

  let items = [];
  try {
    if (certificates) {
      items = (sample ?? (await invoke("list_certificates"))).certificates ?? [];
      explain.textContent =
        items.length < 2
          ? "Apple says both of this Apple ID's certificate slots are taken, but lists fewer. After a revoke, Apple can take a while to free the slot; try again later, or use the certificate you already have by choosing Share Signing in Apparition on your other device."
          : "A free Apple ID has room for two development certificates. Revoke one you no longer need, then Apparition installs again. Nothing is revoked until you confirm.";
    } else {
      const ids = sample ?? (await invoke("list_app_ids"));
      items = ids.appIds ?? [];
      const left = ids.availableQuantity ?? "unknown";
      const limit = ids.maxQuantity == null ? "" : ` (limit: ${ids.maxQuantity})`;
      explain.textContent = `${items.length} App ID${items.length === 1 ? "" : "s"} registered; ${left} more allowed now${limit}. On a free account, deleting an ID may not restore the weekly allowance before it expires.`;
    }
  } catch (error) {
    if (!box.contains(list)) return;
    explain.textContent = `Couldn't list them: ${messageOf(error)}`;
    return;
  }
  if (!box.contains(list)) return;
  if (!items.length) {
    explain.textContent += certificates ? " The account has no development certificates." : " The account has no App IDs.";
    return;
  }

  for (const item of items) {
    const row = template("t-device");
    const input = /** @type {HTMLInputElement} */ (row.querySelector("input"));
    input.name = "slot";
    const title = /** @type {HTMLElement} */ (row.querySelector(".choice-title"));
    const meta = /** @type {HTMLElement} */ (row.querySelector(".choice-meta"));
    const badge = /** @type {HTMLElement} */ (row.querySelector(".badge"));
    if (certificates) {
      title.textContent = `${item.name ?? "Certificate"} (${item.platform ?? "any platform"})`;
      meta.textContent = `Made on “${item.machineName ?? "?"}”, expires ${day(item.expires)}`;
      badge.textContent = item.inUseHere ? "This computer signs with it" : item.madeByApparition ? "Apparition on another device" : "";
    } else {
      title.textContent = item.identifier ?? "?";
      meta.textContent = `${item.name ?? ""}, expires ${day(item.expires)}`;
      badge.textContent = item.expires && new Date(item.expires).getTime() < Date.now() ? "Expired" : "";
    }
    input.addEventListener("change", () => {
      chosen = item;
      confirm.hidden = false;
      typed.value = "";
      button.disabled = true;
      $(box, "warning").textContent = certificates
        ? item.inUseHere
          ? "This computer signs with it: apps it installed stop opening until they're installed again."
          : item.madeByApparition
            ? "Apps that device signed with it stop opening until it signs them again, with a new certificate that needs a free slot. Receiving its key (above) keeps them working instead."
            : "Apps signed with it (by Xcode or another tool) stop opening until signed again; that tool makes a new one when it next signs."
        : "Apps using it keep working until their profile expires. A free account may refuse until the ID expires, and a deleted ID can still count against this week's ten.";
      typed.focus();
    });
    list.append(row);
  }
  filter.addEventListener("input", () => {
    const query = filter.value.trim().toLocaleLowerCase();
    let visible = 0;
    for (const row of list.children) {
      const hide = !row.textContent.toLocaleLowerCase().includes(query);
      row.hidden = hide;
      if (!hide) visible++;
    }
    // A hidden selection must never remain the target of a destructive action.
    chosen = null;
    confirm.hidden = true;
    list.querySelectorAll("input").forEach((input) => { input.checked = false; });
    $(box, "no-matches").hidden = visible > 0;
  });
  box.scrollIntoView({ block: "nearest", behavior: "smooth" });

  typed.addEventListener("input", () => (button.disabled = typed.value.trim() !== word));
  $(box, "keep").addEventListener("click", () => (box.hidden = true));
  button.addEventListener("click", async () => {
    if (!chosen || typed.value.trim() !== word) return;
    setBusy(button, true);
    try {
      if (certificates) await invoke("revoke_certificate", { serial: chosen.serialNumber, confirmation: typed.value });
      else await invoke("delete_app_id", { appIdId: chosen.appIdId, confirmation: typed.value });
      if (manager) await showSlots(box, slot, retry, undefined, true);
      else {
        box.hidden = true;
        retry();
      }
    } catch (error) {
      $(box, "warning").textContent = messageOf(error);
      setBusy(button, false);
    }
  });
}

// ------------------------------------------------------------ 6. done

function done() {
  panel.append(template("t-done"));
  updatePreferences.draw();
  const result = state.result ?? {};
  const isApparition = result.isApparition ?? (result.bundleId ?? "").startsWith("com.illixion.apparition");
  const name = isApparition ? "Apparition" : result.label ?? result.bundleId;
  const watchNames = (result.watches ?? []).filter((/** @type {any} */ w) => !w.error).map((/** @type {any} */ w) => w.name).join(", ");
  $(panel, "title").textContent = result.watchOnly ? `${name} is on ${watchNames}.` : `${name} is ${result.update ? "updated" : "on"} ${result.deviceName ?? "your device"}.`;
  $(panel, "lede").textContent = result.watchOnly ? `Sent to the Apple Watch through ${result.deviceName ?? "the iPhone"}.` : result.queued
    ? `Sent in ${result.seconds?.toFixed(1)} s, and ${result.deviceName ?? "the device"} is still installing it. If the app is open there, the update finishes when it's closed.`
    : `Installed in ${result.seconds?.toFixed(1)} s.${result.closed ? " It was open there, so it was closed for the update." : ""}`;

  const next = $(panel, "next");
  const add = (/** @type {string} */ text) => {
    const li = document.createElement("li");
    li.textContent = text;
    next.append(li);
  };
  add(result.watchOnly ? `Open ${name} from the Watch's apps.` : `Open ${name} from the device's apps.`);
  add(`If ${result.platform ?? "visionOS"} says the developer isn't trusted, trust it in Settings > General > VPN & Device Management.`);
  if (isApparition && result.handoff === "appleId")
    add(`Sign in to Apparition with ${result.appleId}. It finds the certificate this computer shared, so the two use one of the account's certificate slots.`);
  if (isApparition && result.handoff === "certificate")
    add("In Apparition, enter the .p12's password once: the certificate and profiles are waiting in its inbox.");
  if (isApparition && result.pairingHandedOver)
    add(`Apparition got its own pairing with this ${result.platform === "visionOS" ? "headset" : "device"}, so it can sign and install there.`);
  if (isApparition && state.signing?.mode === "appleId")
    add("Apps signed with a free Apple ID last 7 days. A daily Shortcuts automation with Apparition's Refresh Apps action keeps them signed.");

  const extra = $(panel, "extra");
  const notes = [];
  if (result.migrationPending) notes.push("The new app is installed, but its data transfer is incomplete. The old copy was kept. Install again with Keep existing app data to retry.");
  if (result.handoffError) notes.push(`Couldn't share the signing with Apparition (${result.handoffError}). Sign in there yourself instead.`);
  if (result.strippedExtensions?.length) notes.push(`Left out extensions: ${result.strippedExtensions.join(", ")}.`);
  for (const w of result.watches ?? []) notes.push(w.error ? `The watch app didn't install on ${w.name}: ${w.error}` : `The watch app is on ${w.name}.`);
  extra.hidden = !notes.length;
  extra.textContent = notes.join(" ");

  $(panel, "again").addEventListener("click", () => go("install"));
}

// ------------------------------------------------------------ prompts (PIN, 2FA)

const dialog = /** @type {HTMLDialogElement} */ (document.getElementById("prompt"));
const promptForm = /** @type {HTMLFormElement} */ (document.getElementById("prompt-form"));
const promptCode = /** @type {HTMLInputElement} */ (document.getElementById("prompt-code"));
const promptError = /** @type {HTMLElement} */ (document.getElementById("prompt-error"));
const promptSms = /** @type {HTMLElement} */ (document.getElementById("prompt-sms"));
/** @type {any} */ let promptKind = null;
let installPlanReply = null;
/** What a pairing would grant, in words. Developer access is the one to think about: it lets
 * that computer install builds on your devices and read and drive your apps' debug servers. */
function pairingAsks(/** @type {string[] | undefined} */ grants) {
  const list = grants ?? ["anisette"];
  const parts = [];
  if (list.includes("anisette")) parts.push("to use this Mac's anisette");
  if (list.includes("developer")) parts.push("to build, deploy and debug apps through this Mac (it can send builds to your devices and drive your apps' debug servers)");
  return parts.join(", and ") || "to pair";
}

const companionPairingDialog = /** @type {HTMLDialogElement} */ (document.getElementById("companion-pairing-prompt"));
/** @type {number|null} */ let companionPairingId = null;

function openCompanionPairing(/** @type {any} */ payload) {
  const request = payload?.request;
  if (!Number.isInteger(payload?.id) || !request) return;
  companionPairingId = payload.id;
  document.getElementById("companion-pairing-text").textContent =
    `${request.deviceName || "A device"} (${request.peer || "unknown address"}) asks ${pairingAsks(request.grants)}.`;
  document.getElementById("companion-pairing-code").textContent = request.code || "";
  const error = document.getElementById("companion-pairing-error");
  error.hidden = true;
  error.textContent = "";
  document.getElementById("companion-pairing-allow").disabled = false;
  document.getElementById("companion-pairing-decline").disabled = false;
  showView("companion");
  if (!companionPairingDialog.open) companionPairingDialog.showModal();
  document.getElementById("companion-pairing-allow").focus();
}

async function decideCompanionPairing(/** @type {boolean} */ allow) {
  if (companionPairingId === null) return;
  const id = companionPairingId;
  const buttons = [document.getElementById("companion-pairing-allow"), document.getElementById("companion-pairing-decline")];
  buttons.forEach(button => button.disabled = true);
  try {
    const status = await invoke("companion_pairing_decide", { id, allow });
    companionPairingDialog.close();
    companionPairingId = null;
    drawCompanion(status);
  } catch (failure) {
    const error = document.getElementById("companion-pairing-error");
    error.textContent = messageOf(failure);
    error.hidden = false;
    buttons.forEach(button => button.disabled = false);
  }
}

document.getElementById("companion-pairing-allow")?.addEventListener("click", () => decideCompanionPairing(true));
document.getElementById("companion-pairing-decline")?.addEventListener("click", () => decideCompanionPairing(false));
companionPairingDialog.addEventListener("cancel", () => { companionPairingId = null; });

/** Review before provisioning: no IDs are registered while this dialog is open. */
function installPlanPrompt(prompt) {
  const box = promptSms;
  box.hidden = false;
  box.style.display = "block";
  box.style.maxHeight = "55vh";
  box.style.overflowY = "auto";
  const label = document.createElement("label");
  label.textContent = "Install using App ID";
  const select = document.createElement("select");
  select.style.width = "100%";
  const option = (id, text) => { const o = document.createElement("option"); o.value = id; o.textContent = text; select.append(o); return o; };
  option(prompt.defaultTarget, `${prompt.defaultTarget} (saved or usual assignment)`);
  for (const c of prompt.candidates) {
    if (c.id !== prompt.defaultTarget) {
      const o = option(c.id, `${c.id} — ${c.installed ?? c.owner ?? c.name ?? "registered"}${c.reservedChild ? " (reserved)" : ""}`);
      o.disabled = Boolean(c.reservedChild);
    }
  }
  select.hidden = !prompt.appleId;
  label.hidden = !prompt.appleId;
  label.append(select);
  box.append(label);
  const notice = document.createElement("p");
  box.append(notice);
  const selected = new Set(prompt.selected);
  const inputs = [];
  for (const extension of prompt.inventory.extensions) {
    const row = document.createElement("label");
    row.className = "check";
    const input = document.createElement("input");
    input.type = "checkbox";
    input.value = extension.id;
    input.checked = selected.has(extension.id);
    input.disabled = (prompt.unsupported ?? []).includes(extension.id) || (prompt.appleId && extension.extensionKit);
    if (input.disabled) input.checked = false;
    inputs.push(input);
    row.append(input, document.createTextNode(`${extension.watch ? "Apple Watch app: " : ""}${extension.name} (${extension.id})${extension.watch ? " — installed on the paired Watch, uses an App ID; untick to keep the Watch free" : ""}${input.disabled ? (prompt.appleId ? " — ExtensionKit requires imported profiles" : " — no imported profile covers this extension") : ""}`));
    box.append(row);
  }
  if (!inputs.length) box.append(document.createTextNode("This app has no embedded extensions."));
  const quota = document.createElement("p");
  box.append(quota);
  const transfer = document.createElement("p");
  box.append(transfer);
  const data = document.createElement("select");
  data.setAttribute("aria-label", "Existing app data");
  data.style.width = "100%";
  for (const [value, text] of [["", "Choose how to handle app data"], ["keep", "Keep existing app data"], ["discard", "Discard destination app data"]]) {
    const o = document.createElement("option"); o.value = value; o.textContent = text; data.append(o);
  }
  box.append(data);
  const warning = document.createElement("p");
  warning.textContent = "An App ID identifies an installed app. Reusing an occupied ID replaces that app. Keeping its data can give the new app access to its container. Discarding uninstalls it before installation; a failed install leaves it uninstalled. Keychain and shared app group data may remain. Data copied from another ID includes Documents and Library only.";
  box.append(warning);
  const update = () => {
    transfer.textContent = prompt.dataSource && prompt.dataSource !== select.value ? `Keep will copy Documents and Library from ${prompt.dataSource}, then remove that copy only after every file arrives.` : "Keep retains the destination app’s current data container.";
    const candidate = prompt.candidates.find(c => c.id === select.value);
    const occupied = candidate?.installed ?? (!prompt.appleId ? prompt.destination : null);
    notice.textContent = occupied ? `Replaces ${occupied} on this device.` : "An account registration may also be in use on another device or by another installer.";
    const requested = [select.value, ...inputs.filter(i => i.checked).map(i => select.value + i.value.slice(prompt.inventory.id.length))];
    const count = requested.filter(id => !prompt.candidates.some(c => c.id === id)).length;
    const overQuota = prompt.appleId && Number.isInteger(prompt.available) && count > prompt.available;
    quota.textContent = prompt.appleId ? `${count} new App ID${count === 1 ? "" : "s"} required. Apple reports ${prompt.available ?? "unknown"} available.${overQuota ? " Remove extensions or reuse registered IDs to continue." : ""}` : `Uses your imported provisioning profiles; installs as ${prompt.defaultTarget}.`;
    document.getElementById("prompt-submit").disabled = overQuota || Boolean(candidate?.reservedChild);
  };
  select.addEventListener("change", () => { data.value = ""; update(); });
  inputs.forEach(i => i.addEventListener("change", update));
  update();
  installPlanReply = () => {
    if (!data.value) { data.focus(); return null; }
    return { kind: "installPlan", target: select.value, extensions: inputs.filter(i => i.checked).map(i => i.value), dataMode: data.value };
  };
}

/** @param {any} prompt */
function openPrompt(prompt) {
  promptKind = prompt.kind;
  installPlanReply = null;
  promptSms.removeAttribute("style");
  promptCode.value = "";
  promptSms.replaceChildren();
  promptSms.hidden = true;
  promptError.hidden = true;
  const title = /** @type {HTMLElement} */ (document.getElementById("prompt-title"));
  const text = /** @type {HTMLElement} */ (document.getElementById("prompt-text"));
  const shown = /** @type {HTMLElement} */ (document.getElementById("prompt-show"));
  const submit = /** @type {HTMLElement} */ (document.getElementById("prompt-submit"));
  const cancel = /** @type {HTMLElement} */ (document.getElementById("prompt-cancel"));
  const comparing = prompt.kind === "nearbyCode";
  shown.hidden = !comparing;
  promptCode.hidden = comparing;
  submit.textContent = comparing ? "It matches" : "Continue";
  submit.disabled = false;
  cancel.textContent = comparing ? "It's different" : "Cancel";
  if (prompt.kind === "installPlan") {
    title.textContent = `Review ${prompt.inventory.name}`;
    text.textContent = "Choose extensions and a stable App ID before signing. Apple ID assignments and extension choices are remembered on this computer.";
    promptCode.hidden = true;
    installPlanPrompt(prompt);
  } else if (comparing) {
    title.textContent = `Does ${prompt.peer} show this code?`;
    text.textContent = "Check the six digits on both screens. A different code means something else on the network answered: nothing is handed over unless both of you confirm.";
    shown.textContent = `${prompt.code.slice(0, 3)} ${prompt.code.slice(3)}`;
  } else if (prompt.kind === "pin") {
    if (prompt.again) {
      title.textContent = "The device wants to pair again";
      text.textContent =
        "Open its pairing screen (Settings > General > Remote Devices on Vision Pro, Settings > Remotes and Devices > Remote App and Devices on Apple TV) and type the code it shows.";
    } else if (prompt.second) {
      title.textContent = "Type the second code";
      text.textContent = "The device now shows another code. An Apple TV asks twice: this one is for Apparition on it.";
    } else {
      title.textContent = "Type the code from the device";
      text.textContent = "The device's pairing screen now shows a code.";
    }
    promptCode.maxLength = 6;
  } else {
    title.textContent = "Two-factor code";
    text.textContent = "Apple sent a code to your trusted devices. Type it here.";
    promptCode.maxLength = 8;
    if (prompt.lastError) {
      promptError.hidden = false;
      promptError.textContent = prompt.lastError;
    }
    promptSms.hidden = false;
    for (const number of prompt.numbers ?? []) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "link";
      button.textContent = `Text a code to ${number.number}`;
      button.addEventListener("click", () => answer({ kind: "sms", id: number.id }));
      promptSms.append(button);
    }
    const resend = document.createElement("button");
    resend.type = "button";
    resend.className = "link";
    resend.textContent = "Send a new code";
    resend.addEventListener("click", () => answer({ kind: "resend" }));
    promptSms.append(resend);
  }
  if (!dialog.open) dialog.showModal();
  if (comparing || prompt.kind === "installPlan") submit.focus();
  else promptCode.focus();
}

function answer(/** @type {any} */ reply) {
  dialog.close();
  promptCode.value = "";
  invoke("answer_prompt", { answer: reply }).catch(() => {});
}

promptForm.addEventListener("submit", (event) => {
  event.preventDefault();
  if (promptKind === "installPlan") { const reply = installPlanReply?.(); if (reply) answer(reply); return; }
  if (promptKind === "nearbyCode") return answer({ kind: "match", matches: true });
  const code = promptCode.value.replace(/\D/g, "");
  if (!code) return promptCode.focus();
  answer(promptKind === "pin" ? { kind: "pin", pin: code } : { kind: "code", code });
});
document.getElementById("prompt-cancel")?.addEventListener("click", () => {
  dialog.close();
  invoke("cancel_prompt");
});
dialog.addEventListener("cancel", () => invoke("cancel_prompt"));

// ------------------------------------------------------------ companion

const installView = /** @type {HTMLElement} */ (document.getElementById("view-install"));
const companionView = /** @type {HTMLElement} */ (document.getElementById("view-companion"));
const companionPanel = /** @type {HTMLElement} */ (document.getElementById("companion-panel"));
const tabInstall = /** @type {HTMLButtonElement} */ (document.getElementById("tab-install"));
const tabCompanion = /** @type {HTMLButtonElement} */ (document.getElementById("tab-companion"));
const developerView = /** @type {HTMLElement} */ (document.getElementById("view-developer"));
// Apps, Debug and Builds are the Developer view's three sections, promoted to the top bar.
const DEVELOPER_TABS = /** @type {const} */ ({ projects: "tab-apps", debug: "tab-debug", activity: "tab-builds" });
const developerTabs = Object.fromEntries(Object.entries(DEVELOPER_TABS).map(([section, id]) => [section, /** @type {HTMLButtonElement} */ (document.getElementById(id))]));
const developer = new DeveloperView(document.getElementById("developer-panel"), invoke);
/** @type {number|undefined} */ let companionTimer;
/** What an action in progress, or one that just failed, has to say. @type {{text: string, kind?: "error"|"good"|""}|null} */
let companionNote = null;
/** The third-party server's address is being chosen or edited, though the companion doesn't offer it (yet). */
let serverEditing = false;
/** The address field holds what the person typed, which a refresh mustn't overwrite. */
let serverDirty = false;
/** What the last Check of the server found. @type {{text: string, kind?: string}|null} */
let serverNote = null;

const modeView = /** @type {HTMLElement} */ (document.getElementById("view-mode"));
const viewsBar = /** @type {HTMLElement} */ (document.querySelector(".views"));
const modeSwitch = /** @type {HTMLElement} */ (document.getElementById("mode-switch"));

/** The tabs each mode shows, in order. Develop adds the Develop tab and turns Install into Devices. */
const MODE_TABS = { install: [tabInstall, tabCompanion], develop: [...Object.values(developerTabs), tabInstall, tabCompanion] };

function syncDeveloperTabs(/** @type {boolean} */ showing) {
  for (const [name, tab] of Object.entries(developerTabs)) tab.setAttribute("aria-selected", String(showing && name === developer.section));
}

function showView(/** @type {"install"|"companion"|"developer"|"mode"} */ view, /** @type {string} */ section = developer.section) {
  const companion = view === "companion";
  modeView.hidden = view !== "mode";
  installView.hidden = view !== "install";
  companionView.hidden = !companion;
  tabInstall.setAttribute("aria-selected", String(view === "install"));
  developerView.hidden = view !== "developer";
  if (view === "developer") developer.select(section);
  syncDeveloperTabs(view === "developer");
  developer.show(view === "developer");
  tabCompanion.setAttribute("aria-selected", String(companion));
  clearInterval(companionTimer);
  if (view === "mode") modeView.querySelector("main")?.focus({ preventScroll: true });
  if (companion) {
    if (!companionPanel.firstElementChild) buildCompanion();
    refreshCompanion();
    companionTimer = setInterval(refreshCompanion, 2000);
    companionPanel.focus({ preventScroll: true });
  }
  developer.placeSetup(companion);
}

/** Shows the tabs of `mode` and marks the switch. Does not save or navigate. */
function applyMode(/** @type {"install"|"develop"} */ mode) {
  state.useMode = mode;
  document.body.dataset.mode = mode;
  tabInstall.textContent = mode === "develop" ? "Devices" : "Install";
  viewsBar.replaceChildren(...MODE_TABS[mode]);
  viewsBar.hidden = modeSwitch.hidden = false;
  for (const button of modeSwitch.querySelectorAll("button")) button.setAttribute("aria-pressed", String(button.dataset.mode === mode));
}

/** Where a mode opens: Develop on its projects, Install on the install steps. */
const landing = (/** @type {"install"|"develop"} */ mode) => (mode === "develop" ? "developer" : "install");

async function chooseMode(/** @type {"install"|"develop"} */ mode) {
  const changed = state.useMode !== mode;
  applyMode(mode);
  try { await invoke("set_mode", { mode }); } catch {}
  state.settings = { ...state.settings, mode };
  // The install steps word their first screen by mode; redraw it only if it is showing.
  if (changed && state.step === "welcome") render();
  showView(landing(mode));
}
for (const button of document.querySelectorAll("[data-mode]")) {
  button.addEventListener("click", () => chooseMode(/** @type {any} */ (button).dataset.mode));
}
developer.go = () => showView("install");
developer.onSelect = () => syncDeveloperTabs(!developerView.hidden);
tabInstall.addEventListener("click", () => showView("install"));
tabCompanion.addEventListener("click", () => showView("companion"));
for (const [section, tab] of Object.entries(developerTabs)) tab.addEventListener("click", () => showView("developer", section));
// A badge on a top tab (running debug apps, builds in progress) comes from the Developer view's data.
developer.onBadge = (section, count) => {
  const badge = document.getElementById(section === "debug" ? "debug-count" : "builds-count");
  if (!badge) return;
  badge.hidden = !count;
  badge.textContent = String(count || "");
};

function buildCompanion() {
  companionPanel.append(template("t-companion"));
  updatePreferences.draw();
  const where = state.info.platform === "macos-universal" ? "menu bar" : "tray";
  companionPanel.querySelectorAll('[data-role="where"]').forEach((el) => (el.textContent = where));
  $(companionPanel, "windows-firewall").hidden = !state.info.platform.startsWith("windows");
  const relayOn = /** @type {HTMLInputElement} */ ($(companionPanel, "relay-on"));
  relayOn.addEventListener("change", () => companionCall("companion_set_relay", { on: relayOn.checked }));
  const advertiseOn = /** @type {HTMLInputElement} */ ($(companionPanel, "advertise-on"));
  advertiseOn.addEventListener("change", () => companionCall("companion_set_advertise", { on: advertiseOn.checked }));

  for (const [role, allow] of [["pairing-allow", true], ["pairing-deny", false]]) {
    $(companionPanel, role).addEventListener("click", () => {
      const id = Number($(companionPanel, "pairing-request").dataset.id);
      if (id) companionCall("companion_pairing_decide", { id, allow });
    });
  }
  companionPanel.querySelectorAll('input[name="anisette-mode"]').forEach((input) =>
    input.addEventListener("change", () => {
      const mode = /** @type {HTMLInputElement} */ (input).value;
      serverNote = null;
      // A third-party server needs its address first: choosing it opens the address, and "Offer this
      // server" is what starts offering it (so a typo is never offered, and what was on stays on).
      serverEditing = mode === "server";
      if (serverEditing) return refreshCompanion();
      const note = mode === "builtin" ? "Starting this Mac's anisette…" : mode === "vm" ? "Offering the anisette VM's…" : mode === "android" ? "Starting Apparition's own anisette (it fetches its libraries the first time)…" : "";
      companionCall("companion_set_anisette", { mode }, note);
    }),
  );
  const serverAddress = /** @type {HTMLInputElement} */ ($(companionPanel, "server-address"));
  serverAddress.addEventListener("input", () => {
    serverDirty = true;
    serverNote = null;
  });
  $(companionPanel, "server-use").addEventListener("click", async () => {
    serverNote = null;
    await companionCall("companion_set_anisette", { mode: "server", server: serverAddress.value }, "Offering that server's anisette…");
    // Done unless it was refused (the note says why, and the address stays to be fixed).
    if (!companionNote) {
      serverEditing = false;
      serverDirty = false;
      refreshCompanion();
    }
  });
  $(companionPanel, "server-check").addEventListener("click", async (event) => {
    const note = document.createElement("p");
    await checkAnisetteServer(serverAddress.value, /** @type {HTMLButtonElement} */ (event.currentTarget), note);
    serverNote = { text: note.textContent ?? "", kind: note.className.replace("status", "").trim() };
    refreshCompanion();
  });
  // The anisette VM: one button, then progress until it is set up (vm.rs does the work, resumable).
  const vmRun = /** @type {HTMLButtonElement} */ ($(companionPanel, "vm-setup-run"));
  const startVm = async (/** @type {string|null} */ ipsw) => {
    vmNote = null;
    try {
      await invoke("vm_setup", { ipsw });
    } catch (error) {
      vmNote = { text: messageOf(error), kind: "error" };
    }
    refreshCompanion();
  };
  vmRun.addEventListener("click", () => startVm(null));
  $(companionPanel, "vm-pick").addEventListener("click", async () => {
    const [path] = await invoke("pick_file", { kind: "ipsw" });
    if (path) startVm(path);
  });
  $(companionPanel, "vm-cancel").addEventListener("click", async () => {
    await invoke("vm_cancel");
    refreshCompanion();
  });
  const vmCheck = /** @type {HTMLButtonElement} */ ($(companionPanel, "vm-check"));
  vmCheck.addEventListener("click", async () => {
    setBusy(vmCheck, true);
    vmNote = { text: "Asking Apple with the VM's anisette (no Apple ID is used)…" };
    refreshCompanion();
    try {
      await invoke("vm_check");
      vmNote = { text: "Apple accepts this VM's anisette.", kind: "good" };
    } catch (error) {
      vmNote = { text: messageOf(error), kind: "error" };
    }
    setBusy(vmCheck, false);
    refreshCompanion();
  });
  // The VM's own window, to use it by hand (an Apple Account sign-in in its System Settings, say).
  const vmShow = /** @type {HTMLButtonElement} */ ($(companionPanel, "vm-show"));
  vmShow.addEventListener("click", async () => {
    setBusy(vmShow, true);
    try {
      await invoke("vm_show");
      vmNote = { text: "The VM's window is open. Its account is apparition (Type its password logs in). It keeps running while the window is open; closing it hides the VM again." };
    } catch (error) {
      vmNote = { text: messageOf(error), kind: "error" };
    }
    setBusy(vmShow, false);
    refreshCompanion();
  });
  const vmPassword = /** @type {HTMLButtonElement} */ ($(companionPanel, "vm-password"));
  vmPassword.addEventListener("click", async () => {
    setBusy(vmPassword, true);
    try {
      await invoke("vm_type_password");
      vmNote = { text: "Typed the password of the VM's account (apparition) into its window.", kind: "good" };
    } catch (error) {
      vmNote = { text: messageOf(error), kind: "error" };
    }
    setBusy(vmPassword, false);
    refreshCompanion();
  });
  // Removing takes a second click: it deletes 30 GB that took 45 minutes to make.
  const vmRemove = /** @type {HTMLButtonElement} */ ($(companionPanel, "vm-remove"));
  let removeArmed = 0;
  vmRemove.addEventListener("click", async () => {
    // Setup removes the restore image, so a new VM is the 20 GB download again (or a copy you have).
    const download = state.vm && !state.vm.imageCached ? " Setting one up again downloads macOS again (20 GB)." : "";
    if (!removeArmed) {
      vmRemove.textContent = "Move it to the Trash";
      vmNote = { text: `Nothing needs this VM to be made again: removing it only gives back its disk space.${download}` };
      drawVm();
      removeArmed = window.setTimeout(() => {
        removeArmed = 0;
        vmRemove.textContent = "Remove…";
        vmNote = null;
        drawVm();
      }, 5000);
      return;
    }
    window.clearTimeout(removeArmed);
    removeArmed = 0;
    vmRemove.textContent = "Remove…";
    try {
      state.vm = await invoke("vm_remove");
      state.vmAnisette = false;
      vmNote = { text: `The anisette VM is in the Trash. Empty it to get the disk space back.${download}` };
    } catch (error) {
      vmNote = { text: messageOf(error), kind: "error" };
    }
    refreshCompanion();
  });
  listen("vm-progress", (/** @type {any} */ e) => {
    if (state.vm) state.vm = { ...state.vm, settingUp: true, progress: e.payload };
    drawVm();
  });
  listen("vm-finished", (/** @type {any} */ e) => {
    vmNote = !e.payload.ok ? null
      : e.payload.update ? { text: "The VM has this version's programs, and Apple accepts its anisette.", kind: "good" }
      : { text: "The anisette VM is set up, and the companion offers it to paired devices.", kind: "good" };
    refreshCompanion();
  });
  // A macOS VM in recoveryOS can't paste: show the short line to type, and what it has fetched.
  const vmStatus = $(companionPanel, "vm-setup-status");
  const vmLines = $(companionPanel, "vm-setup-lines");
  const vmStart = /** @type {HTMLButtonElement} */ ($(companionPanel, "vm-setup-start"));
  const vmStop = async () => {
    await invoke("vm_setup_stop");
    vmLines.hidden = true;
    vmStart.hidden = false;
  };
  vmStart.addEventListener("click", async () => {
    try {
      const { port, addresses } = await invoke("vm_setup_start");
      $(companionPanel, "vm-setup-list").replaceChildren(
        ...addresses.map((/** @type {string} */ ip) => {
          const li = document.createElement("li");
          const code = document.createElement("code");
          code.textContent = `curl -s ${ip}:${port}|bash`;
          li.append(code);
          return li;
        }),
      );
      vmLines.hidden = false;
      vmStart.hidden = true;
      setStatus(vmStatus, "Waiting for the VM. It stops by itself after 15 minutes.");
    } catch (error) {
      setStatus(vmStatus, String(error?.message ?? error), "error");
    }
  });
  $(companionPanel, "vm-setup-stop").addEventListener("click", async () => {
    await vmStop();
    setStatus(vmStatus, "");
  });
  listen("vm-setup-progress", async (/** @type {any} */ e) => {
    const p = e.payload;
    if (p.done) {
      await vmStop();
      setStatus(vmStatus, "The VM has everything. Let it restart, then pair with it (the walkthrough, step 6).", "good");
    } else if (p.timedOut) {
      await vmStop();
      setStatus(vmStatus, "Nobody finished in 15 minutes. Show the line again to retry.", "error");
    } else {
      setStatus(vmStatus, p.program || p.helper ? "The VM is downloading…" : "The VM fetched the script.");
    }
  });
  // Also the only way to quit where there's no tray to show the menu (niri, plain GNOME).
  $(companionPanel, "quit").addEventListener("click", () => invoke("quit_app"));
  const login = /** @type {HTMLInputElement} */ ($(companionPanel, "login"));
  invoke("launch_at_login").then((/** @type {boolean} */ on) => (login.checked = on));
  login.addEventListener("change", async () => {
    try {
      login.checked = await invoke("set_launch_at_login", { on: login.checked });
    } catch (error) {
      login.checked = !login.checked;
      companionNote = { text: messageOf(error), kind: "error" };
      refreshCompanion();
    }
  });
}

/** Runs a companion command and shows the status it returns (or its error). */
async function companionCall(/** @type {string} */ command, /** @type {any} */ args, /** @type {string} */ progress = "") {
  companionNote = progress ? { text: progress } : null;
  refreshCompanion();
  try {
    const status = await invoke(command, args);
    companionNote = null;
    drawCompanion(status);
  } catch (error) {
    companionNote = { text: messageOf(error), kind: "error" };
    refreshCompanion();
  }
}

async function refreshCompanion() {
  try {
    const [status, vm] = await Promise.all([invoke("companion_status"), invoke("vm_status")]);
    state.vm = previewVm ?? vm;
    state.vmAnisette = !!vm?.ready;
    drawCompanion(status);
  } catch {}
}

/** Debug builds: the anisette VM's group in a sample state (APPARITION_DESKTOP_PREVIEW=vm-new, vm-setup, vm-error;
 * plain `vm` shows the real one). */
const VM_PREVIEWS = {
  "vm-new": { supported: true, exists: false, installed: false, ready: false, settingUp: false, imageCached: false, diskBytes: 0 },
  "vm-setup": { supported: true, exists: true, installed: true, ready: false, settingUp: true, imageCached: true, diskBytes: 23e9,
    progress: { step: "recovery", text: "recoveryOS is turning SIP off and installing Apparition", fraction: null } },
  "vm-install": { supported: true, exists: true, installed: false, ready: false, settingUp: true, imageCached: true, diskBytes: 9e9,
    progress: { step: "install", text: "Installing macOS in the VM", fraction: 0.42 } },
  "vm-error": { supported: true, exists: true, installed: true, ready: false, settingUp: false, imageCached: true, diskBytes: 23e9,
    error: "recoveryOS's csrutil didn't accept the VM's account; ~/Library/Application Support/Apparition/anisette.vm/screen.png has its screen. Run setup again to retry." },
  "vm-outdated": { supported: true, exists: true, installed: true, ready: true, outdated: true, settingUp: false, imageCached: false, diskBytes: 27e9, suspended: true },
  "vm-stale": { supported: true, exists: true, installed: true, ready: true, stale: true, settingUp: false, imageCached: false, diskBytes: 27e9, suspended: true },
  "vm-handing-over": { supported: true, exists: true, installed: true, ready: true, stale: true, updating: true, handingOver: true, settingUp: true, imageCached: false, diskBytes: 27e9, suspended: false, serving: true, phase: "running",
    progress: { step: "update", text: "Handing them to the VM, which checks the release's signature itself and keeps running", fraction: null } },
  "vm-updating": { supported: true, exists: true, installed: true, ready: true, outdated: true, updating: true, settingUp: true, imageCached: false, diskBytes: 27e9,
    progress: { step: "update", text: "recoveryOS is putting this app's programs into the VM", fraction: null } },
};
/** @type {any} */
let previewVm = null;

/** A note under the anisette VM's buttons: the last check's answer, an error. */
/** @type {{text: string, kind?: string}|null} */
let vmNote = null;

/** The anisette VM's group: what it is doing, or what can be done with it. */
function drawVm() {
  const vm = state.vm;
  if (!companionPanel.firstElementChild || !vm) return;
  const group = $(companionPanel, "vm");
  // Only where it can run: a Mac with Apple silicon, and one with no anisette of its own (or one that has it set up).
  group.hidden = !vm.supported || (!!state.companion?.anisette?.native && !vm.exists);
  if (group.hidden) return;
  const busy = !!vm.settingUp;
  const progress = vm.progress;
  $(companionPanel, "vm-progress").hidden = !busy;
  if (busy) {
    $(companionPanel, "vm-step").textContent = progress?.text ?? "Starting…";
    const bar = $(companionPanel, "vm-bar");
    const known = progress?.fraction != null;
    bar.parentElement?.classList.toggle("indeterminate", !known);
    bar.style.width = known ? `${Math.round(progress.fraction * 100)}%` : "";
  }
  const pill = $(companionPanel, "vm-pill");
  const [label, kind] = vm.updating ? ["Updating", "info"]
    : busy ? ["Setting up", "info"]
    : vm.ready ? (vm.serving && vm.phase === "running" ? ["Running", "good"] : ["Saved", "good"])
    : vm.exists ? ["Not finished", "warn"]
    : ["", ""];
  pill.hidden = !label;
  pill.textContent = label;
  pill.className = `pill ${kind}`.trim();
  $(companionPanel, "vm-intro").hidden = vm.ready;
  $(companionPanel, "vm-memory-note").hidden = vm.ready || busy;
  const run = $(companionPanel, "vm-setup-run");
  // A VM set up by an older app gets this one's programs (the app starts that itself at launch; this retries it).
  run.hidden = busy || vm.updating || (vm.ready && !vm.outdated && !vm.stale);
  run.textContent = vm.outdated || vm.stale ? "Update the VM's programs" : vm.exists ? "Finish setting it up" : "Set up the anisette VM";
  $(companionPanel, "vm-pick").hidden = busy || vm.ready || vm.installed || vm.imageCached;
  $(companionPanel, "vm-cancel").hidden = !busy;
  $(companionPanel, "vm-check").hidden = busy || !vm.ready;
  $(companionPanel, "vm-show").hidden = busy || !vm.ready;
  $(companionPanel, "vm-password").hidden = busy || !vm.ready;
  $(companionPanel, "vm-remove").hidden = busy || !vm.exists;
  const disk = vm.diskBytes ? ` It takes ${(vm.diskBytes / 1e9).toFixed(1)} GB of disk.` : "";
  const text = vmNote?.text
    ?? (vm.error && !busy ? vm.error
      : busy ? (vm.handingOver ? "Fetching this version's programs for the VM from its release. The VM checks the release's signature itself and keeps running, so sign-ins keep working."
        : vm.updating ? "The VM, its account and its anisette stay as they are. Until this is done (a few minutes), sign-ins can't use it." : "")
      : vm.updating ? "Its programs are being updated from a terminal (apparition vm update)."
      : vm.outdated ? "This version of Apparition has newer programs for the VM. Updating them takes a few minutes from the VM's own recoveryOS, with no download, and keeps the VM as it is."
      : vm.stale ? "This version of Apparition has newer programs for the VM. It takes them from the release in seconds, while it runs."
      : vm.ready ? (vm.serving && vm.phase === "running"
          ? `Running. It is saved to disk and stopped after a few idle minutes.${disk}`
          : `Saved to disk, using no memory. It wakes in about a second when a device signs in.${disk}`)
      : vm.exists ? "Setup stopped partway; it carries on from where it was." : "");
  const kindOf = vmNote?.kind ?? (vm.error && !busy ? "error" : "");
  setStatus($(companionPanel, "vm-status"), text, kindOf);
}

const amount = (/** @type {number} */ bytes) => (bytes < 1000 ? `${bytes} B` : megabytes(bytes));
const clock = (/** @type {number} */ seconds) => new Date(seconds * 1000).toLocaleTimeString(undefined, { timeStyle: "medium" });

function drawCompanion(/** @type {any} */ status) {
  const running = !!status.relay.running;
  const dot = /** @type {HTMLElement} */ (document.getElementById("companion-dot"));
  const label = running ? "Companion running" : "Companion stopped";
  dot.classList.toggle("on", running);
  dot.setAttribute("aria-label", label);
  dot.title = label;
  const stateLine = companionPanel.querySelector('[data-role="companion-state"]');
  if (stateLine) {
    stateLine.classList.toggle("on", running);
    stateLine.querySelector(".tab-dot")?.classList.toggle("on", running);
    $(companionPanel, "companion-state-text").textContent = running ? "Companion running" : "Companion stopped";
  }
  const before = JSON.stringify([state.companion?.relay?.running, state.companion?.anisette?.offered, state.companion?.anisette?.mode, state.companion?.anisette?.server]);
  state.companion = status;
  followOffer(status);
  if (before !== JSON.stringify([status.relay.running, status.anisette.offered, status.anisette.mode, status.anisette.server])) drawSetupBadge();
  if (!companionPanel.firstElementChild) return;
  const relayOn = /** @type {HTMLInputElement} */ ($(companionPanel, "relay-on"));
  relayOn.checked = status.relay.on;
  $(companionPanel, "relay-port").textContent = String(status.relay.port);
  $(companionPanel, "computer").textContent = status.computerName || "this computer";
  $(companionPanel, "computer-2").textContent = status.computerName || "this computer";
  const advertiseOn = /** @type {HTMLInputElement} */ ($(companionPanel, "advertise-on"));
  advertiseOn.checked = status.advertise.on;
  advertiseOn.disabled = !status.relay.on;
  $(companionPanel, "advertise-hint").hidden = !status.advertise.on;
  $(companionPanel, "advertise-off").hidden = status.advertise.on || !status.relay.on;
  $(companionPanel, "relay-address").textContent = status.advertise.address
    ? `${status.advertise.address}:${status.relay.port}`
    : `this computer's address, port ${status.relay.port}`;
  const badge = $(companionPanel, "relay-badge");
  badge.textContent = status.relay.running ? "On" : status.relay.error ? "Failed" : "Off";
  badge.className = `badge ${status.relay.running ? "online" : ""}`.trim();
  const relayError = $(companionPanel, "relay-error");
  setStatus(relayError, status.relay.error ?? "", "error");

  const table = $(companionPanel, "connections");
  const rows = status.connections.map((/** @type {any} */ c) => {
    const tr = document.createElement("tr");
    const cells = [clock(c.started), c.peer, String(c.port), c.outcome ? amount(c.toDevice) : "…", c.outcome ? amount(c.fromDevice) : "…", c.outcome ?? "open"];
    for (const text of cells) {
      const td = document.createElement("td");
      td.textContent = text;
      tr.append(td);
    }
    if (!c.outcome) tr.lastElementChild?.classList.add("open");
    return tr;
  });
  table.querySelector("tbody").replaceChildren(...rows);
  table.hidden = rows.length === 0;
  $(companionPanel, "no-connections").hidden = rows.length > 0 || !status.relay.running;

  $(companionPanel, "vm-setup").hidden = !status.anisette.builtinAvailable;
  const anisette = status.anisette;
  $(companionPanel, "mode-builtin").hidden = !anisette.native;
  $(companionPanel, "mode-vm").hidden = !state.vmAnisette && anisette.mode !== "vm";
  $(companionPanel, "mode-android").hidden = !anisette.androidAvailable && anisette.mode !== "android";
  $(companionPanel, "anisette-not-mac").hidden = anisette.native || state.vmAnisette;
  $(companionPanel, "anisette-not-mac-why").textContent = anisette.builtinAvailable
    ? "macOS 27 keeps it to itself outside a VM set up for it"
    : "only a Mac does";
  companionPanel.querySelectorAll('input[name="anisette-mode"]').forEach((el) => {
    const input = /** @type {HTMLInputElement} */ (el);
    input.checked = input.value === (serverEditing ? "server" : anisette.mode);
    // Offering anisette goes through the relay's advertisement, so it needs the relay.
    input.disabled = !status.relay.on;
  });
  // The third-party server's address: shown while it is offered or being chosen. It keeps what the person
  // is typing; otherwise it follows the companion's, or the one the sign-in already has.
  const serverConfig = $(companionPanel, "server-config");
  serverConfig.hidden = !(serverEditing || anisette.mode === "server");
  const serverAddress = /** @type {HTMLInputElement} */ ($(companionPanel, "server-address"));
  if (!serverDirty && document.activeElement !== serverAddress) {
    serverAddress.value = anisette.server || serverAddressOf(state.settings?.anisette);
  }
  /** @type {HTMLButtonElement} */ ($(companionPanel, "server-use")).disabled = !status.relay.on;
  const serverShown = serverNote ?? (serverEditing && companionNote?.kind === "error" ? companionNote : null);
  setStatus($(companionPanel, "server-status"), serverShown?.text ?? "", serverShown?.kind ?? "");

  const pairing = status.pairing;
  const builtin = anisette.mode === "builtin" || anisette.mode === "vm" || anisette.mode === "server";
  $(companionPanel, "access").hidden = !builtin;
  const accessPill = $(companionPanel, "access-pill");
  accessPill.textContent = "Encrypted · paired devices only";
  accessPill.className = "pill good";
  $(companionPanel, "access-paired-hint").hidden = false;
  const request = $(companionPanel, "pairing-request");
  const waiting = pairing.waiting[0];
  request.hidden = !waiting;
  if (waiting) {
    request.dataset.id = String(waiting.id);
    $(companionPanel, "pairing-device").textContent = waiting.request.deviceName || "A device";
    $(companionPanel, "pairing-peer").textContent = waiting.request.peer;
    $(companionPanel, "pairing-code").textContent = waiting.request.code;
  }
  const paired = $(companionPanel, "paired-devices");
  const pairedRows = pairing.devices.map((/** @type {any} */ device) => {
    const tr = document.createElement("tr");
    const name = (device.grants ?? []).includes("developer") ? `${device.name} · developer access` : device.name;
    for (const text of [name, new Date(device.paired * 1000).toLocaleDateString()]) {
      const td = document.createElement("td");
      td.textContent = text;
      tr.append(td);
    }
    const td = document.createElement("td");
    const forget = document.createElement("button");
    forget.type = "button";
    forget.className = "link";
    forget.textContent = "Unpair";
    forget.addEventListener("click", () => companionCall("companion_forget_device", { id: device.id }));
    td.append(forget);
    tr.append(td);
    return tr;
  });
  paired.querySelector("tbody").replaceChildren(...pairedRows);
  paired.hidden = !anisette.offered || pairedRows.length === 0;
  const text = companionNote?.text
    ?? anisette.error
    ?? (!status.relay.on
      ? "Turn the relay on to offer anisette."
      : serverEditing && anisette.mode !== "server"
        ? "Not offering the server yet: give its address and choose Offer this server. What was offered stays on until then."
        : anisette.offered === "builtin"
          ? "Offering this Mac's own anisette to paired devices and computers."
          : anisette.offered === "vm"
            ? "Offering the anisette VM's to paired devices and computers."
            : anisette.offered === "server"
              ? `Passing ${serverHost(anisette.server ?? "")}'s anisette to paired devices and computers.`
              : "");
  const kind = companionNote?.kind ?? (anisette.error ? "error" : anisette.offered ? "good" : "");
  setStatus($(companionPanel, "anisette-status"), text, kind);
  drawVm();
}

// ------------------------------------------------------------ start

async function start() {
  await listen("prompt", (/** @type {any} */ e) => openPrompt(e.payload));
  await listen("prompt-done", () => dialog.open && dialog.close());
  // A device asks to pair: show the matching code and approval in a focused dialog.
  await listen("companion-pairing", (/** @type {any} */ e) => openCompanionPairing(e.payload));
  state.info = await invoke("app_info");
  document.body.classList.toggle("mac", state.info.platform === "macos-universal");
  state.settings = await invoke("saved_settings");
  [state.builtinAnisette, state.pairedMac, state.vm, state.androidAnisette] = await Promise.all([invoke("builtin_anisette"), invoke("paired_mac"), invoke("vm_status"), invoke("android_anisette")]);
  state.vmAnisette = !!state.vm?.ready;
  drawSetupBadge();
  // The companion's status only polls on its own tab; the badge needs to know on every screen.
  setInterval(refreshCompanion, 5000);

  const updates = new UpdateBanner(
    /** @type {HTMLElement} */ (document.getElementById("update")), invoke, () => state.busy || dialog.open || updateDialog.open,
  );
  await updatePreferences.ready;
  await listen("auto-updates-changed", (/** @type {any} */ e) => updatePreferences.draw(e.payload));
  await listen("desktop-update", (/** @type {any} */ e) => updates.draw(e.payload));
  updates.draw(await invoke("update_status"));

  refreshCompanion();
  // Someone who used Apparition before modes existed is not asked: a saved Build service means
  // they develop, anything else means they install. They can switch from the header.
  let mode = state.settings.mode;
  if (!mode && (state.settings.email || state.info.preview)) {
    const service = await invoke("service_settings").catch(() => ({}));
    mode = service?.url ? "develop" : "install";
    if (!state.info.preview) invoke("set_mode", { mode }).catch(() => {});
  }
  if (mode) applyMode(mode);
  if (state.info.preview === "companion") {
    render();
    return showView("companion");
  }
  if (/^vm(-|$)/.test(state.info.preview ?? "")) {
    previewVm = VM_PREVIEWS[state.info.preview] ?? null;
    render();
    showView("companion");
    // The real VM (`vm`): from the anisette choice down; the samples: the VM's group.
    const target = previewVm ? '[data-role="vm"]' : '[data-role="anisette-modes"]';
    return setTimeout(() => companionPanel.querySelector(target)?.scrollIntoView({ block: "start" }), 600);
  }
  if (state.info.preview) return preview(state.info.preview);
  render();
  const macHost = state.info.platform === "macos-universal";
  const needs = modeView.querySelector('[data-role="develop-needs"]');
  if (needs && !macHost) needs.textContent = "Builds run on a Mac with Xcode: connect this computer to one.";
  showView(mode ? landing(mode) : "mode");
}

/** Debug builds only (the backend passes `preview` only there): open on a step with sample data. */
function preview(/** @type {string} */ step) {
  state.device = step === "pair" || step === "pin" || step === "usbpair" ? null : { udid: "PREVIEW", name: "Vision Pro", address: "192.0.2.10", osVersion: "27.2", platform: "visionOS" };
  state.pairSkipped = step !== "pair";
  if (step === "usbpair") state.selected = { kind: "usb", name: "iPhone or iPad", udid: "00008120-000A1B2C3D4E5F60", usb: true, key: "u" };
  if (step === "pair" || step === "pin") state.selected = { kind: "pair", name: "Vision Pro", address: "192.0.2.10", port: 51967, key: "x" };
  if (["signed", "install", "progress", "slots", "done"].includes(step)) {
    state.signing = { mode: "appleId", email: "you@example.com", teamId: "ABCDE12345", team: { name: "Your Name" } };
  }
  if (step === "done") {
    state.result = { bundleId: "com.illixion.apparition", deviceName: "Vision Pro", seconds: 18.4, queued: false, handoff: "appleId", appleId: "you@example.com", label: "Apparition" };
  }
  state.step = { signed: "signing", pin: "pair", usbpair: "pair", twofactor: "signing", progress: "install", slots: "install" }[step] ?? step;
  render();
  if (step === "progress" || step === "slots") {
    $(panel, "stages").hidden = false;
    $(panel, "advanced").hidden = true;
    const marks = step === "progress"
      ? { release: ["done", "Version 0.2.0, signed by ixion@YubiKey5-gpg"], download: ["done", "48 MB, hash checked"], connecting: ["done", "Vision Pro"], signing: ["active", "you@example.com"] }
      : { release: ["done", "Version 0.2.0, signed by ixion@YubiKey5-gpg"], download: ["done", "48 MB, hash checked"], connecting: ["done", "Vision Pro"], signing: ["failed", "you@example.com"] };
    for (const [name, [value, detail]] of Object.entries(marks)) {
      const li = panel.querySelector(`[data-stage="${name}"]`);
      li.dataset.state = value;
      li.querySelector(".stage-detail").textContent = detail;
    }
    if (step === "slots") {
      setStatus($(panel, "status"), "The Apple ID has no room for another development certificate.", "error");
      showSlots($(panel, "slots"), "certificates", () => {}, {
        certificates: [
          { name: "Apple Development: you@example.com", platform: "ios", machineName: "Apparition", expires: "2027-10-01T00:00:00Z", madeByApparition: true, serialNumber: "1" },
          { name: "Apple Development: you@example.com", platform: "macos", machineName: "Your MacBook", expires: "2027-06-11T00:00:00Z", serialNumber: "2" },
        ],
      });
    }
  }
  if (step === "pin") openPrompt({ kind: "pin" });
  if (step === "twofactor") openPrompt({ kind: "twoFactor", numbers: [{ id: 1, number: "+•• ••• •• 42" }] });
}

start();
