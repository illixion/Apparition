// Compiled and driven by verify-device-build-service.cjs; no device or signing needed.
import Foundation

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
        let service = BuildService(defaults: defaults)
        service.address = base
        await service.connect(password: "fixture-password")
        precondition(service.connected && service.state?.repos.count == 1, service.message)
        let project = service.state!.repos[0]
        await service.build(project: project, target: project.platforms[0], branch: "current", deploy: false,
                            launch: true, debug: true, patch: false)
        precondition(service.state?.builds.count == 1, service.message)
        let job = service.state!.builds[0]
        await service.log(job)
        precondition(service.detail == "fixture build log")
        await service.cancel(job)
        precondition(service.state?.builds[0].status == "cancelled")
        await service.disconnect()
        precondition(!service.connected && service.state == nil && service.detail.isEmpty)

        // A redirect must be rejected instead of moving the password to another route.
        await service.connect(password: "redirect")
        precondition(!service.connected && service.message.contains("302"))
        await service.connect(password: "oversized")
        precondition(!service.connected && service.message.contains("oversized"))
        print("PASS: device client auth, builds, logs, cancellation, revocation, URL and response boundaries")
    }
}
