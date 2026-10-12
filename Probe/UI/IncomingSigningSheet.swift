import RAVEUI
import SwiftUI

/// Shown over any tab when signing files arrive (shared, opened in Apparition, picked, or put
/// in Files > Apparition > Signing): asks each certificate's password and lists what was
/// imported.
struct IncomingSigningSheet: View {
    let signing: SigningStore

    @State private var password = ""
    @State private var error: String?
    @State private var working = false
    @FocusState private var focused: Bool

    var body: some View {
        NavigationStack {
            Form {
                RAVEScreenHeading(title)
                if let pending = signing.pendingCertificates.first {
                    Section {
                        SecureField("Password", text: $password)
                            .textContentType(.password)
                            .focused($focused)
                            .onSubmit { submit(pending) }
                        if let error { Note(text: error, kind: .error) }
                    } header: {
                        Text(pending.url.lastPathComponent)
                    } footer: {
                        Text("The password chosen when the certificate was exported. It opens the file once on this \(ThisDevice.noun) and isn't stored.")
                    }
                    Section {
                        Button("Import Certificate") { submit(pending) }
                            .disabled(working || password.isEmpty)
                        Button("Skip", role: .cancel) {
                            signing.skipPending(pending)
                            reset()
                        }
                        .disabled(working)
                    }
                }
                if !signing.incomingResults.isEmpty {
                    Section {
                        ForEach(signing.incomingResults) { result in
                            HStack(alignment: .top) {
                                VStack(alignment: .leading, spacing: 4) {
                                    Text(result.file).font(.headline)
                                    Note(text: result.text, kind: result.ok ? .success : .error)
                                    if let missing = signing.missingHalf(of: result) {
                                        Note(text: missing, kind: .warning)
                                    }
                                }
                                Spacer()
                                if result.identityId != nil || result.profileUUID != nil {
                                    Button("Remove", role: .destructive) { signing.removeImported(result) }
                                        .buttonStyle(.bordered)
                                }
                            }
                            .focusRow()
                        }
                    } header: {
                        Text("Imported")
                    } footer: {
                        Text("A certificate and its profile can arrive in either order, one share at a time. Anything imported stays until you remove it here or in Settings > Signing.")
                    }
                }
            }
            .raveScreenTitle(title)
            .toolbar {
                if signing.pendingCertificates.isEmpty {
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Done") { signing.clearResults() }
                    }
                }
            }
            .onAppear { focused = true }
        }
        .sheetFrame(minWidth: 560, minHeight: 360)
    }

    private var title: String {
        signing.pendingCertificates.isEmpty ? "Signing Files" : "Import Certificate"
    }

    private func submit(_ pending: SigningStore.IncomingCertificate) {
        guard !password.isEmpty else { return }
        working = true
        let typed = password
        Task {
            do {
                try await signing.importPending(pending, password: typed)
                reset()
            } catch {
                self.error = "\(error)"
            }
            working = false
        }
    }

    private func reset() {
        password = ""
        error = nil
    }
}

extension SigningStore {
    /// Whether IncomingSigningSheet has anything to show.
    var hasIncoming: Bool { !pendingCertificates.isEmpty || !incomingResults.isEmpty }
}
