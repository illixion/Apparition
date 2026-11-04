# Developer UI

The desktop Developer tab and Apparition Store share the project list in
`service/public/shared/projects.js` and `projects.css`. Freshness comes from
`service/public/freshness.js`. Edit those sources for project cards, target
selection, branches, deploy flags, search, sorting, and request feedback.

`desktop/build.rs` copies these assets into the ignored `desktop/ui/shared/`
folder before Tauri embeds the frontend. They are bundled locally; no remote
scripts or service HTML are loaded into the desktop webview.

The `ProjectList` component takes a state snapshot and a `build(repo, targets,
flags)` callback. The Store callback uses its existing cookie API. The desktop
adapter maps each selected device/platform to the allowlisted Rust service
bridge, keeping credentials out of JavaScript. The component owns pending state,
duplicate click prevention, and inline errors for both hosts.

Desktop-only artifact, report, patch, and re-sign controls attach through the
`extend(card, repo)` callback. Mac zip downloads and OTA links retain the Store's
browser handlers. Connection setup, signing, Activity and Reports remain in their
respective shells; this extraction unifies the project workflow, not every page.

Run `node --test desktop/test/*.test.mjs` for native bridge/controller checks and
`node --test store/test/project-ui.test.js store/test/freshness.test.js` for the
shared request and freshness behavior. Check both shells in a browser after
changing shared layout or events.
