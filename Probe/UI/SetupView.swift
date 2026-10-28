import DebugTrace
import RAVEUI
import SwiftUI

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Setup")

/// Which of the four setup steps are done, read from the models (and the paired device saved
/// in UserDefaults by ProbeModel).
struct SetupStatus {
    let companion: Bool
    let paired: Bool
    let anisette: Bool
    let account: Bool

    @MainActor
    init(model: ProbeModel, account: AccountModel, pairedUDID: String?) {
        companion = model.routeChosen
        paired = model.hasPairingFile && pairedUDID != nil && model.pairingProblem == nil
        anisette = !account.anisetteURL.isEmpty
        // Signed in now, or able to sign in unattended with the remembered password (not while
        // that waits for the person, after a 2FA code went unentered).
        self.account = account.team != nil || (account.rememberPassword && Keychain.hasPassword && !account.autoSignInPaused)
    }

    /// The Connection step: how to reach the device, and the anisette server that goes with it
    /// (an imported certificate signs without an Apple ID, so it needs none).
    var connection: Bool { companion && (anisette || SigningStore.signsWithOwnCertificate) }

    static let stepCount = 3
    var doneCount: Int { [connection, paired, account].filter { $0 }.count }
    var complete: Bool { doneCount == Self.stepCount }

    /// Whether the window opens on the Library rather than Setup. At launch nobody is signed in
    /// yet (the session lives in memory), so an Apple ID signed in with before counts as done:
    /// the next install or refresh signs in again, or asks.
    @MainActor
    static func opensOnLibrary(model: ProbeModel, account: AccountModel) -> Bool {
        let status = SetupStatus(model: model, account: account,
                                 pairedUDID: UserDefaults.standard.string(forKey: "pairedUDID"))
        // An imported certificate signs without an Apple ID, so anisette and sign-in can wait.
        let signer = SigningStore.signsWithOwnCertificate || (status.anisette && (status.account || account.lastAppleID != nil))
        return status.companion && status.paired && signer
    }
}

/// First-run setup, also reachable later from its tab: companion, pairing, anisette, Apple ID.
struct SetupView: View {
    let model: ProbeModel
    let account: AccountModel
    let companions: CompanionFinder
    let onFinish: () -> Void

    @AppStorage("pairedUDID") private var pairedUDID: String?

    var body: some View {
        let status = SetupStatus(model: model, account: account, pairedUDID: pairedUDID)
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 24) {
                    RAVEScreenHeading("Setup")
                    intro(status)
                    StepPanel(number: 1, title: "Connection", systemImage: "macbook.and.visionpro", done: status.connection) {
                        ConnectionView(model: model, account: account, companions: companions)
                    }
                    StepPanel(number: 2, title: "Pair This \(ThisDevice.title)", systemImage: "link", done: status.paired,
                              blocked: status.companion ? nil : "Choose how Apparition connects first: pairing goes through it.") {
                        PairStep(model: model)
                    }
                    StepPanel(number: 3, title: "Apple ID", systemImage: "person.crop.circle", done: status.account,
                              blocked: status.anisette ? nil : "Choose an anisette server in the Connection step first: Apple's sign-in needs one.") {
                        AppleIDStep(account: account)
                    }
                    finish(status)
                }
                .padding(32)
                .frame(maxWidth: 960)
                .frame(maxWidth: .infinity)
            }
            .raveScreenTitle("Setup")
        }
    }

    private func intro(_ status: SetupStatus) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Apparition installs apps on this \(ThisDevice.noun), signed with your own Apple ID, and keeps them refreshed. Three things make that work.")
                .font(.title3)
                .foregroundStyle(.secondary)
            ProgressView(value: Double(status.doneCount), total: Double(SetupStatus.stepCount)) {
                Text("\(status.doneCount) of \(SetupStatus.stepCount) done").font(.callout.weight(.medium))
            }
            .frame(maxWidth: 360)
        }
    }

    private func finish(_ status: SetupStatus) -> some View {
        HStack {
            if status.complete {
                Note(text: "All set. Apps you install are signed for this \(ThisDevice.noun) and can be refreshed from the Library.", kind: .success)
            } else {
                Note(text: "You can come back to Setup at any time.")
            }
            Spacer()
            if status.complete {
                Button("Go to Library", action: onFinish)
                    .buttonStyle(.borderedProminent)
                    .controlSize(.large)
            } else {
                Button("Finish Later", action: onFinish)
                    .buttonStyle(.bordered)
                    .controlSize(.large)
            }
        }
        .focusRow()
    }
}

/// A step's title with its icon, or the title alone where there's no room for the icon.
private struct PanelTitleStyle: LabelStyle {
    let iconless: Bool

    func makeBody(configuration: Configuration) -> some View {
        if iconless {
            configuration.title
        } else {
            Label(configuration)
        }
    }
}

/// One numbered step: a header with its status, then its content.
private struct StepPanel<Content: View>: View {
    let number: Int
    let title: String
    let systemImage: String
    let done: Bool
    var blocked: String? = nil
    @ViewBuilder let content: Content
    @Environment(\.horizontalSizeClass) private var sizeClass

    var body: some View {
        Card(padding: 28) {
            VStack(alignment: .leading, spacing: 18) {
                HStack(spacing: 14) {
                    ZStack {
                        Circle().fill(done ? Color.green.opacity(0.25) : Color.white.opacity(0.1))
                        if done {
                            Image(systemName: "checkmark").font(.headline).foregroundStyle(.green)
                        } else {
                            Text("\(number)").font(.headline)
                        }
                    }
                    .frame(width: 40, height: 40)
                    Label(title, systemImage: systemImage)
                        .font(.title2.weight(.semibold))
                        // On a phone the icon leaves the title a few letters a line.
                        .labelStyle(PanelTitleStyle(iconless: sizeClass == .compact))
                        .lineLimit(2)
                    Spacer(minLength: 0)
                    if done {
                        StatusPill(text: "Done", systemImage: "checkmark.circle.fill", tint: .green)
                    } else if blocked != nil {
                        StatusPill(text: "Waiting", systemImage: "hourglass", tint: .secondary)
                    } else {
                        StatusPill(text: "To do", systemImage: "circle.dashed", tint: .orange)
                    }
                }
                if let blocked, !done {
                    Note(text: blocked, kind: .info)
                }
                content
            }
        }
    }
}

// MARK: - 2. Pairing

struct PairStep: View {
    let model: ProbeModel

    @AppStorage("pairedUDID") private var pairedUDID: String?
    @AppStorage("pairedName") private var pairedName: String?
    @State private var pin = ""
    /// What the field last received, for the submit log line (counts only, never the code).
    @State private var rawCharacters = 0
    @State private var rawScalars = 0
    @State private var submitted = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            if model.pairingProblem == .rejected {
                Note(text: "This \(ThisDevice.noun) no longer trusts Apparition's pairing, so installs, refreshes and JIT fail until you pair again.", kind: .warning)
            } else if model.hasPairingFile, pairedUDID != nil {
                Note(text: "Paired with \(pairedName ?? "this \(ThisDevice.noun)").", kind: .success)
            }
            #if os(visionOS)
            VStack(alignment: .leading, spacing: 8) {
                Text("1. Open Settings > General > Remote Devices in a window beside this one, and leave it open.")
                Text("2. Start pairing here. Settings shows a 6-digit code; type it below.")
                Text("3. Apparition then reads this \(ThisDevice.noun)'s identity through the tunnel to confirm.")
            }
            .foregroundStyle(.secondary)

            serviceStatus
            #elseif os(tvOS)
            // Settings' pairing screen closes when Apparition comes back to the front, so a
            // computer pairs instead and hands Apparition its own record when it installs it.
            VStack(alignment: .leading, spacing: 8) {
                Text("1. Open Settings > Remotes and Devices > Remote App and Devices on this Apple TV.")
                Text("2. On a computer on the same network, open the Apparition desktop app (or run apparition pair, then apparition install). The Apple TV shows a code twice: once for the computer, once for Apparition.")
                Text("3. Come back here and confirm the identity.")
            }
            .foregroundStyle(.secondary)
            if !model.hasPairingFile {
                Note(text: "Not paired yet. Installing Apparition from a computer pairs it; for the copy already here, run apparition pair, then apparition pairing hand-off.", kind: .info)
            }
            #else
            // iPhone and iPad have no Remote Devices screen to pair from: a computer pairs over
            // usbmuxd and hands Apparition its record, when it installs it or on its own.
            VStack(alignment: .leading, spacing: 8) {
                Text("1. On a computer, open the Apparition desktop app and pair, or run apparition pair --usb. The first time, connect this \(ThisDevice.noun) with a cable and tap Trust; once it trusts the computer, Wi-Fi is enough.")
                Text("2. Give Apparition the pairing: installing Apparition from the computer does it, or run apparition pairing hand-off for the copy already here.")
                Text("3. Confirm the identity here.")
                Text("Apparition keeps a copy of the pairing in this \(ThisDevice.noun)'s Keychain, so reinstalling Apparition doesn't lose it.")
                    .font(.callout)
            }
            .foregroundStyle(.secondary)
            if !model.hasPairingFile {
                Note(text: "Not paired yet: installs, refreshes and JIT need the pairing from a computer (steps 1 and 2).", kind: .info)
            }
            #endif

            HStack(spacing: 12) {
                #if os(visionOS)
                Button {
                    guard let port = model.selfManualPairingPort else { return }
                    submitted = false
                    pin = ""
                    log.info("pairing through the \(model.connectionMode?.rawValue ?? "default", privacy: .public) connection, port \(port, privacy: .public)")
                    Task {
                        await model.pair(host: model.defaultTunnelHost, port: port)
                        if model.latestAttempt("pair")?.outcome == "ok" {
                            await model.identify(host: model.defaultTunnelHost, port: model.defaultTunnelPort)
                        }
                    }
                } label: {
                    Label(model.hasPairingFile ? "Pair Again" : "Start Pairing", systemImage: "link.badge.plus")
                }
                .buttonStyle(.borderedProminent)
                .disabled(model.busy || !model.routeChosen || model.selfManualPairingPort == nil)
                #endif

                Button("Confirm Identity") {
                    Task { await model.identify(host: model.defaultTunnelHost, port: model.defaultTunnelPort) }
                }
                .buttonStyle(.bordered)
                .disabled(model.busy || !model.routeChosen || !model.hasPairingFile)
                .help("Open a tunnel and read this \(ThisDevice.noun)'s name and UDID, which signing needs")

                if model.busy { ProgressView() }
            }
            .focusRow()

            if model.pinRequested {
                pinEntry
            }

            attemptNote("pair", label: "Pairing")
            attemptNote("identify", label: "Identity check")
        }
        .onAppear {
            if model.advertised.isEmpty { model.startBrowsing() }
        }
        .onChange(of: model.pinRequested) { _, requested in
            if requested { submitted = false; pin = "" }
        }
    }

    @ViewBuilder
    private var serviceStatus: some View {
        if let port = model.selfManualPairingPort {
            Note(text: "Found this \(ThisDevice.noun)'s pairing service (port \(port)).", kind: .success)
        } else {
            HStack(spacing: 12) {
                Note(text: "Looking for the pairing service. It appears while Remote Devices is open in Settings.", kind: .info)
                Button("Look Again") { model.startBrowsing() }
                    .buttonStyle(.borderless)
            }
            .focusRow()
        }
    }

    private var pinEntry: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Enter the code Settings shows").font(.headline)
            HStack(alignment: .top, spacing: 16) {
                DigitCodeField(title: "000000", code: $pin, contentType: nil,
                               onRawInput: { rawCharacters = $0; rawScalars = $1 },
                               onComplete: { submit($0) })
                Button("Pair") { submit(pin) }
                    .buttonStyle(.borderedProminent)
                    .disabled(pin.count != 6 || submitted)
                Button("Cancel", role: .cancel) {
                    model.cancelPin()
                    pin = ""
                }
                .buttonStyle(.bordered)
            }
            .focusRow()
        }
        .padding(18)
        .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
    }

    private func submit(_ code: String) {
        let digits = DigitCodeField.digits(in: code, length: 6)
        log.info("pin submit: \(digits.count, privacy: .public) digits; field held \(rawCharacters, privacy: .public) characters, \(rawScalars, privacy: .public) scalars")
        guard digits.count == 6, !submitted else { return }
        submitted = true
        model.submitPin(digits)
        pin = ""
    }

    @ViewBuilder
    private func attemptNote(_ action: String, label: String) -> some View {
        if let attempt = model.latestAttempt(action) {
            switch attempt.outcome {
            case nil: Note(text: "\(label) in progress…")
            case "ok": Note(text: "\(label) succeeded\(attempt.peer.map { ": \($0.name)" } ?? "").", kind: .success)
            case let outcome?: Note(text: "\(label) failed: \(outcome)", kind: .error)
            }
        }
    }
}

// MARK: - 4. Apple ID

struct AppleIDStep: View {
    let account: AccountModel

    @State private var email = ""
    @State private var password = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            if let team = account.team {
                Note(text: "Signed in\(account.email.map { " as \($0)" } ?? ""), team \(team.name ?? team.id).", kind: .success)
            } else {
                if account.rememberPassword && Keychain.hasPassword {
                    if account.autoSignInPaused {
                        Note(text: "Refreshes are waiting for you: Apple asked for a two-factor code when Apparition last signed in on its own. Sign in with the remembered password to enter one.", kind: .warning)
                        Button("Sign In with Remembered Password") {
                            Task { await account.signInWithRememberedPassword(byPerson: true) }
                        }
                        .buttonStyle(.borderedProminent)
                        .disabled(account.busy || account.anisetteURL.isEmpty)
                    } else {
                        Note(text: "Your password is remembered on this \(ThisDevice.noun), so refreshes sign in on their own. Sign in now to check it.", kind: .success)
                    }
                }
                form
            }
            if let request = account.twoFactor {
                TwoFactorPrompt(account: account, request: request)
            }
            if account.busy && account.twoFactor == nil {
                HStack(spacing: 12) {
                    ProgressView()
                    Text(account.downloadProgress ?? "Signing in…").foregroundStyle(.secondary)
                }
            }
            if let error = account.lastError {
                Note(text: error, kind: .error)
            }
            Note(text: "Apple IDs protected with security keys can't sign in here: Apple then asks for the key, which Apparition can't answer on the \(ThisDevice.noun). Use an Apple ID without security keys (a free one works).")
        }
        .onAppear { if email.isEmpty { email = account.lastAppleID ?? "" } }
        .hostsTwoFactorPrompt(account)
    }

    private var form: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Apps are signed with your own Apple ID, as Xcode would. Your password goes straight to Apple. A free Apple ID works; its apps need refreshing every 7 days.")
                .foregroundStyle(.secondary)
            TextField("Apple ID (email)", text: $email)
                .textContentType(.username)
                .keyboardType(.emailAddress)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .roundedFieldStyle()
                .frame(maxWidth: 420)
            SecureField("Password", text: $password)
                .textContentType(.password)
                .roundedFieldStyle()
                .frame(maxWidth: 420)
                .onSubmit(signIn)
            Toggle(isOn: Binding(get: { account.rememberPassword }, set: { account.rememberPassword = $0 })) {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Remember password")
                    Text("Kept only in this \(ThisDevice.noun)'s Keychain (never synced), so refreshes can sign in with nobody present. Turning this off deletes it.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
            }
            .frame(maxWidth: 620)
            Button("Sign In", action: signIn)
                .buttonStyle(.borderedProminent)
                .disabled(account.busy || email.isEmpty || password.isEmpty || account.anisetteURL.isEmpty)
        }
    }

    /// The password goes from the field into AccountModel.signIn and nowhere else.
    private func signIn() {
        guard !account.busy, !email.isEmpty, !password.isEmpty else { return }
        let (address, secret) = (email.trimmingCharacters(in: .whitespaces), password)
        password = ""
        Task { await account.signIn(email: address, password: secret) }
    }
}
