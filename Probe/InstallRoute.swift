import Foundation

/// How a project's build reaches a device: Apparition on the device downloads the kept IPA and
/// signs and installs it itself, or the Mac installs it through the Xcode tunnel (`devicectl`).
///
/// The device route is the default whenever it is possible. It needs only an HTTPS connection to
/// the build service, so it works over Tailscale, and it works with a free Apple ID. The tunnel is
/// kept as the secondary route: it is quicker for a build-and-sign update, it can launch the app,
/// and it signs with the Mac's paid profile. Only the device that is open can take the first route,
/// so a target on another device, or a device whose Apparition is closed, goes through the tunnel.
enum InstallRoute: String, Sendable, Equatable {
    /// Apparition on the device installs it (`install: device`).
    case thisDevice
    /// The Mac installs it through the Xcode tunnel (`install: mac`).
    case xcodeTunnel

    /// The mode the build service is asked for.
    var mode: BuildService.InstallMode {
        switch self {
        case .thisDevice: .device
        case .xcodeTunnel: .mac
        }
    }
}

/// What can install a target right now, and which route is the default.
struct InstallOptions: Equatable, Sendable {
    /// Why a route is not available, in a sentence for the person; nil when it is.
    var thisDeviceProblem: String?
    var tunnelProblem: String?

    var canInstallHere: Bool { thisDeviceProblem == nil }
    var canUseTunnel: Bool { tunnelProblem == nil }
    /// The device route when it works, else the tunnel, else nothing.
    var preferred: InstallRoute? {
        if canInstallHere { return .thisDevice }
        if canUseTunnel { return .xcodeTunnel }
        return nil
    }
    /// The other route, when there is one to offer as a secondary choice.
    var secondary: InstallRoute? {
        guard let preferred else { return nil }
        let other: InstallRoute = preferred == .thisDevice ? .xcodeTunnel : .thisDevice
        return (other == .thisDevice ? canInstallHere : canUseTunnel) ? other : nil
    }
    /// The sentence explaining why nothing can install it, naming both reasons.
    var unavailable: String? {
        guard preferred == nil else { return nil }
        return [thisDeviceProblem, tunnelProblem].compactMap { $0 }.joined(separator: " ")
    }
}

extension InstallOptions {
    /// What is known about a target and this device, as plain values so it can be checked without
    /// the app's models.
    struct Facts: Sendable {
        /// The target is this device (its name is the paired one, or its platform suits this device).
        var targetIsThisDevice: Bool
        /// The service takes `install: device` and `POST /installs`.
        var serviceInstallsOnDevice: Bool
        /// Apparition can sign for this device: an Apple ID is signed in or a certificate is imported.
        var canSignHere: Bool
        /// Apparition has a way to reach this device's installer (a pairing).
        var canReachInstaller: Bool
        /// The Mac lists the target's device and says it can reach it. nil: the service doesn't say.
        var macSeesDevice: Bool?
        /// The Mac has no record of the device at all (not paired with it).
        var macKnowsDevice: Bool
        var deviceName: String
    }

    init(_ facts: Facts) {
        let name = facts.deviceName
        if !facts.targetIsThisDevice {
            thisDeviceProblem = "\(name) is not this device, so only its own Apparition can install here."
        } else if !facts.serviceInstallsOnDevice {
            thisDeviceProblem = "This build service is too old to hand builds to the device."
        } else if !facts.canSignHere {
            thisDeviceProblem = "Sign in with an Apple ID or import a certificate to install here."
        } else if !facts.canReachInstaller {
            thisDeviceProblem = "Pair Apparition with this device in Setup to install here."
        }
        if !facts.macKnowsDevice {
            tunnelProblem = "\(name) is not paired with the Mac."
        } else if facts.macSeesDevice == false {
            tunnelProblem = "The Mac can't reach \(name) right now."
        }
    }
}

/// Which project and target an app on the device belongs to. Chosen by the person, or proposed from
/// the bundle identifier; it outlives refreshes because it is stored by the app's own identifier.
struct ProjectLink: Codable, Equatable, Hashable, Sendable {
    var project: String
    var device: String
    var platform: String
    /// A person confirmed or chose it, as opposed to the app being matched on its own.
    var chosen: Bool
}

/// Bundle identifiers Apparition and Xcode produce for one app: the project's own, with a team suffix
/// (a free Apple ID adds its team) or a `.debug` suffix (an Xcode debug build alongside the real one).
enum BundleIdentity {
    /// The identifier a project's target would be configured with.
    static func base(_ bundleId: String) -> String {
        var parts = bundleId.split(separator: ".", omittingEmptySubsequences: false).map(String.init)
        // The team comes last (Apparition adds it to whatever identifier the build had), and the
        // debug suffix is the build's own; strip each at most once, in whichever order they came.
        var teamStripped = false, debugStripped = false
        while parts.count > 2, let last = parts.last {
            if !debugStripped, last == "debug" { parts.removeLast(); debugStripped = true }
            else if !teamStripped, isTeamID(last) { parts.removeLast(); teamStripped = true }
            else { break }
        }
        return parts.joined(separator: ".")
    }

    /// Whether `installed` is `target` itself or the same app under a team or debug suffix.
    static func matches(installed: String, target: String) -> Bool {
        installed == target || base(installed) == base(target)
    }

    static func isTeamID(_ value: String) -> Bool {
        value.count == 10 && value.allSatisfy { $0.isASCII && ($0.isUppercase || $0.isNumber) }
    }
}

/// Stored links, keyed by the installed app's bundle identifier.
struct ProjectLinks: Codable, Equatable, Sendable {
    private(set) var byBundleId: [String: ProjectLink] = [:]

    func link(for bundleId: String) -> ProjectLink? { byBundleId[bundleId] }

    mutating func set(_ link: ProjectLink, for bundleId: String) { byBundleId[bundleId] = link }

    /// Forgets the link, and records that a person did, so matching doesn't propose it again.
    mutating func unlink(_ bundleId: String) {
        byBundleId[bundleId] = ProjectLink(project: "", device: "", platform: "", chosen: true)
    }

    func isUnlinked(_ bundleId: String) -> Bool { byBundleId[bundleId].map { $0.project.isEmpty } ?? false }
}

extension BuildService {
    /// The project target an installed app belongs to: the person's link when there is one, else the
    /// best match on the bundle identifier (exactly, or modulo a team or debug suffix). A match never
    /// overrides a link, and an app the person unlinked is not matched again.
    func projectFor(bundleId: String, deviceFamily: [Int]?, preferredDevice: String?,
                    links: ProjectLinks) -> (project: Project, target: Target, link: ProjectLink?)? {
        if let link = links.link(for: bundleId) {
            guard !link.project.isEmpty else { return nil }
            guard let project = project(named: link.project),
                  let target = project.platforms.first(where: {
                      $0.device == link.device && $0.platform.caseInsensitiveCompare(link.platform) == .orderedSame
                  }) else { return nil }
            return (project, target, link)
        }
        if let exact = matchingProject(for: bundleId, deviceFamily: deviceFamily, preferredDevice: preferredDevice) {
            return (exact.project, exact.target, nil)
        }
        let matches = (state?.repos ?? []).flatMap { project in
            project.platforms.compactMap { target -> (Project, Target)? in
                guard let id = target.bundleId, BundleIdentity.matches(installed: bundleId, target: id) else { return nil }
                return (project, target)
            }
        }
        if let preferredDevice, let local = matches.first(where: { $0.1.device.caseInsensitiveCompare(preferredDevice) == .orderedSame }) {
            return (local.0, local.1, nil)
        }
        for platform in ThisDevice.preferredBuildPlatforms(deviceFamily: deviceFamily) {
            if let match = matches.first(where: { $0.1.platform.lowercased() == platform }) { return (match.0, match.1, nil) }
        }
        return matches.first.map { ($0.0, $0.1, nil) }
    }
}
