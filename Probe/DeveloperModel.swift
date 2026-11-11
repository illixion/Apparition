import DebugTrace
import Foundation
import Observation

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Developer")

/// What a build is asked to do, besides where it goes.
struct BuildOptions: Equatable, Sendable {
    var branch = "current"
    var launch = false
    var debug = true
    var patch = false
    var injectBackend = "dynamic"
}

/// The Developer feature as the rest of the app sees it: the build service, which project each app
/// on this device belongs to, and installing a project's build here.
///
/// Installing here is the default route: the Mac builds and keeps the IPA, Apparition downloads it
/// over the service's HTTPS address, signs it with this device's own signer and installs it. That
/// needs nothing from the Mac's network (so it works over Tailscale) and works with a free Apple ID.
/// The Mac's Xcode tunnel remains a secondary route (`InstallRoute`).
@MainActor @Observable
final class DeveloperModel {
    /// Where one target's install has got to.
    enum Phase: Equatable {
        case building(job: Int)
        case downloading
        case installing
        case done(String)
        /// `keptBuild` is a build the Mac made but could not install through the tunnel: this device
        /// can still install it.
        case failed(String, keptBuild: String?)

        var isWorking: Bool {
            switch self {
            case .building, .downloading, .installing: true
            case .done, .failed: false
            }
        }
    }

    let service: BuildService
    private let library: LibraryModel
    private let probe: ProbeModel
    private(set) var links: ProjectLinks
    private(set) var phases: [String: Phase] = [:]

    private static var linksURL: URL { AccountModel.dataDir.appendingPathComponent("project-links.json") }

    init(service: BuildService, library: LibraryModel, probe: ProbeModel) {
        self.service = service
        self.library = library
        self.probe = probe
        links = (try? JSONDecoder().decode(ProjectLinks.self, from: Data(contentsOf: Self.linksURL))) ?? ProjectLinks()
    }

    /// While Apparition is in front and connected: asks the service for its state every few seconds.
    /// Every request names this device, which is how the service knows Apparition is open here (a
    /// build asked for `install: auto` is then left for this device to install, and goes through the
    /// Xcode tunnel only when Apparition is closed). Returns when Apparition leaves the front.
    func keepPresent(while active: @MainActor () -> Bool) async {
        await service.restoreConnection()
        // The service may have been restarted (as a newer version) while Apparition was away.
        await service.refreshCapabilities()
        while service.connected, active(), !Task.isCancelled {
            await service.refreshState()
            try? await Task.sleep(for: .seconds(8))
        }
    }

    static func key(_ project: BuildService.Project, _ target: BuildService.Target) -> String {
        "\(project.name)|\(target.device)|\(target.platform)"
    }

    func phase(_ project: BuildService.Project, _ target: BuildService.Target) -> Phase? { phases[Self.key(project, target)] }

    /// Whether anything is being installed from a project right now.
    var isWorking: Bool { phases.values.contains { $0.isWorking } }

    func dismissPhase(_ project: BuildService.Project, _ target: BuildService.Target) {
        if phases[Self.key(project, target)]?.isWorking == false { phases[Self.key(project, target)] = nil }
    }

    // MARK: Which project an app belongs to

    /// The project target an app on the device belongs to (a person's link, else a match).
    func project(for app: ProbeModel.DeviceApp) -> (project: BuildService.Project, target: BuildService.Target, link: ProjectLink?)? {
        let managed = library.installed.first { $0.installedBundleId == app.bundleId }
        let preferred = UserDefaults.standard.string(forKey: "pairedName")
        return service.projectFor(bundleId: app.bundleId, deviceFamily: app.deviceFamily, preferredDevice: preferred, links: links)
            ?? managed.flatMap { service.projectFor(bundleId: $0.id, deviceFamily: app.deviceFamily, preferredDevice: preferred, links: links) }
    }

    /// Records that an app on the device is a project's target, whatever its identifier says.
    func link(_ bundleId: String, to project: BuildService.Project, target: BuildService.Target) {
        links.set(ProjectLink(project: project.name, device: target.device, platform: target.platform, chosen: true), for: bundleId)
        saveLinks()
    }

    func unlink(_ bundleId: String) {
        links.unlink(bundleId)
        saveLinks()
    }

    private func saveLinks() {
        do { try JSONEncoder().encode(links).write(to: Self.linksURL, options: .atomic) }
        catch { log.error("could not save project links: \(String(describing: error), privacy: .public)") }
    }

    // MARK: Which route installs a target

    /// This device is the target's: the name the Mac knows it by is the name Apparition paired with
    /// (or, with no pairing name, the target's platform is this device's).
    func isThisDevice(_ target: BuildService.Target) -> Bool {
        if let paired = UserDefaults.standard.string(forKey: "pairedName"), !paired.isEmpty {
            return target.device.caseInsensitiveCompare(paired) == .orderedSame
        }
        return ThisDevice.preferredBuildPlatforms(deviceFamily: nil).contains(target.platform.lowercased())
    }

    func installOptions(_ target: BuildService.Target) -> InstallOptions {
        let device = service.device(named: target.device)
        let listed = service.state?.devices != nil
        return InstallOptions(.init(
            targetIsThisDevice: isThisDevice(target),
            serviceInstallsOnDevice: service.installsOnDevice,
            canSignHere: library.signerOptions.contains { $0.unusable == nil },
            canReachInstaller: probe.hasPairingFile && probe.routeChosen,
            macSeesDevice: device.map(\.reachable),
            // A service that lists no devices says nothing about the Mac's pairing.
            macKnowsDevice: !listed || device != nil,
            deviceName: target.device))
    }

    // MARK: Building and installing

    /// Builds a target and gets the result onto its device by `route`. With `.thisDevice` the Mac only
    /// builds and keeps the IPA, which this device then downloads and installs; with `.xcodeTunnel`
    /// the Mac installs it itself. Waits for the end, so `phase` follows it.
    func buildAndInstall(project: BuildService.Project, target: BuildService.Target,
                         route: InstallRoute, options: BuildOptions) async {
        let key = Self.key(project, target)
        guard phases[key]?.isWorking != true, !service.busy else { return }
        phases[key] = .building(job: 0)
        guard let queued = await service.build(project: project, target: target, branch: options.branch,
                                               install: route.mode, launch: options.launch && route == .xcodeTunnel,
                                               debug: options.debug, patch: options.patch, injectBackend: options.injectBackend) else {
            phases[key] = .failed(service.message, keptBuild: nil)
            return
        }
        phases[key] = .building(job: queued.id)
        guard let job = await service.finished(queued) else {
            phases[key] = .failed("Lost the build service before build #\(queued.id) finished.", keptBuild: nil)
            return
        }
        guard job.status == "success" else {
            let reason = job.error ?? "Build #\(job.id) \(job.status)."
            // The Mac keeps a build it could not install; this device can install it itself.
            phases[key] = .failed(reason, keptBuild: job.keptBuildId.flatMap { service.artifact(buildId: $0) != nil ? $0 : nil })
            return
        }
        switch route {
        case .xcodeTunnel:
            phases[key] = .done("Installed from the Mac.")
        case .thisDevice:
            guard let buildId = job.buildId, let artifact = service.artifact(buildId: buildId) else {
                phases[key] = .failed("The Mac finished build #\(job.id) but did not keep it.", keptBuild: nil)
                return
            }
            await performInstall(artifact, project: project, target: target, launch: options.launch)
        }
    }

    /// Builds a target and keeps the IPA, installing nothing anywhere.
    func buildOnly(project: BuildService.Project, target: BuildService.Target, options: BuildOptions) async {
        await service.build(project: project, target: target, branch: options.branch, install: .none, launch: false,
                            debug: options.debug, patch: options.patch, injectBackend: options.injectBackend)
    }

    /// Downloads a build the Mac kept, signs it here and installs it, then tells the service so its
    /// ledger (and every other client) shows it.
    func install(_ artifact: BuildService.Artifact, project: BuildService.Project, target: BuildService.Target, launch: Bool = false) async {
        guard phases[Self.key(project, target)]?.isWorking != true else { return }
        await performInstall(artifact, project: project, target: target, launch: launch)
    }

    private func performInstall(_ artifact: BuildService.Artifact, project: BuildService.Project, target: BuildService.Target, launch: Bool) async {
        let key = Self.key(project, target)
        phases[key] = .downloading
        let file: URL
        do { file = try await service.download(artifact) }
        catch { phases[key] = .failed(error.localizedDescription, keptBuild: artifact.buildId); return }
        defer { try? FileManager.default.removeItem(at: file) }
        phases[key] = .installing
        let result = await library.installProjectBuild(file, project: project.name, buildId: artifact.buildId, signing: artifact.signing)
        switch result {
        case .installed(let name):
            phases[key] = .done("\(name) installed on this \(ThisDevice.noun).")
            await service.reportInstall(target: target, buildId: artifact.buildId)
            await library.refreshDeviceApps()
            if launch, let bundleId = library.installed.first(where: { $0.id == target.bundleId })?.installedBundleId {
                Task { await probe.launchJIT(host: probe.defaultTunnelHost, port: probe.defaultTunnelPort, bundleId: bundleId, waitSeconds: 600) }
            }
        case .cancelled:
            phases[key] = .failed("Installing was cancelled.", keptBuild: artifact.buildId)
        case .failed(let reason):
            phases[key] = .failed(reason, keptBuild: artifact.buildId)
        }
    }

    /// Where installing a build another computer sent has got to.
    func sentPhase(_ artifact: BuildService.Artifact) -> Phase? { phases["sent|\(artifact.app.bundleId)"] }

    /// Downloads a build another computer sent to the Mac (`apparition deploy`), signs it here and
    /// installs it, like a project's kept build. It belongs to no project: its reservations are
    /// kept under its bundle id.
    func installSent(_ artifact: BuildService.Artifact) async {
        let key = "sent|\(artifact.app.bundleId)"
        guard phases[key]?.isWorking != true else { return }
        phases[key] = .downloading
        let file: URL
        do { file = try await service.download(artifact) }
        catch { phases[key] = .failed(error.localizedDescription, keptBuild: artifact.buildId); return }
        defer { try? FileManager.default.removeItem(at: file) }
        phases[key] = .installing
        switch await library.installProjectBuild(file, project: "sent:\(artifact.app.bundleId)", buildId: artifact.buildId, signing: artifact.signing) {
        case .installed(let name):
            phases[key] = .done("\(name) installed on this \(ThisDevice.noun).")
            await library.refreshDeviceApps()
        case .cancelled:
            phases[key] = .failed("Installing was cancelled.", keptBuild: artifact.buildId)
        case .failed(let reason):
            phases[key] = .failed(reason, keptBuild: artifact.buildId)
        }
    }

    /// The newest kept build of a target that is not the one installed, if any: what Install update offers.
    func update(for project: BuildService.Project, target: BuildService.Target, installedBuild: String?) -> BuildService.Artifact? {
        guard let bundleId = target.bundleId,
              let latest = service.latestArtifact(project: project, target: target, bundleId: bundleId) else { return nil }
        return installedBuild == nil || (latest.app.build ?? latest.buildId) != installedBuild ? latest : nil
    }
}
