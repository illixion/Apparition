// @ts-check

/** Persistent across tabs; update state belongs to Rust, including when started from the tray. */
export class UpdateBanner {
  constructor(/** @type {HTMLElement} */ root, /** @type {any} */ invoke, /** @type {any} */ busy) {
    this.root = root;
    this.invoke = invoke;
    this.busy = busy;
    this.text = /** @type {HTMLElement} */ (root.querySelector("#update-text"));
    this.button = /** @type {HTMLButtonElement} */ (root.querySelector("#update-open"));
    /** @type {any} */
    this.status = {};
    this.button.addEventListener("click", () => this.activate());
  }

  draw(/** @type {any} */ status) {
    this.status = status;
    this.root.hidden = !status.version && !["error", "current"].includes(status.phase);
    const working = ["downloading", "installing"].includes(status.phase);
    this.text.textContent = [
      status.version ? `Apparition ${status.version} is available.` : "",
      status.message,
    ].filter(Boolean).join(" ");
    this.button.disabled = working;
    this.button.textContent = working ? "Updating..." : !status.version ? status.phase === "current" ? "Check again" : "Retry check"
      : status.canInstall ? "Update & restart" : "Downloads";
  }

  async activate() {
    if (this.button.disabled) return;
    if (!this.status.version) {
      try {
        await this.invoke("check_update");
      } catch {
        // The backend broadcasts the check failure and keeps the retry button available.
      }
      return;
    }

    if (!this.status.canInstall) {
      await this.invoke("open_downloads");
      return;
    }
    if (this.busy()) {
      this.text.textContent = "Finish the current operation before updating. Updating restarts Apparition and pauses the companion.";
      return;
    }
    this.button.disabled = true;
    try {
      await this.invoke("install_update");
    } catch (error) {
      // The backend also broadcasts failures, so they persist on a reopened window.
      this.draw({ ...this.status, phase: "error", message: error?.message ?? String(error) });
    }
  }
}

/** Consent is stored by Rust so tray-only launches obey it too. */
export class UpdatePreferences {
  constructor(/** @type {Document} */ root, /** @type {HTMLDialogElement} */ dialog, /** @type {any} */ invoke) {
    this.root = root;
    this.dialog = dialog;
    this.invoke = invoke;
    this.on = false;
    this.saving = false;
    this.error = "";
    this.checking = false;
    this.ready = invoke("auto_updates").then(on => this.draw(on)).catch(error => {
      this.error = error?.message ?? String(error);
      this.draw(false);
    });
    root.addEventListener("change", (/** @type {any} */ event) => {
      if (event.target.matches('[data-role="auto-updates"]')) this.save(event.target.checked);
    });
    dialog.querySelector('[data-role="check-once"]')?.addEventListener("click", () => this.checkOnce());
    dialog.querySelector('[data-role="skip-check"]')?.addEventListener("click", () => {
      if (!this.checking) dialog.close();
    });
    dialog.addEventListener("cancel", event => {
      if (this.checking) event.preventDefault();
    });
  }

  draw(/** @type {boolean} */ on = this.on) {
    this.on = on;
    this.root.querySelectorAll('[data-role="auto-updates"]').forEach((/** @type {HTMLInputElement} */ input) => {
      input.checked = on;
      input.disabled = this.saving;
    });
    this.root.querySelectorAll('[data-role="update-preferences-status"]').forEach((/** @type {HTMLElement} */ status) => {
      status.textContent = this.error;
      status.hidden = !this.error;
    });
  }

  async save(/** @type {boolean} */ on) {
    if (this.saving) return;
    this.saving = true;
    this.error = "";
    this.draw();
    try {
      this.on = await this.invoke("set_auto_updates", { on });
    } catch (error) {
      this.error = error?.message ?? String(error);
    } finally {
      this.saving = false;
      this.draw();
    }
  }

  async beforeStart() {
    await this.ready;
    if (this.on) return;
    const status = /** @type {HTMLElement} */ (this.dialog.querySelector('[data-role="check-status"]'));
    status.hidden = true;
    await new Promise(resolve => {
      this.dialog.addEventListener("close", resolve, { once: true });
      this.dialog.showModal();
    });
  }

  async checkOnce() {
    if (this.checking) return;
    this.checking = true;
    const buttons = this.dialog.querySelectorAll("button");
    buttons.forEach(button => button.disabled = true);
    const status = /** @type {HTMLElement} */ (this.dialog.querySelector('[data-role="check-status"]'));
    status.hidden = false;
    status.textContent = "Checking for a signed desktop update...";
    try {
      await this.invoke("check_update");
      this.dialog.close();
    } catch (error) {
      status.textContent = error?.message ?? String(error);
    } finally {
      this.checking = false;
      buttons.forEach(button => button.disabled = false);
    }
  }
}
