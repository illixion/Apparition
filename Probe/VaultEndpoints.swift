import DebugTrace
import Foundation

/// Development endpoints for StateVault: what it holds, and a purge to test the restore with.
@MainActor
enum VaultEndpoints {
    static func register() {
        DebugSurface.shared.register([
            .query("vaultStatus", "Files StateVault keeps (on disk and in the Keychain, with sizes) and any the Keychain has that the disk lost") { _ in
                StateVault.status()
            },
            .command("vaultSync", "Run StateVault's sync now, as at launch: restore what's missing after a purge, then mirror the files into the Keychain") { _ in
                StateVault.sync()
            },
            .command("vaultSimulatePurge",
                     "Delete everything Apparition keeps in Caches, as a tvOS storage purge would. Relaunch (or vaultSync) to restore",
                     parameters: [.string("confirm", "Type purge", required: true)]) { args in
                guard try args.requireString("confirm") == "purge" else {
                    throw DebugError.failedPrecondition("Not confirmed", hint: "Pass confirm=purge")
                }
                return ["deleted": try StateVault.simulatePurge()]
            },
        ])
    }
}
