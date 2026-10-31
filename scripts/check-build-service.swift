// Compiled and driven by verify-device-build-service.cjs; no device or signing needed.
import Foundation

/// BuildService asks the real `ThisDevice` (UIKit) which platforms suit this device's apps; this
/// macOS check has no UIKit, so a stand-in answers with the headset's.
enum ThisDevice { static func preferredBuildPlatforms(deviceFamily: [Int]?) -> [String] { ["visionos"] } }

@main struct CheckBuildService {
    @MainActor static func main() async throws {
        for address in ["http://192.168.1.2", "https://user:password@example.com", "https://example.com/path", "https://example.com?q=1", "file:///tmp/a"] {
            do { _ = try BuildService.serviceURL(address); preconditionFailure("accepted invalid address") }
            catch { precondition(error is BuildService.Failure) }
        }
        _ = try BuildService.serviceURL("https://example.com/")
        let base = CommandLine.arguments[1]
        let suite = "com.illixion.apparition.service-check.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        var stored: [String: String] = [:]
        let passwords = BuildServicePasswords(load: { stored[$0] }, save: { stored[$0] = $1; return true },
            delete: { origin in if let origin { stored.removeValue(forKey: origin) } else { stored.removeAll() } })
        let service = BuildService(defaults: defaults, passwords: passwords)
        service.address = base
        await service.connect(password: "fixture-password")
        precondition(service.connected && service.state?.repos.count == 1, service.message)
        let origin = try BuildService.serviceURL(base).absoluteString
        precondition(stored[origin] == "fixture-password")
        precondition(!defaults.dictionaryRepresentation().values.contains { ($0 as? String) == "fixture-password" })
        let restored = BuildService(defaults: defaults, passwords: passwords)
        await restored.restoreConnection()
        precondition(restored.connected && restored.state?.repos.count == 1, restored.message)
        // A saved password for this origin is never sent to another service.
        let other = BuildService(defaults: defaults, passwords: passwords)
        other.address = "https://other.example.invalid"
        await other.restoreConnection()
        precondition(!other.connected)
        let project = service.state!.repos[0]
        await service.build(project: project, target: project.platforms[0], branch: "current", deploy: false,
                            launch: true, debug: true, patch: false)
        precondition(service.state?.builds.count == 1, service.message)
        let job = service.state!.builds[0]
        await service.log(job)
        precondition(service.detail == "fixture build log")
        await service.cancel(job)
        precondition(service.state?.builds[0].status == "cancelled")
        // Re-signing: offered only when the service says so, for the devices a project installs the app on,
        // with only unexpired profiles, and the body is exactly the five plain fields.
        precondition(service.canResign)
        let kept = service.artifacts[0]
        precondition(service.devices(for: kept) == ["AVP"])
        let profiles = await service.resignProfiles()
        precondition(profiles.map(\.id) == ["fixture-dev"], "expired profiles are not offered")
        let queued = await service.resign(.kept(kept), device: "AVP", profile: profiles[0], keepData: false, dryRun: true)
        precondition(queued?.id == 2 && service.state?.builds.count == 2, service.message)
        precondition(service.state?.builds.first { $0.id == 2 }?.kind == "Re-sign check")
        // The service's own sentence reaches the person, not a bare status code.
        let refused = BuildService.ResignProfile(id: "refused", name: "x", appId: "x", expired: false, debuggable: true)
        let none = await service.resign(.kept(kept), device: "AVP", profile: refused, keepData: true, dryRun: false)
        precondition(none == nil && service.message == "No configured project installs com.example.fixture on AVP.", service.message)
        // The installed build is re-signed when the Mac kept it; a target with no kept build is built first.
        precondition(service.canResignByBuilding && service.device(named: "AVP")?.reachable == true)
        precondition(service.install(for: project.platforms[0])?.buildId == "fixture-build" && service.install(for: project.platforms[1]) == nil)
        guard case .kept(let installed)? = service.resignSource(for: project, target: project.platforms[0]), installed.buildId == "fixture-build" else {
            preconditionFailure("the kept installed build is not the re-sign source")
        }
        guard case .build(_, let phone)? = service.resignSource(for: project, target: project.platforms[1]), phone.device == "Phone" else {
            preconditionFailure("a target without a kept build is not built first")
        }
        let chained = await service.resign(.build(project, phone), device: "Phone", profile: profiles[0], keepData: true, dryRun: false)
        precondition(chained?.id == 4 && service.message == "Queued build #3, then re-sign #4.", service.message)
        precondition(project.git?.commit == "abc1234" && service.artifacts(for: project).count == 1)
        await service.disconnect()
        precondition(!service.canResign && !service.connected && service.state == nil && service.detail.isEmpty)
        precondition(stored[origin] == nil)
        let disconnected = BuildService(defaults: defaults, passwords: passwords)
        await disconnected.restoreConnection()
        precondition(!disconnected.connected)
        stored[origin] = "fixture-password"
        restored.rememberPassword = false
        precondition(stored.isEmpty)
        precondition(!BuildService(defaults: defaults, passwords: passwords).rememberPassword)

        // A redirect must be rejected instead of moving the password to another route.
        await service.connect(password: "redirect")
        precondition(!service.connected && service.message.contains("302"))
        await service.connect(password: "oversized")
        precondition(!service.connected && service.message.contains("oversized"))
        service.rememberPassword = true
        stored[origin] = "fixture-password"
        await service.connect(password: "rejected")
        precondition(stored[origin] == "fixture-password") // Never save a rejected replacement.
        stored[origin] = "rejected"
        let rejected = BuildService(defaults: defaults, passwords: passwords)
        await rejected.restoreConnection()
        precondition(!rejected.connected && stored[origin] == nil) // Do not retry a revoked password on every launch.
        stored[origin] = "fixture-password"
        let otherOrigin = "https://other.example.invalid/"
        stored[otherOrigin] = "other-fixture"
        let pending = Task { await service.connect(password: "rejected-delayed", restoring: true) }
        try await Task.sleep(for: .milliseconds(100))
        await service.disconnect()
        service.address = otherOrigin
        await pending.value
        precondition(stored[otherOrigin] == "other-fixture")
        precondition(service.message == "Disconnected.") // A stale reply cannot change the new connection state.
        print("PASS: password restoration, expiry recovery, origin scoping, forgetting, rejected credentials, transport boundaries, install state and re-sign requests (kept and built first)")
    }
}
