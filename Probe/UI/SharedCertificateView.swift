import SwiftUI

/// In place of the error when signing stopped rather than make a second certificate: Apparition
/// on another device already has the Apple ID's, and its key would let this one sign with it too
/// (core's check_certificate). Offers to get the key, or to make a certificate anyway, and runs
/// the signing again after either. Bare content: a List section in Browse, a Card in the Library.
struct SharedCertificateView: View {
    let account: AccountModel
    let library: LibraryModel
    let choice: LibraryModel.CertificateChoice

    @State private var receiving = false
    @State private var confirmingNew = false

    private var working: Bool { library.busy }
    private var email: String? { account.email ?? account.lastAppleID }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Label("Another Apparition has this Apple ID's certificate", systemImage: "person.2.badge.key")
                .font(.headline)
                .foregroundStyle(.orange)
            Text("Apparition on another device or computer already signs with \(email ?? "this Apple ID"). Get its key so this \(ThisDevice.noun) signs with the same certificate, then \(choice.appName) installs. A free Apple ID holds only two certificates, and making one past that means revoking one, which stops the apps it signed.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .focusableText()
            HStack(spacing: 12) {
                Button("Get It from Another Device…") { receiving = true }
                    .buttonStyle(.borderedProminent)
                    .disabled(working)
                Button("Make a New Certificate") { confirmingNew = true }
                    .buttonStyle(.bordered)
                    .disabled(working)
                    .confirmationDialog("Make a new certificate here?", isPresented: $confirmingNew, titleVisibility: .visible) {
                        Button("Make a New Certificate") { Task { await library.makeNewCertificate() } }
                    } message: {
                        Text("It takes the account's other certificate slot, and the other device keeps its own. Choose this only if no device has Apparition's key any more (Apparition was deleted there, say). If both slots are taken, you'll be asked to revoke one.")
                    }
                Button("Dismiss") { library.dismissCertificateChoice() }
                    .buttonStyle(.borderless)
            }
            .focusRow()
            Text("On the other device: Settings > Share Signing with Another Device, then choose this \(ThisDevice.noun) under Nearby.")
                .font(.footnote)
                .foregroundStyle(.secondary)
                .focusableText()
        }
        .padding(.vertical, 6)
        .panel(isPresented: $receiving) {
            ReceiveSigningSheet(signing: library.signing, email: email) { imported in
                receiving = false
                if imported { Task { await library.retryAfterCertificateChoice() } }
            }
        }
    }
}

/// What to offer next to a slot error. For certificates (7460), getting another Apparition's
/// key comes first: it needs no slot, where revoking stops the apps the certificate signed.
struct SlotErrorActions: View {
    let account: AccountModel
    let library: LibraryModel
    let error: String
    let onSetup: () -> Void
    /// Runs again what failed, once a key arrived.
    var retry: (() -> Void)? = nil

    var body: some View {
        if DeveloperSlots.isCertificateSlotError(error) {
            Note(text: "Revoking a certificate stops the apps it signed. If one of the two is Apparition's on another device, get its key instead and this \(ThisDevice.noun) signs with that one.", kind: .info)
            HStack(spacing: 12) {
                GetSigningButton(signing: library.signing, email: account.email ?? account.lastAppleID, onImported: retry)
                ManageSlotsButton(account: account, library: library, onSignIn: onSetup)
            }
            .focusRow()
        } else {
            ManageSlotsButton(account: account, library: library, onSignIn: onSetup)
        }
    }
}
