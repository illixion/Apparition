import RAVEUI
import SwiftUI

/// Shares the Apple ID's certificate key with another device (core/src/share.rs), so both sign
/// with one certificate instead of using up a free account's two. Nearby, it goes over the local
/// network to Apparition waiting for it there, checked by a six-digit code both screens show
/// (NearbySigning.swift); this is the only way on tvOS, which has no AirDrop. Anywhere else, as a
/// .apparitionkey file sealed under five words, sent with the share sheet (AirDrop, Messages,
/// Files), with the words shown here for the other device's owner to type. The file is deleted
/// when the sheet closes.
struct ShareSigningSheet: View {
    let signing: SigningStore
    let email: String
    let teamId: String?
    let onDone: () -> Void

    var body: some View {
        NavigationStack {
            Form {
                RAVEScreenHeading("Share Signing")
                Section {
                    Text("Another iPhone, iPad, Vision Pro, Apple TV or computer signing with \(email) can use this \(ThisDevice.noun)'s certificate instead of making its own. A free Apple ID holds two, and each one past that means revoking one, which stops the apps it signed.")
                        .focusableText()
                }
                NearbySendSection(signing: signing, email: email, teamId: teamId)
                #if !os(tvOS)
                KeyFileSection(signing: signing, email: email, teamId: teamId)
                #endif
            }
            .raveScreenTitle("Share Signing")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done", action: onDone)
                }
            }
        }
        .sheetFrame(minWidth: 620, minHeight: 520)
        .task { signing.nearbySender.startBrowsing() }
        .onDisappear {
            signing.nearbySender.stopBrowsing()
            signing.nearbySender.reset()
        }
    }
}

#if !os(tvOS)  // No share sheet or AirDrop there.
/// The key as a sealed file, for a device that isn't on this network.
private struct KeyFileSection: View {
    let signing: SigningStore
    let email: String
    let teamId: String?

    @State private var shared: (file: URL, words: String)?
    @State private var error: String?
    @State private var sealing = false

    var body: some View {
        Group {
            if let shared {
                Section {
                    Text(shared.words)
                        .font(.system(.title2, design: .monospaced))
                        .textSelection(.enabled)
                        .padding(.vertical, 6)
                } header: {
                    Text("The Five Words")
                } footer: {
                    Text("Tell them to whoever opens the file; don't send them with it. Anyone with both can sign as this Apple ID.")
                }
                Section {
                    ShareLink(item: shared.file, preview: SharePreview("Apple ID signing")) {
                        Label("Share Key File…", systemImage: "square.and.arrow.up")
                    }
                } footer: {
                    Text("AirDrop it to the other device, or send it any way you like; it can't be opened without the words. Then open it in Apparition there (on a computer, in Apparition's signing step, or `apparition signing import`).")
                }
            } else {
                Section {
                    Button(sealing ? "Sealing the key…" : "Make a Key File") { seal() }
                        .disabled(sealing)
                    if let error { Note(text: error, kind: .error) }
                } header: {
                    Text("Key File")
                } footer: {
                    Text("For a device that isn't on this network: a file only the five words shown with it open.")
                }
            }
        }
        .onDisappear {
            if let shared { signing.discardShare(shared.file) }
        }
    }

    private func seal() {
        sealing = true
        Task {
            do {
                shared = try await signing.shareAppleIDKey(email: email, teamId: teamId)
                error = nil
            } catch {
                self.error = "\(error)"
            }
            sealing = false
        }
    }
}
#endif
