// swiftc Probe/InstallAssignments.swift scripts/check-install-assignments.swift -o /tmp/apparition-assignments-check
import Foundation

@main struct CheckInstallAssignments {
    static func main() throws {
        var store = InstallAssignments()
        store.reserve(.init(source: "first", bundleId: "org.first", name: "First", team: "TEAM",
                            target: "pool.id", extensions: ["org.first.share"]))
        store.complete(target: "pool.id", team: "TEAM")
        // A failed replacement must keep the old extension reserved after an app restart.
        store.reserve(.init(source: "second", bundleId: "org.second", name: "Second", team: "TEAM",
                            target: "pool.id", extensions: ["org.second.widget"]))
        store = try JSONDecoder().decode(InstallAssignments.self, from: JSONEncoder().encode(store))
        precondition(store.records.count == 1)
        precondition(store.records[0].source == "second")
        precondition(store.records[0].claims == ["pool.id", "pool.id.share", "pool.id.widget"])
        // Once installation replaces the old copy, only the new extension owns an ID.
        store.complete(target: "pool.id", team: "TEAM")
        precondition(store.records[0].claims == ["pool.id", "pool.id.widget"])
        // A forgotten app keeps its reservation while installed. A managed failed install
        // keeps it even if no copy reached the device. Only an absent, unmanaged copy is free.
        store.reconcile(installed: ["pool.id"], managed: [])
        precondition(store.records.count == 1)
        store.reconcile(installed: [], managed: ["pool.id"])
        precondition(store.records.count == 1)
        store.reconcile(installed: [], managed: [])
        precondition(store.records.isEmpty)
        // Moving to another ID retains both claims until migration removes the old copy.
        store.reserve(.init(source: "app", bundleId: "org.app", name: "App", team: "TEAM",
                            target: "old.id", extensions: []))
        store.reserve(.init(source: "app", bundleId: "org.app", name: "App", team: "TEAM",
                            target: "new.id", extensions: []))
        precondition(store.records.count == 2)
        store.remove(target: "old.id")
        precondition(store.records.map(\.target) == ["new.id"])
        print("PASS: native assignment persistence, failed replacement, forgetting, reconciliation and migration")
    }
}
