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
      await this.invoke("check_update");
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
