import RAVEUI
import SwiftUI
#if canImport(UIKit)
import UIKit
import UniformTypeIdentifiers
#endif

/// Shares the Apple ID's certificate key with another device (core/src/share.rs), so both sign
/// with one certificate instead of using up a free account's two. Nearby, it goes over the local
/// network to Apparition waiting for it there, checked by a six-digit code both screens show
/// (NearbySigning.swift); this is the only way on tvOS, which has no AirDrop. Anywhere else, as a
/// .apparitionkey file sealed under five words, sent with the share sheet (AirDrop, Messages,
/// Files), with the words shown here for the other device's owner to type. For an app that isn't
/// Apparition (LiveContainer's JIT-less mode, say), the certificate as a .p12 under a password of
/// six words, to copy or share. Files are deleted when the sheet closes.
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
                CertificateFileSection(signing: signing)
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

/// The certificate as a .p12 for another app to import. Apparition's own sealed file is for
/// Apparition; this is for apps that read the standard format. It is the means to sign as the
/// Apple ID, so the sheet says so before it makes the file, and the password is shown apart
/// from it.
private struct CertificateFileSection: View {
    let signing: SigningStore

    @State private var exported: (file: URL, password: String)?
    @State private var error: String?
    @State private var working = false
    @State private var confirming = false
    @State private var copied = false

    var body: some View {
        Group {
            if let exported {
                Section {
                    Text(exported.password)
                        .font(.system(.title3, design: .monospaced))
                        .textSelection(.enabled)
                        .padding(.vertical, 6)
                    Button {
                        copy(exported.password)
                    } label: {
                        Label(copied ? "Copied" : "Copy Password", systemImage: copied ? "checkmark" : "doc.on.doc")
                    }
                } header: {
                    Text("The Password")
                } footer: {
                    Text("Copying keeps it on this device's clipboard for two minutes, and not on your other devices. Send it apart from the file, over something other than where the file went.")
                }
                Section {
                    ShareLink(item: exported.file, preview: SharePreview("Apparition Certificate.p12")) {
                        Label("Share Certificate File…", systemImage: "square.and.arrow.up")
                    }
                    Note(text: "Anyone with this file and its password can sign apps as your Apple ID. Delete the file once the other app has it. If it ends up anywhere it shouldn't, revoke the certificate in Certificates & App IDs.", kind: .warning)
                } footer: {
                    Text("Import it in the other app with this password. This file is deleted when you close this screen.")
                }
            } else {
                Section {
                    Button(working ? "Making the file…" : "Export Certificate as .p12…") { confirming = true }
                        .disabled(working)
                    if let error { Note(text: error, kind: .error) }
                } header: {
                    Text("For Other Apps")
                } footer: {
                    Text("For an app that signs with a certificate you give it, such as LiveContainer's JIT-less mode. It needs you signed in, and it makes no new certificate.")
                }
            }
        }
        .confirmationDialog("Export your signing certificate?", isPresented: $confirming, titleVisibility: .visible) {
            Button("Export Certificate") { export() }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("The file holds your certificate and its private key. With its password, anyone can sign apps as your Apple ID. Only give it to an app you trust.")
        }
        .onDisappear {
            if let exported { signing.discardShare(exported.file) }
        }
    }

    private func export() {
        working = true
        Task {
            do {
                exported = try await signing.exportCertificateP12()
                error = nil
            } catch {
                let text = "\(error)"
                self.error = text.contains("Not signed in") ? "Sign in with your Apple ID first (Settings), then export again." : text
            }
            working = false
        }
    }

    /// Local only and gone after two minutes, so a clipboard history or another device's paste
    /// doesn't keep the password.
    private func copy(_ password: String) {
        #if canImport(UIKit)
        UIPasteboard.general.setItems(
            [[UTType.plainText.identifier: password]],
            options: [.localOnly: true, .expirationDate: Date().addingTimeInterval(120)])
        #endif
        copied = true
        Task {
            try? await Task.sleep(for: .seconds(3))
            copied = false
        }
    }
}
#endif
