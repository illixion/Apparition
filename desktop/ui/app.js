// @ts-check
// Apparition's page: the install wizard and the Companion view. Every action is a Rust command
// (see src/flow.rs and src/companion.rs); this page only shows steps and collects answers. Passwords are read from their field, sent with the
// command and cleared from the field at once; nothing here stores them.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const STEPS = ["welcome", "headset", "pair", "signing", "install", "done"];
const DEFAULT_ANISETTE = "http://localhost:6969";

const state = {
  step: "welcome",
  /** @type {any} */ info: {},
  /** @type {any[]} */ paired: [],
  /** @type {any[]} */ found: [],
  /** iPhones and iPads usbmuxd sees. @type {any[]} */ usb: [],
  /** @type {any} */ selected: null,
  /** The headset installed to: {udid, name, address, osVersion?}. @type {any} */ device: null,
  pairSkipped: false,
  mode: "appleId",
  /** @type {any} */ signing: null,
  /** @type {string|null} */ p12: null,
  /** @type {string[]} */ profiles: [],
  what: "latest",
  /** @type {string|null} */ localPath: null,
  /** What the backend has prepared to install: "latest" or a local path. @type {string|null} */ prepared: null,
  /** @type {any} */ release: null,
  /** @type {any} */ result: null,
  keepExtensions: false,
  handOff: true,
  /** @type {any} */ settings: {},
  builtinAnisette: false,
  busy: false,
};

const panel = /** @type {HTMLElement} */ (document.getElementById("panel"));

// ------------------------------------------------------------ helpers

/** @param {ParentNode} root @param {string} role @returns {any} */
const $ = (root, role) => root.querySelector(`[data-role="${role}"]`);

/** @param {string} id @returns {DocumentFragment} */
const template = (id) => /** @type {HTMLTemplateElement} */ (document.getElementById(id)).content.cloneNode(true);

/** The message of a failed command: a Failure ({message, slot, cancelled}) or a string. */
function messageOf(/** @type {any} */ error) {
  if (typeof error === "string") return error;
  return error?.message ?? String(error);
}

/** @param {HTMLElement} el @param {string} text @param {"error"|"good"|""} [kind] */
function setStatus(el, text, kind = "") {
  el.hidden = !text;
  el.textContent = text;
  el.className = `status ${kind}`.trim();
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

function go(/** @type {string} */ step) {
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
  panel.querySelector('[data-go="headset"]')?.addEventListener("click", () => go("headset"));
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
    hint.textContent = `Forgot ${device.name}. On a headset, also remove this computer from Remote Devices.`;
  });
}

// ------------------------------------------------------------ 3. pair

function pair() {
  panel.append(template("t-pair"));
  const target = state.selected;
  $(panel, "name").textContent = target?.name ?? "the headset";
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
      "If it asks whether to trust this computer, tap Trust and enter its passcode.",
      "Keep it on the same Wi-Fi as this computer, then select Start pairing. No code is needed.",
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
    setStatus(status, usb ? "Pairing over the cable, then looking for it on Wi-Fi…" : "Waiting for the headset to show its code…");
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
      setBusy(start, false);
      back.disabled = false;
    }
  };
}

function pairingError(/** @type {any} */ error) {
  const message = messageOf(error);
  if (error?.cancelled) return message;
  if (/srp|proof/i.test(message)) return `The code didn't match. Check the code the headset shows, and try again.\n\n${message}`;
  if (/trust|usbmuxd|lockdown|isn't connected/i.test(message)) return message;
  if (/refused|reset|unreachable|timed out|reach/i.test(message))
    return `Couldn't reach the headset. Keep Remote Devices open on it (the pairing port changes when it reopens), and check both are on the same network.\n\n${message}`;
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
  const continueButton = /** @type {HTMLButtonElement} */ ($(panel, "continue"));
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
              ? `doesn't list ${state.device?.name ?? "this headset"}`
              : `lists ${state.device?.name ?? "this headset"}`;
        add(`${profile.name}: ${why}`, !fits);
      }
      if (!usable) add("None of these profiles can sign for this headset. Add one that lists it.", true);
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
  appleForm.anisette.value = savedAnisette && savedAnisette !== "builtin" ? savedAnisette : DEFAULT_ANISETTE;
  const sourceField = /** @type {HTMLElement} */ ($(appleForm, "anisette-source"));
  const sourceSelect = /** @type {HTMLSelectElement} */ (appleForm.elements.namedItem("source"));
  // Only offered where this computer can make its own; elsewhere a server is the one way.
  // Nothing saved comes back as null, so `!= null` (not `!== undefined`).
  sourceSelect.value = state.builtinAnisette && savedAnisette != null && savedAnisette !== "builtin" ? "server" : state.builtinAnisette ? "builtin" : "server";
  sourceField.hidden = !state.builtinAnisette;
  const drawSource = () => {
    const builtin = sourceSelect.value === "builtin";
    for (const role of ["anisette-builtin-hint"]) $(appleForm, role).hidden = !builtin;
    for (const role of ["anisette-server", "anisette-server-hint"]) $(appleForm, role).hidden = builtin;
    appleForm.anisette.required = !builtin;
  };
  sourceSelect.addEventListener("change", drawSource);
  drawSource();
  appleForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    const submit = /** @type {HTMLButtonElement} */ (appleForm.querySelector('[type="submit"]'));
    const args = { email: appleForm.email.value, password: appleForm.password.value, anisette: sourceSelect.value === "builtin" ? "builtin" : appleForm.anisette.value };
    appleForm.password.value = "";
    setBusy(submit, true);
    state.busy = true;
    draw();
    setStatus(status, "Signing in to Apple…");
    try {
      const result = await invoke("apple_id_sign_in", args);
      state.signing = { mode: "appleId", ...result };
      state.settings = { email: result.email, anisette: args.anisette.trim() };
      setStatus(status, "");
    } catch (error) {
      setStatus(status, signInError(error, args.anisette !== "builtin"), error?.cancelled ? "" : "error");
    } finally {
      args.password = "";
      setBusy(submit, false);
      state.busy = false;
      draw();
    }
  });

  // Another device's certificate key, before signing in
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
    if (!state.profiles.length) return setStatus(status, "Add at least one provisioning profile that lists the headset.", "error");
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

  draw();
}

/** Whether a profile's Platform list covers the device's platform. */
function platformMatches(/** @type {string[]|undefined} */ platforms) {
  const names = { visionOS: ["xrOS", "visionOS"], iOS: ["iOS"], tvOS: ["tvOS"] }[state.device?.platform ?? "visionOS"] ?? [];
  return (platforms ?? []).some((p) => names.includes(p));
}

function signInError(/** @type {any} */ error, /** @type {boolean} */ usingServer = true) {
  const message = messageOf(error);
  if (error?.cancelled) return message;
  // core's REFUSED_ANISETTE already explains it.
  if (/-80045/.test(message)) return message;
  if (!usingServer && /anisette|connect|dns|resolve|refused/i.test(message))
    return `Couldn't sign in. Check the internet connection (Apparition fetches Apple's anisette libraries on first use).\n\n${message}`;
  if (usingServer && (/anisette/i.test(message) || /connect|dns|resolve|refused/i.test(message)))
    return `Couldn't sign in. Check that the anisette server is running and its address is right.\n\n${message}`;
  return message;
}

// ------------------------------------------------------------ 5. install

/** Install stages, in order. */
const STAGES = ["release", "download", "connecting", "signing", "installing", "handoff"];

function install() {
  panel.append(template("t-install"));
  const start = /** @type {HTMLButtonElement} */ ($(panel, "start"));
  const back = /** @type {HTMLButtonElement} */ ($(panel, "back"));
  const status = $(panel, "status");
  const stages = $(panel, "stages");
  const slots = $(panel, "slots");
  const advanced = $(panel, "advanced");
  const what = /** @type {NodeListOf<HTMLInputElement>} */ (panel.querySelectorAll('input[name="what"]'));
  const handoff = /** @type {HTMLInputElement} */ ($(panel, "handoff"));
  const extensions = /** @type {HTMLInputElement} */ ($(panel, "extensions"));

  back.addEventListener("click", () => go("signing"));
  handoff.checked = state.handOff;
  extensions.checked = state.keepExtensions;
  handoff.addEventListener("change", () => (state.handOff = handoff.checked));
  extensions.addEventListener("change", () => (state.keepExtensions = extensions.checked));

  const describe = () => {
    what.forEach((w) => (w.checked = w.value === state.what));
    const local = state.what === "local" && state.localPath;
    panel.querySelector("h1").textContent = local ? `Install ${basename(state.localPath ?? "")}` : "Install Apparition";
    $(panel, "lede").textContent = local
      ? `Signs this app for ${state.device?.name ?? "the headset"} and installs it.`
      : `The latest official release, checked against Apparition's release signature before it's used, signed for ${state.device?.name ?? "the headset"}.`;
    $(panel, "local-name").textContent = state.localPath ? basename(state.localPath) : "Any .ipa on this computer.";
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
          stage === "connecting" ? detail : stage === "signing" ? signingLabel() : stage === "installing" ? (detail.update ? "Updating" : "") : "";
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
          mark("download", "skipped", `${local.name}, ${megabytes(local.size)}`);
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
        options: { udid: state.device.udid, keepExtensions: state.keepExtensions, handOff: state.handOff },
      });
      for (const s of ["connecting", "signing", "installing"]) mark(s, "done");
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
      if (error?.slot && state.signing?.mode === "appleId") showSlots(slots, error.slot, run);
      start.textContent = "Try again";
      advanced.hidden = false;
    } finally {
      cleanup();
      setBusy(start, false);
      back.disabled = false;
    }
  };
  start.addEventListener("click", run);
}

function installError(/** @type {any} */ error) {
  const message = messageOf(error);
  if (error?.slot === "certificates") return `The Apple ID has no room for another development certificate.\n\n${message}`;
  if (error?.slot === "appIds") return `The Apple ID can't make another App ID right now.\n\n${message}`;
  if (/reach|refused|reset|timed out|unreachable/i.test(message))
    return `Couldn't reach ${state.device?.name ?? "the device"}. Check that it's awake and on the same network as this computer.\n\n${message}`;
  return message;
}

/**
 * Lists what fills the slot and frees one the user picks, only after they type the word.
 * @param {HTMLElement} box @param {"certificates"|"appIds"} slot @param {() => void} retry
 * @param {any} [sample] a listing to show instead of asking the account (debug previews)
 */
async function showSlots(box, slot, retry, sample) {
  const certificates = slot === "certificates";
  const word = certificates ? "revoke" : "delete";
  box.replaceChildren(template("t-slots"));
  box.hidden = false;
  $(box, "title").textContent = certificates ? "Free a certificate slot" : "Free an App ID";
  $(box, "word").textContent = word;
  const button = /** @type {HTMLButtonElement} */ ($(box, "do"));
  button.textContent = certificates ? "Revoke certificate" : "Delete App ID";
  const typed = /** @type {HTMLInputElement} */ ($(box, "typed"));
  const confirm = $(box, "confirm");
  const list = $(box, "list");
  const explain = $(box, "explain");
  explain.textContent = "Looking at the account…";
  /** @type {any} */ let chosen = null;

  let items = [];
  try {
    if (certificates) {
      items = (sample ?? (await invoke("list_certificates"))).certificates ?? [];
      explain.textContent =
        "A free Apple ID has room for two development certificates. Revoke one you no longer need, then Apparition installs again. Nothing is revoked until you confirm.";
    } else {
      const ids = sample ?? (await invoke("list_app_ids"));
      items = ids.appIds ?? [];
      const left = ids.availableQuantity ?? "?";
      explain.textContent = `A free Apple ID can make ten App IDs a week (${left} more allowed now). Deleting one may help; nothing is deleted until you confirm.`;
    }
  } catch (error) {
    explain.textContent = `Couldn't list them: ${messageOf(error)}`;
    return;
  }
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
            ? "Apps that device signed with it stop opening until it signs them again (it makes a new certificate then, which needs a free slot)."
            : "Apps signed with it (by Xcode or another tool) stop opening until signed again; that tool makes a new one when it next signs."
        : "Apps using it keep working until their profile expires. A free account may refuse until the ID expires, and a deleted ID can still count against this week's ten.";
      typed.focus();
    });
    list.append(row);
  }
  box.scrollIntoView({ block: "nearest", behavior: "smooth" });

  typed.addEventListener("input", () => (button.disabled = typed.value.trim() !== word));
  $(box, "keep").addEventListener("click", () => (box.hidden = true));
  button.addEventListener("click", async () => {
    if (!chosen || typed.value.trim() !== word) return;
    setBusy(button, true);
    try {
      if (certificates) await invoke("revoke_certificate", { serial: chosen.serialNumber, confirmation: typed.value });
      else await invoke("delete_app_id", { appIdId: chosen.appIdId, confirmation: typed.value });
      box.hidden = true;
      retry();
    } catch (error) {
      $(box, "warning").textContent = messageOf(error);
      setBusy(button, false);
    }
  });
}

// ------------------------------------------------------------ 6. done

function done() {
  panel.append(template("t-done"));
  const result = state.result ?? {};
  const isApparition = (result.bundleId ?? "").startsWith("com.illixion.apparition");
  const name = isApparition ? "Apparition" : result.label ?? result.bundleId;
  $(panel, "title").textContent = `${name} is ${result.update ? "updated" : "on"} ${result.deviceName ?? "your device"}.`;
  $(panel, "lede").textContent = result.queued
    ? `Sent in ${result.seconds?.toFixed(1)} s. It's an update of an app that's open on the headset: close it there to finish.`
    : `Installed in ${result.seconds?.toFixed(1)} s.`;

  const next = $(panel, "next");
  const add = (/** @type {string} */ text) => {
    const li = document.createElement("li");
    li.textContent = text;
    next.append(li);
  };
  add(`Open ${name} from the headset's apps.`);
  add(`If ${result.platform ?? "visionOS"} says the developer isn't trusted, trust it in Settings > General > VPN & Device Management.`);
  if (isApparition && result.handoff === "appleId")
    add(`Sign in to Apparition with ${result.appleId}. It finds the certificate this computer shared, so the two use one of the account's certificate slots.`);
  if (isApparition && result.handoff === "certificate")
    add("In Apparition, enter the .p12's password once: the certificate and profiles are waiting in its inbox.");
  if (isApparition && result.pairingHandedOver)
    add(`Apparition got its own pairing with this ${result.platform === "iOS" ? "device" : "headset"}, so it can sign and install there.`);
  if (isApparition && state.signing?.mode === "appleId")
    add("Apps signed with a free Apple ID last 7 days. A daily Shortcuts automation with Apparition's Refresh Apps action keeps them signed.");

  const extra = $(panel, "extra");
  const notes = [];
  if (result.handoffError) notes.push(`Couldn't share the signing with Apparition (${result.handoffError}). Sign in there yourself instead.`);
  if (result.strippedExtensions?.length) notes.push(`Left out extensions no profile covers: ${result.strippedExtensions.join(", ")}.`);
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

/** @param {any} prompt */
function openPrompt(prompt) {
  promptKind = prompt.kind;
  promptCode.value = "";
  promptSms.replaceChildren();
  promptSms.hidden = true;
  promptError.hidden = true;
  const title = /** @type {HTMLElement} */ (document.getElementById("prompt-title"));
  const text = /** @type {HTMLElement} */ (document.getElementById("prompt-text"));
  if (prompt.kind === "pin") {
    title.textContent = prompt.again ? "The headset wants to pair again" : "Type the code from the headset";
    text.textContent = prompt.again
      ? "Open Settings > General > Remote Devices on the headset and type the code it shows."
      : "Remote Devices on the headset now shows a code.";
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
  promptCode.focus();
}

function answer(/** @type {any} */ reply) {
  dialog.close();
  promptCode.value = "";
  invoke("answer_prompt", { answer: reply }).catch(() => {});
}

promptForm.addEventListener("submit", (event) => {
  event.preventDefault();
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
/** @type {number|undefined} */ let companionTimer;
/** What an action in progress, or one that just failed, has to say. @type {{text: string, kind?: "error"|"good"|""}|null} */
let companionNote = null;

function showView(/** @type {"install"|"companion"} */ view) {
  const companion = view === "companion";
  installView.hidden = companion;
  companionView.hidden = !companion;
  tabInstall.setAttribute("aria-selected", String(!companion));
  tabCompanion.setAttribute("aria-selected", String(companion));
  clearInterval(companionTimer);
  if (companion) {
    if (!companionPanel.firstElementChild) buildCompanion();
    refreshCompanion();
    companionTimer = setInterval(refreshCompanion, 2000);
    companionPanel.focus({ preventScroll: true });
  }
}
tabInstall.addEventListener("click", () => showView("install"));
tabCompanion.addEventListener("click", () => showView("companion"));

function buildCompanion() {
  companionPanel.append(template("t-companion"));
  const where = state.info.platform === "macos-universal" ? "menu bar" : "tray";
  companionPanel.querySelectorAll('[data-role="where"]').forEach((el) => (el.textContent = where));
  $(companionPanel, "windows-firewall").hidden = !state.info.platform.startsWith("windows");
  const relayOn = /** @type {HTMLInputElement} */ ($(companionPanel, "relay-on"));
  relayOn.addEventListener("change", () => companionCall("companion_set_relay", { on: relayOn.checked }));
  companionPanel.querySelectorAll('input[name="anisette-mode"]').forEach((input) =>
    input.addEventListener("change", () => {
      const mode = /** @type {HTMLInputElement} */ (input).value;
      const note = mode === "builtin" ? "Starting this computer's anisette (the first time, it fetches Apple's libraries)…" : "";
      companionCall("companion_set_anisette", { mode }, note);
    }),
  );
  for (const [role, up] of [["docker-up", true], ["docker-down", false]]) {
    const button = $(companionPanel, role);
    button.addEventListener("click", async () => {
      setBusy(button, true);
      await companionCall("companion_docker", { up }, up ? "Starting the Docker server…" : "Stopping the Docker server…");
      setBusy(button, false);
    });
  }
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
    drawCompanion(await invoke("companion_status"));
  } catch {}
}

const amount = (/** @type {number} */ bytes) => (bytes < 1000 ? `${bytes} B` : megabytes(bytes));
const clock = (/** @type {number} */ seconds) => new Date(seconds * 1000).toLocaleTimeString(undefined, { timeStyle: "medium" });

function drawCompanion(/** @type {any} */ status) {
  /** @type {HTMLElement} */ (document.getElementById("companion-dot")).hidden = !status.relay.running;
  if (!companionPanel.firstElementChild) return;
  const relayOn = /** @type {HTMLInputElement} */ ($(companionPanel, "relay-on"));
  relayOn.checked = status.relay.on;
  $(companionPanel, "relay-port").textContent = String(status.relay.port);
  $(companionPanel, "computer").textContent = status.computerName || "this computer";
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

  const anisette = status.anisette;
  $(companionPanel, "mode-builtin").hidden = !anisette.builtinAvailable;
  companionPanel.querySelectorAll('[data-role="anisette-port"]').forEach((el) => (el.textContent = String(anisette.port)));
  companionPanel.querySelectorAll('input[name="anisette-mode"]').forEach((el) => {
    const input = /** @type {HTMLInputElement} */ (el);
    input.checked = input.value === anisette.mode;
    // Offering anisette goes through the relay's advertisement, so it needs the relay.
    input.disabled = !status.relay.on;
  });
  $(companionPanel, "docker-row").hidden = anisette.mode !== "local";
  const text = companionNote?.text
    ?? anisette.error
    ?? (!status.relay.on
      ? "Turn the relay on to offer anisette."
      : anisette.offered === "builtin"
        ? anisette.native
          ? `Offering this Mac's own anisette on port ${anisette.port}.`
          : `Offering anisette from Apple's Android libraries on port ${anisette.port}.`
        : anisette.offered === "local"
          ? `Offering the server on port ${anisette.port}.`
          : anisette.mode === "local"
            ? `Nothing answers on port ${anisette.port} yet. Start the Docker server, or run your own there.`
            : "");
  const kind = companionNote?.kind ?? (anisette.error ? "error" : anisette.offered ? "good" : "");
  setStatus($(companionPanel, "anisette-status"), text, kind);
}

// ------------------------------------------------------------ start

async function start() {
  await listen("prompt", (/** @type {any} */ e) => openPrompt(e.payload));
  await listen("prompt-done", () => dialog.open && dialog.close());
  state.info = await invoke("app_info");
  document.body.classList.toggle("mac", state.info.platform === "macos-universal");
  state.settings = await invoke("saved_settings");
  state.builtinAnisette = await invoke("builtin_anisette");

  invoke("check_update")
    .then((/** @type {any} */ update) => {
      if (!update) return;
      const box = /** @type {HTMLElement} */ (document.getElementById("update"));
      /** @type {HTMLElement} */ (document.getElementById("update-text")).textContent = `Apparition ${update.version} is available.`;
      box.hidden = false;
      document.getElementById("update-open")?.addEventListener("click", () => invoke("open_downloads"));
    })
    .catch(() => {});

  refreshCompanion();
  if (state.info.preview === "companion") {
    render();
    return showView("companion");
  }
  if (state.info.preview) return preview(state.info.preview);
  render();
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
