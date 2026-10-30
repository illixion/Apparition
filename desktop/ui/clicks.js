// @ts-check

// Every button acts once per deliberate press. A mouse double-click is the common accident:
// its second click either repeats the action or lands on whatever control the first one put
// under the pointer (the next step's button). Browsers number the clicks of a multi-click in
// `detail`, so anything past the first is dropped before any handler runs. A held Enter or
// Space key repeats the same way.

const CONTROLS = [
  "button", "a[href]", "summary", "label",
  'input[type="checkbox"]', 'input[type="radio"]', 'input[type="submit"]', 'input[type="button"]',
  '[role="button"]', '[role="tab"]', '[role="switch"]',
].join(", ");

/** @param {any} target */
const control = target => target?.closest?.(CONTROLS);

/** @param {any} event */
function drop(event) {
  event.preventDefault();
  event.stopImmediatePropagation();
}

/** Installs the guard on `root` (the window), in the capture phase so it runs first. */
export function guardRepeatedActivation(/** @type {any} */ root) {
  root.addEventListener("click", (/** @type {any} */ event) => {
    if (event.detail > 1 && control(event.target)) drop(event);
  }, true);
  root.addEventListener("keydown", (/** @type {any} */ event) => {
    if (!event.repeat) return;
    const target = event.target;
    // Enter submits from a form's text field too; Space only presses a control.
    const submits = event.key === "Enter" && target?.closest?.("form") && target.tagName !== "TEXTAREA";
    if ((event.key === "Enter" || event.key === " ") && control(target) || submits) drop(event);
  }, true);
}
