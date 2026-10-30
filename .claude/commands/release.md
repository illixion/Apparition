---
description: Release Apparition: bump the version, push, build on GitHub Actions, attest, publish and bless the release site
allowed-tools: Bash, Read, Edit, Write, AskUserQuestion
---

You are performing a release of Apparition. The user may give $ARGUMENTS: `major`, `minor` or `patch` (default `patch` when empty or unrecognised). Apparition versions are plain `major.minor.patch` (semver, no pre-release suffix). The release site is `~/Projects/apparition.illixion.com`; releases are signed by the YubiKey.

Follow these steps in order, stopping on any error. Steps marked **USER** need the person at the keyboard; stop there, say what to run, and wait.

## 0. Preconditions

- `gh auth status` must show the account **illixion**; if not, `gh auth switch --account illixion`.
- In `~/Projects/Apparition`: `git status --porcelain` must be empty, and `git fetch origin && git status -sb` must show `main` not behind `origin/main`.
- `git log origin/main..main --format='%h %G? %s'` lists unpushed commits. Every one must show `G` (signed). If any is unsigned, stop: this repo's pre-push hook rejects unsigned commits authored as illixion, and signing needs a touch.

## 1. Decide the version

Read the current `MARKETING_VERSION` in `project.yml`. Bump it per $ARGUMENTS. Check the README for a version the release already names (e.g. "from 0.3.3") and use that instead of guessing; if the README and the bump disagree, ask the user.

Show the user the commits since the last tag (`git log v<current>..HEAD --oneline`) and the proposed version. Ask them to confirm it before editing.

## 2. Bump the version

Decide first whether this is a **full** release (every part: a tag push, or `parts` all/empty) or a **partial** one (`parts` lists some parts, e.g. `app`, `macos`, `windows`, `linux`).

- **Full:** set the same version in all four places (match the 0.3.2 release commit, `5f8b2b8`):
  - `project.yml`: `MARKETING_VERSION: "x.y.z"`
  - `desktop/Cargo.toml` and `installer/Cargo.toml`: line 3, `version = "x.y.z"`
  - Refresh the lock files so their own entries match: `(cd desktop && cargo metadata --format-version 1 >/dev/null)` and the same in `installer/`. Only the `apparition-desktop` and `apparition-installer` entries should change in the lock diff.

  Check with `git diff`: exactly five lines change.
- **Partial:** `project.yml` is the only version that must match. `scripts/package-release.sh` checks the Cargo files only when it builds installers, so a `parts=app` release needs just the `MARKETING_VERSION` bump. A partial release that includes any installer part (`macos`, `windows`, `linux`) needs all four places, same as a full one.

Also grep for the old version to catch any other mention, and update docs that say "from <version>" only if they refer to the new release.

## 3. Commit and push the release

- `git add project.yml desktop/Cargo.toml desktop/Cargo.lock installer/Cargo.toml installer/Cargo.lock`
- Before committing, check the Rust crates build: `(cd installer && cargo check)` and `(cd desktop && cargo check)`. Both must finish with no errors. A broken build here would otherwise only show up after the tag has started the full release, which is how 0.3.3 failed on its first run.
- Commit **signed** in the foreground: `git commit -S`. Subject `Release x.y.z`. The body summarises everything since the last release in plain prose (not a raw list of commit subjects), followed by the `Co-Authored-By` trailer for the model doing the release, as the session's attribution instructions give it (it names the model actually running; never copy a model name from an earlier commit or from this file).
- If signing fails with "agent refused operation", stop. **USER:** run `git commit -S` themselves with the `!` prefix (touch the YubiKey), then continue.
- `git push origin main`.

## 4. Tag and start the build

- **Full release:** `git tag -s vx.y.z -m "Apparition x.y.z"` then `git push origin vx.y.z`. The tag starts the full build (`.github/workflows/release.yml`). Do not also dispatch it.
- **Partial release:** do **not** push a tag. A tag push always builds every part, so the tag would start a full build. Dispatch by hand instead: `gh workflow run release.yml -R illixion/Apparition -f version=x.y.z -f parts=<list>`, where `<list>` is a comma-separated subset of `app,macos,windows,linux`. The version must already be committed (see step 2 for which files). Step 5 creates the release with `--target` and makes the tag then.
- If a tag was pushed by mistake, cancel the duplicate full run by its exact id (`gh run list --workflow release.yml`, then `gh run cancel <id>`).
- Get the run id with `gh run list --workflow release.yml -R illixion/Apparition --limit 3`.

Wait for it in the background (`gh run watch <id> -R illixion/Apparition --exit-status --interval 60`, run_in_background). It takes about 45 minutes. Do not poll. If it fails, read the failing job's log (`gh run view <id> --log-failed`), report it, and stop. Do not publish a partial release.

## 5. Check the build and create the draft release

The release stays a **draft** until it is blessed: a draft is private, and (with immutable releases on) a published release's files and tag can never change, so nothing may go public before every file, including the signature, is on it.

- The artifact name depends on how the run started: `release-vx.y.z` for a tag push (the version carries its `v`), but `release-x.y.z` for a hand-dispatched run (the workflow's `VERSION` is the `version` input as typed, no `v`). Check with `gh run view <id> -R illixion/Apparition --json artifacts` or the run page, then `gh run download <id> -R illixion/Apparition -n <name> -D build/release/x.y.z`.
- A partial run uploads only the files of the parts it built. That is expected; check the set against the parts you asked for.
- `for f in build/release/x.y.z/Apparition-*; do gh attestation verify "$f" -R illixion/Apparition; done`. Every file must verify.
- Draft the release notes from the commit summary and show them to the user for approval.
- Create the draft. Full release, tag pushed: `gh release create vx.y.z build/release/x.y.z/Apparition-* -R illixion/Apparition --draft --verify-tag --title "Apparition x.y.z" --notes "<approved notes>"`. Partial release, or any run without a tag: the same command with `--target <full sha of the built commit>` in place of `--verify-tag`.
- Download the draft's assets back (`gh release download vx.y.z -R illixion/Apparition -D <empty dir>`, which works for a draft) and check each file's `sha256sum` against the build's. Once published the files can't be replaced, so a wrong upload is found here or not at all.

## 6. Bless, then publish

In `~/Projects/apparition.illixion.com` (check `git status` is clean and `git pull` is current):

- **Installers-only partial release** (no `app` part): `scripts/publish.py add` refuses a folder with no IPA (`no Apparition-<version>-<platform>.ipa`). Stop and ask the user before going further; the site script needs a change first.
- Run the script with `python3`, not directly: `scripts/publish.py` is committed without its executable bit, so `scripts/publish.py` fails with "Permission denied". `python3 scripts/publish.py add x.y.z ../Apparition/build/release/x.y.z --url-base https://github.com/illixion/Apparition/releases/download/vx.y.z --notes "<approved notes>"`. This hashes the files, then calls `scripts/bless.sh`, which signs `SHA256SUMS` with the YubiKey.
- The YubiKey needs a PIN and a touch, and runs in the foreground with `--gpg-agent` (the plain agent path is refused for the YubiKey). If it does not respond (no prompt, timeout, or agent error), stop and pause: tell the user to touch the YubiKey or run the command with the `!` prefix, then wait for their reply before retrying. Do not fall back to any other key.
- Add the signed manifest to the draft: `gh release upload vx.y.z Website/releases/x.y.z/SHA256SUMS Website/releases/x.y.z/SHA256SUMS.sig -R illixion/Apparition` (run from the site repo). Without `--clobber`: an existing file with that name is an error to look at.
- **Immutable releases.** Check `gh api repos/illixion/Apparition/immutable-releases`. If `enabled` is false and the user wants it on, **USER:** turn it on (Settings > General > Releases, or ask) before the next step, since it only applies to releases published afterwards. Do not turn it on yourself.
- Publish: `gh release edit vx.y.z -R illixion/Apparition --draft=false`. From here nothing on the release can change, so stop and ask the user first if anything above was off.
- Then `python3 scripts/publish.py verify x.y.z --download`, from the site repo. Every URL must serve its signed file; it can only work now, because a draft's URLs aren't public.
- `git add -A && git commit -m "List Apparition x.y.z"`, signed, then `git push`. Updating the website is automatic: the push deploys it, so do not ask first.

## 7. Report

Give the user: the release URL, the site URL, the release commit SHA and branch, and anything left open. Remind them: the Apparition repo is not pushed beyond `main` plus the tag, and the `apparition.illixion.com` push deploys the site.
