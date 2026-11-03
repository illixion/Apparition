import Foundation

@main
struct TwoFactorProgressChecks {
    static func main() {
        let progress = TwoFactorAsked()
        precondition(!progress.value && !progress.unanswered)
        progress.set()
        precondition(progress.value && progress.unanswered)
        precondition(progress.record("code:000000") == "code submitted")
        precondition(!progress.unanswered, "Submitted code must preserve later verification/session errors")
        progress.set()
        precondition(progress.unanswered, "A second prompt needs a new response")
        progress.record("devices")
        precondition(!progress.unanswered)
        progress.set()
        progress.record(nil)
        precondition(progress.unanswered, "A timeout must pause unattended refresh")
        progress.set()
        progress.record("abort")
        precondition(progress.unanswered, "An explicit cancel must pause unattended refresh")
        print("2FA response classification: PASS")
    }
}
