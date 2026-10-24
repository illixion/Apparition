import RAVEUI
import SwiftUI

/// The six digits both screens show, as "123 456".
struct NearbyCodeView: View {
    let code: String

    var body: some View {
        Text(code.count == 6 ? "\(code.prefix(3)) \(code.suffix(3))" : code)
            .font(.system(size: 56, weight: .semibold, design: .monospaced))
            .frame(maxWidth: .infinity)
            .padding(.vertical, 8)
            .accessibilityLabel(code.map(String.init).joined(separator: " "))
            .focusableText()
    }
}

/// The comparison step on either side: the code, the question, and the two answers.
struct NearbyCompareSection: View {
    let code: String
    let peer: String
    let confirm: (Bool) -> Void

    var body: some View {
        Section {
            NearbyCodeView(code: code)
            HStack(spacing: 16) {
                Button("It Matches") { confirm(true) }
                    .buttonStyle(.borderedProminent)
                Button("It's Different", role: .destructive) { confirm(false) }
                    .buttonStyle(.bordered)
            }
            .focusRow()
        } header: {
            Text("Does \(peer) show this code?")
        } footer: {
            Text("Check the digits on both screens. A different code means something else on the network answered: choose It's Different and nothing is handed over.")
        }
    }
}

/// Gets the Apple ID's certificate key from Apparition on another device or computer, so this
/// one signs with the same certificate instead of making one of the account's two. Waits on
/// the local network for the other device to send it (Share Signing there), then both compare a
/// six-digit code. Also says the other ways in. `onDone(true)` once a key was imported.
struct ReceiveSigningSheet: View {
    let signing: SigningStore
    let email: String?
    let onDone: (Bool) -> Void

    private var receiver: NearbyReceiver { signing.nearbyReceiver }
    private var emailText: String { email ?? "<your Apple ID>" }

    var body: some View {
        NavigationStack {
            Form {
                RAVEScreenHeading("Get Signing")
                Section {
                    Text("A free Apple ID holds only two certificates, and making a third means revoking one, which stops the apps it signed. If Apparition on another device or computer already signs with \(email ?? "your Apple ID"), get its key and this \(ThisDevice.noun) signs with the same certificate.")
                        .focusableText()
                }
                switch receiver.phase {
                case .compare(let code, let peer):
                    NearbyCompareSection(code: code, peer: peer) { matches in Task { await receiver.confirm(matches) } }
                case .confirmed(let code, let peer):
                    Section {
                        NearbyCodeView(code: code)
                        HStack(spacing: 12) {
                            ProgressView()
                            Text("Waiting for \(peer) to confirm the code…").foregroundStyle(.secondary)
                        }
                    }
                case .done(let peer, let appleId):
                    Section {
                        Note(text: "Apps signed with \(appleId ?? "that Apple ID") on this \(ThisDevice.noun) now use the same certificate as \(peer).", kind: .success)
                    }
                default:
                    waitingSection
                    otherWaysSection
                }
            }
            .raveScreenTitle("Get Signing")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button(isDone ? "Done" : "Cancel") { onDone(isDone) }
                }
            }
        }
        .sheetFrame(minWidth: 620, minHeight: 520)
        .task { receiver.start() }
        .onDisappear { receiver.stop() }
    }

    private var isDone: Bool {
        if case .done = receiver.phase { true } else { false }
    }

    private var waitingSection: some View {
        Section {
            Label("In Apparition there: Settings > Share Signing with Another Device, then choose “\(receiver.name)” under Nearby.", systemImage: "iphone.and.arrow.forward")
                .focusableText()
            VStack(alignment: .leading, spacing: 6) {
                Label("On a computer, in Apparition: Signing > Send to a Nearby Device. Or in a terminal:", systemImage: "laptopcomputer")
                Text("apparition signing send --apple-id \(emailText)" + (addressFlag.map { " \($0)" } ?? ""))
                    .font(.callout.monospaced())
                    .foregroundStyle(.secondary)
                    .selectableText()
            }
            .focusableText()
            HStack(spacing: 12) {
                ProgressView()
                Text("Waiting as “\(receiver.name)”…").foregroundStyle(.secondary)
            }
            if case .connecting(let peer) = receiver.phase {
                Text("\(peer) is connecting…").foregroundStyle(.secondary)
            }
            if case .failed(let reason) = receiver.phase {
                Note(text: "\(reason). Still waiting, so you can try again.", kind: .warning)
            }
        } header: {
            Text("On the Device That Has It")
        } footer: {
            Text("Both screens then show a six-digit code. Check that they match on each: the key goes only to a device whose person confirms, and is used here only once you do.")
        }
    }

    /// `--to address:port`, for a computer that can't browse (a terminal on macOS, Tailscale).
    private var addressFlag: String? {
        guard let address = receiver.address, let port = receiver.port else { return nil }
        return "--to \(address):\(port)"
    }

    private var otherWaysSection: some View {
        Section {
            #if !os(tvOS)
            Text("Not on the same network: on the other device choose Share Signing > Make a Key File, send the file here (AirDrop, Messages, Files), open it in Apparition and type the five words it shows.")
                .focusableText()
            #endif
            VStack(alignment: .leading, spacing: 6) {
                Text("From a computer this \(ThisDevice.noun) is paired with, which already signs with the Apple ID:")
                Text("apparition signing hand-off --apple-id \(emailText)")
                    .font(.callout.monospaced())
                    .foregroundStyle(.secondary)
                    .selectableText()
            }
            .focusableText()
        } header: {
            Text("Other Ways")
        }
    }
}

/// Hands this install's certificate key to Apparition that's waiting for it nearby (Get Signing
/// there), as a section of Share Signing: the receivers Bonjour finds, an address for one it
/// can't, and the code comparison.
struct NearbySendSection: View {
    let signing: SigningStore
    let email: String
    let teamId: String?

    @State private var address = ""

    private var sender: NearbySender { signing.nearbySender }

    var body: some View {
        switch sender.phase {
        case .compare(let code, let peer):
            NearbyCompareSection(code: code, peer: peer) { matches in Task { await sender.confirm(matches) } }
        case .confirmed(let code, let peer):
            Section {
                NearbyCodeView(code: code)
                HStack(spacing: 12) {
                    ProgressView()
                    Text("Sent. Waiting for \(peer) to confirm the code…").foregroundStyle(.secondary)
                }
            }
        case .connecting(let peer):
            Section {
                HStack(spacing: 12) {
                    ProgressView()
                    Text("Connecting to \(peer)…").foregroundStyle(.secondary)
                }
                Button("Cancel", role: .cancel) { sender.reset() }
            } header: {
                Text("Nearby")
            }
        case .done(let peer, _):
            Section {
                Note(text: "\(peer) now signs with this \(ThisDevice.noun)'s certificate.", kind: .success)
                Button("Share with Another Device") { sender.reset() }
            } header: {
                Text("Nearby")
            }
        case .idle, .waiting, .failed:
            pickSection
        }
    }

    private var pickSection: some View {
        Section {
            ForEach(sender.peers) { peer in
                Button {
                    sender.send(to: peer, email: email, teamId: teamId)
                } label: {
                    Label(peer.name, systemImage: "dot.radiowaves.left.and.right")
                }
            }
            if sender.peers.isEmpty {
                HStack(spacing: 12) {
                    ProgressView()
                    Text("Looking for Apparition waiting for a key…").foregroundStyle(.secondary)
                }
            }
            if case .failed(let reason) = sender.phase {
                Note(text: reason, kind: .error)
            }
            HStack {
                TextField("Address it shows (192.168.1.20:52144)", text: $address)
                    .keyboardType(.numbersAndPunctuation)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .font(.body.monospaced())
                    .textFieldTapTarget()
                    .onSubmit(sendToAddress)
                Button("Send", action: sendToAddress)
                    .disabled(address.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            .focusRow()
        } header: {
            Text("Nearby")
        } footer: {
            Text("On the other device, open Apparition and choose Settings > Get Signing from Another Device (or Get It from Another Device where its signing stopped); on a computer, Signing > Receive from a Nearby Device, or `apparition signing receive`. It's listed here while it waits. Not listed (another network, Tailscale)? Type the address it shows.")
        }
    }

    private func sendToAddress() {
        guard !address.trimmingCharacters(in: .whitespaces).isEmpty else { return }
        sender.send(toAddress: address, email: email, teamId: teamId)
    }
}

/// Opens Get Signing in a panel: next to a full-slots error, where a key from another
/// Apparition means no new certificate is needed. `onImported` runs once a key arrived.
struct GetSigningButton: View {
    let signing: SigningStore
    let email: String?
    var onImported: (() -> Void)? = nil
    @State private var showing = false

    var body: some View {
        Button {
            showing = true
        } label: {
            Label("Get Signing from Another Device", systemImage: "square.and.arrow.down")
        }
        .buttonStyle(.bordered)
        .panel(isPresented: $showing) {
            ReceiveSigningSheet(signing: signing, email: email) { imported in
                showing = false
                if imported { onImported?() }
            }
        }
    }
}
