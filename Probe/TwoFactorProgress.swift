import Foundation

/// Status only: no code is retained here. A requested prompt is not proof it went unanswered.
final class TwoFactorAsked: @unchecked Sendable {
    private let lock = NSLock()
    private var asked = false
    private var response: String?

    func set() {
        lock.lock(); defer { lock.unlock() }
        asked = true
        response = nil
    }

    @discardableResult
    func record(_ answer: String?) -> String {
        lock.lock(); defer { lock.unlock() }
        let kind: String
        if let answer, answer.hasPrefix("code:") { kind = "code submitted" }
        else if answer == nil || answer == "abort" { kind = "cancelled or timed out" }
        else { kind = "delivery method selected" }
        response = kind
        return kind
    }

    var value: Bool {
        lock.lock(); defer { lock.unlock() }
        return asked
    }

    var unanswered: Bool {
        lock.lock(); defer { lock.unlock() }
        return asked && (response == nil || response == "cancelled or timed out")
    }
}
