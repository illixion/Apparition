import RAVEUI
import SwiftUI

#if !os(tvOS)  // No share sheet or AirDrop there.

/// Shares the Apple ID's certificate key with another device (core/src/share.rs), so both sign
/// with one certificate instead of using up a free account's two: a .apparitionkey file sealed
/// under five words, sent with the share sheet (AirDrop, Messages, Files), and the words, shown
/// here for the other device's owner to type. The file is deleted when the sheet closes.
struct ShareSigningSheet: View {
    let signing: SigningStore
    let email: String
    let teamId: String?
    let onDone: () -> Void

    @State private var shared: (file: URL, words: String)?
    @State private var error: String?

    var body: some View {
        NavigationStack {
            Form {
                RAVEScreenHeading("Share Signing")
                Section {
                    Text("Another iPhone, iPad, Vision Pro or computer signing with \(email) can use this \(ThisDevice.noun)'s certificate instead of making its own. A free Apple ID has two, and each new one stops the apps the oldest signed.")
                }
                if let shared {
                    Section {
                        Text(shared.words)
                            .font(.system(.title2, design: .monospaced))
                            .textSelection(.enabled)
                            .padding(.vertical, 6)
                    } header: {
                        Text("The five words")
                    } footer: {
                        Text("Tell them to whoever opens the file; don't send them with it. Anyone with both can sign as this Apple ID.")
                    }
                    Section {
                        ShareLink(item: shared.file, preview: SharePreview("Apple ID signing")) {
                            Label("Share Key File…", systemImage: "square.and.arrow.up")
                        }
                    } footer: {
                        Text("AirDrop it to the other device, or send it any way you like; it can't be opened without the words. Then open it in Apparition there (or on a computer, in Apparition's signing step).")
                    }
                } else if let error {
                    Section { Note(text: error, kind: .error) }
                } else {
                    Section { ProgressView("Sealing the key…") }
                }
            }
            .raveScreenTitle("Share Signing")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done", action: onDone)
                }
            }
        }
        .sheetFrame(minWidth: 560, minHeight: 420)
        .task {
            do {
                shared = try await signing.shareAppleIDKey(email: email, teamId: teamId)
            } catch {
                self.error = "\(error)"
            }
        }
        .onDisappear {
            if let shared { signing.discardShare(shared.file) }
        }
    }
}
#endif
