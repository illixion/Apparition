import CoreImage.CIFilterBuiltins
import RAVECompanion
import RAVEDebugAccess
import RAVEUI
import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

/// Settings > Developer > Debugging from a Computer. Off until the person turns it on, and
/// asked each time for how long (DebugAccess.swift).
struct DebugAccessView: View {
    @Bindable var model: DebugAccessModel

    @State private var choosingDuration = false
    @State private var pairing = false
    @State private var confirmNewPassword = false
    @State private var removing: RAVEDebugComputer?

    var body: some View {
        Form {
            RAVEScreenHeading("Debugging from a Computer")
            accessSection
            computersSection
            if !model.launchServer {
                passwordSection
                privacySection
            }
        }
        .raveScreenTitle("Debugging from a Computer")
        .panel(isPresented: $pairing) {
            PairComputerSheet(model: model) { pairing = false }
        }
        .onAppear {
            model.checkDeadline()
            model.loadPassword()
        }
    }

    // MARK: On and off

    private var accessSection: some View {
        Section {
            Toggle("Allow debugging from a computer", isOn: Binding(
                get: { model.isOn },
                set: { on in
                    if on { choosingDuration = true } else { model.turnOff() }
                }))
            .confirmationDialog("Allow debugging for how long?", isPresented: $choosingDuration, titleVisibility: .visible) {
                Button("For an Hour") { model.turnOn(.hour) }
                Button("Until I Turn It Off") { model.turnOn(.forever) }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("An hour ends by the clock, even if Apparition is closed. Until you turn it off, it starts again every time Apparition opens.")
            }
            switch model.status {
            case .off:
                EmptyView()
            case .starting:
                LabeledContent("Status", value: "Starting…")
            case .failed(let reason):
                Text(reason).foregroundStyle(.red).focusableText()
            case .on(let until):
                if let until {
                    TimelineView(.periodic(from: .now, by: 15)) { _ in
                        LabeledContent("Stops in", value: Self.timeLeft(until))
                    }
                } else {
                    LabeledContent("Stops", value: "When you turn it off")
                }
                if let port = model.pairedPort {
                    LabeledContent("Paired computers connect on port", value: String(port))
                }
                Button("Stop Now", role: .destructive) { model.turnOff() }
            }
        } footer: {
            if model.launchServer {
                Text("This copy was built for development, so its own debug server already runs with the build's token. Turning this on adds the listener for paired computers beside it.")
            } else {
                Text("Lets a computer you paired read Apparition's logs and state and run its debug commands, to find a problem or work on Apparition itself. The connection is encrypted with a key only that computer holds. Code changes and screen captures still ask here each time. Nothing listens while this is off.")
            }
        }
    }

    static func timeLeft(_ until: Date) -> String {
        let seconds = max(Int(until.timeIntervalSinceNow.rounded(.up)), 0)
        if seconds < 60 { return "less than a minute" }
        let minutes = (seconds + 59) / 60
        return minutes == 1 ? "1 minute" : "\(minutes) minutes"
    }

    // MARK: Paired computers

    private var computersSection: some View {
        Section {
            ForEach(model.access.computers) { computer in
                LabeledContent {
                    Button("Remove", role: .destructive) { removing = computer }
                } label: {
                    Text(computer.name)
                    Text("Paired \(computer.pairedAt.formatted(date: .abbreviated, time: .shortened))")
                }
            }
            Button {
                pairing = true
            } label: {
                Label("Pair a Computer…", systemImage: "laptopcomputer.and.arrow.down")
            }
        } header: {
            Text("Paired Computers")
        } footer: {
            Text(model.access.computers.isEmpty
                 ? "Pairing shows six digits here and on the computer. If they match, you allow it here, and that computer gets a key of its own."
                 : "Removing a computer takes its key away at once. The others keep working.")
        }
        .confirmationDialog("Remove \(removing?.name ?? "this computer")?",
                            isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } }),
                            titleVisibility: .visible) {
            Button("Remove", role: .destructive) {
                if let removing { model.access.remove(removing.id) }
                removing = nil
            }
        } message: {
            Text("It can no longer debug Apparition on this \(ThisDevice.noun). To let it back in, pair it again.")
        }
    }

    // MARK: Password (advanced)

    private var passwordSection: some View {
        Section {
            Toggle("Also allow a password", isOn: $model.passwordAccess)
            if model.passwordAccess, let password = model.password {
                LabeledContent("Password") {
                    Text(password).font(.caption.monospaced()).selectableText()
                }
                #if !os(tvOS)
                Button {
                    UIPasteboard.general.string = password
                } label: {
                    Label("Copy Password", systemImage: "doc.on.doc")
                }
                #endif
                if let code = QRCode.image(password) {
                    HStack {
                        Spacer()
                        Image(decorative: code, scale: 1)
                            .interpolation(.none)
                            .resizable()
                            .frame(width: 180, height: 180)
                            .padding(10)
                            .background(.white, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                            .accessibilityLabel("The password as a QR code")
                        Spacer()
                    }
                }
                if let url = model.passwordURL {
                    LabeledContent("Address") {
                        Text(url).font(.caption.monospaced()).selectableText()
                    }
                }
                Button("New Password", role: .destructive) { confirmNewPassword = true }
                    .confirmationDialog("Make a new password?", isPresented: $confirmNewPassword, titleVisibility: .visible) {
                        Button("New Password", role: .destructive) { model.newPassword() }
                    } message: {
                        Text("The old one stops working at once, and every computer you allowed with it is asked again.")
                    }
            }
        } header: {
            Text("Advanced")
        } footer: {
            Text("For tools that can't pair. It opens a second listener that takes this password and is not encrypted: on Wi-Fi, anyone on the network can capture the password and everything sent with it. Use it only over Tailscale or another VPN. Each computer that uses it is asked about here once. It listens only while debugging from a computer is on.")
        }
    }

    // MARK: Privacy

    private var privacySection: some View {
        Section {
            Toggle("Share personal detail with connected computers", isOn: $model.sharesPersonalDetail)
        } header: {
            Text("Privacy")
        } footer: {
            Text("Off, a connected computer gets what a released app gives: Apparition's own log, where private values are always left out, and the parts of its state that hold nothing personal. On adds the system's log of Apparition, debug-level lines, screenshots and the screen's text (each still asked for), and full snapshots of its state. A debug command a computer calls by name answers in full either way, so pair only computers you trust. Tools on the computer may send what they read to a cloud model. Problem reports you send are redacted whatever this says.")
        }
    }
}

/// Pairing one computer: the pairing listener is up only while this is on screen.
private struct PairComputerSheet: View {
    let model: DebugAccessModel
    let onDone: () -> Void

    private var access: RAVEDebugAccess { model.access }

    var body: some View {
        NavigationStack {
            Form {
                RAVEScreenHeading("Pair a Computer")
                switch access.pairing {
                case .idle, .waiting:
                    waiting
                case .confirming(let code, let computer):
                    confirming(code: code, computer: computer)
                case .paired(let computer):
                    Section {
                        Label("Paired with \(computer)", systemImage: "checkmark.circle.fill")
                            .foregroundStyle(.green)
                        if !model.isOn {
                            Text("Debugging from a computer is off, so it can't connect yet. Turn it on in the screen behind this one.")
                                .foregroundStyle(.secondary)
                                .focusableText()
                        }
                    }
                case .failed(let reason):
                    Section {
                        Text(reason).foregroundStyle(.red).focusableText()
                        Button("Try Again") {
                            access.endPairing()
                            access.beginPairing()
                        }
                    }
                }
            }
            .raveScreenTitle("Pair a Computer")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done", action: onDone)
                }
            }
        }
        .sheetFrame(minWidth: 560, minHeight: 480)
        .onAppear { access.beginPairing() }
        .onDisappear { access.endPairing() }
    }

    @ViewBuilder
    private var waiting: some View {
        Section {
            HStack {
                ProgressView()
                Text("Waiting for a computer…")
            }
            Text("On the computer, from a checkout of DebugTrace with RAVESDK beside it, run:")
                .focusableText()
            Text("Tools/debugtrace-mcp pair").font(.callout.monospaced()).selectableText()
        } footer: {
            Text("It finds this \(ThisDevice.noun) on the local network and shows six digits. The same digits appear here, and nothing is paired until you allow it.")
        }
        if let port = access.pairingPort, let address = model.addresses.first {
            Section {
                Text("Tools/debugtrace-mcp pair --host \(address)\(port == RAVECompanionNamespace.debugAccess.defaultPairPort ? "" : " --port \(port)")")
                    .font(.callout.monospaced())
                    .selectableText()
            } header: {
                Text("If It Finds Nothing")
            } footer: {
                Text("Give it this \(ThisDevice.noun)'s address. Over Tailscale, use its name or address there instead.")
            }
        }
    }

    private func confirming(code: String, computer: String) -> some View {
        Section {
            VStack(spacing: 12) {
                Text(code)
                    .font(.system(size: 54, weight: .semibold, design: .monospaced))
                    .accessibilityLabel(code.map(String.init).joined(separator: " "))
                Text("\(computer) wants to debug Apparition.")
                    .multilineTextAlignment(.center)
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, 8)
            Button("Allow") { access.answer(true) }
            Button("Don't Allow", role: .destructive) { access.answer(false) }
        } footer: {
            Text("Allow only if the computer shows the same six digits. That computer can then read Apparition's logs and state and run its debug commands whenever debugging from a computer is on, until you remove it.")
        }
    }
}

enum QRCode {
    /// `text` as a QR code, one pixel per module, for a view to scale up without smoothing.
    static func image(_ text: String) -> CGImage? {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(text.utf8)
        filter.correctionLevel = "M"
        guard let output = filter.outputImage else { return nil }
        return CIContext().createCGImage(output, from: output.extent)
    }
}
