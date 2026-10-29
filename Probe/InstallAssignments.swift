import Foundation

/// Reservations survive forgetting an app's archive. A failed replacement also keeps the old
/// extensions reserved until an installation succeeds, because the old copy may still be present.
struct InstallAssignment: Codable, Sendable {
    var source: String
    var bundleId: String
    var name: String
    var team: String
    var target: String
    var extensions: [String]
    var pendingClaims: [String] = []

    var claims: Set<String> {
        var result = Set([target] + pendingClaims)
        for child in extensions where child.hasPrefix(bundleId + ".") {
            result.insert(target + child.dropFirst(bundleId.count))
        }
        return result
    }
}

struct InstallAssignments: Codable, Sendable {
    private(set) var records: [InstallAssignment] = []

    mutating func reserve(_ assignment: InstallAssignment) {
        var assignment = assignment
        for old in records where old.team == assignment.team && old.target == assignment.target {
            assignment.pendingClaims += old.claims.filter { $0 != assignment.target }
        }
        assignment.pendingClaims = Array(Set(assignment.pendingClaims)).sorted()
        records.removeAll { $0.team == assignment.team && $0.target == assignment.target }
        records.append(assignment)
    }

    mutating func complete(target: String, team: String) {
        for index in records.indices where records[index].target == target && records[index].team == team {
            records[index].pendingClaims = []
        }
    }

    /// A forgotten and uninstalled app has no refresh left that could reclaim these IDs.
    mutating func reconcile(installed: Set<String>, managed: Set<String>) {
        records.removeAll { !installed.contains($0.target) && !managed.contains($0.target) }
    }

    mutating func remove(target: String) {
        records.removeAll { $0.target == target }
    }
}
