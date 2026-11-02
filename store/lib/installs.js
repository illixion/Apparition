'use strict';
// An install the device made for itself (Apparition downloaded a kept build and signed it), told
// to the service so its install ledger, and every client reading it, stays truthful without the
// Mac ever reaching the device. The client names a configured target and a kept build; everything
// else in the entry (repo, branch, commit, version) is read from the service's own records, never
// taken from the request.

class ReportError extends Error {
  constructor(message, status = 400) { super(message); this.status = status; }
}
const text = (value, max) => typeof value === 'string' && value.length > 0 && value.length <= max && !/[\x00-\x1f\x7f]/.test(value);
const same = (a, b) => typeof a === 'string' && typeof b === 'string' && a.toLowerCase() === b.toLowerCase();

// report: {device, bundleId, buildId, platform?}. repos: the configured projects. builds: the
// service's jobs. readBuild(buildId) -> {manifest}, throwing when the build isn't kept.
function ledgerEntryFromReport(report, { repos, builds, readBuild, now = Date.now() }) {
  if (!report || typeof report !== 'object' || Array.isArray(report) ||
      Object.keys(report).some(k => !['device', 'bundleId', 'buildId', 'platform'].includes(k)) ||
      !text(report.device, 128) || !text(report.bundleId, 255) || !/^[A-Za-z0-9-]{1,64}$/.test(report.buildId || '') ||
      (report.platform !== undefined && !text(report.platform, 40))) {
    throw new ReportError('Name a configured device, its bundle identifier and a kept build.');
  }
  const found = repos.flatMap(repo => (repo.platforms || []).map(target => ({ repo, target })))
    .find(({ target }) => target.device === report.device && target.bundleId === report.bundleId &&
      (report.platform === undefined || same(target.platform, report.platform)));
  if (!found) throw new ReportError('That is not a configured target.');
  let kept;
  try { kept = readBuild(report.buildId); } catch { throw new ReportError('That build is not kept on the Mac.', 404); }
  const app = kept.manifest.app || {};
  if (app.bundleId !== found.target.bundleId && app.originalBundleId !== found.target.bundleId) {
    throw new ReportError('That build is not of this target.');
  }
  if (!same(kept.manifest.platform, found.target.platform)) throw new ReportError('That build is for another platform.');
  const job = builds.find(b => b.buildId === report.buildId && b.repo === found.repo.name);
  return {
    device: found.target.device,
    bundleId: found.target.bundleId,
    entry: {
      repo: found.repo.name, branch: job?.builtBranch ?? null, commit: job?.commit ?? null,
      buildId: report.buildId, platform: found.target.platform,
      version: app.version ?? null, buildNo: app.build ?? null, at: now,
      // The device itself did this; no guess, and no `devicectl` involved.
      confirmed: true, via: 'apparition',
    },
  };
}

module.exports = { ledgerEntryFromReport, ReportError };
