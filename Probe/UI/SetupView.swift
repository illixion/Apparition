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
        companion = model.relay != nil
        paired = model.hasPairingFile && pairedUDID != nil
        anisette = !account.anisetteURL.isEmpty
        // Signed in now, or able to sign in unattended with the remembered password.
        self.account = account.team != nil || (account.rememberPassword && Keychain.hasPassword)
    }

    var doneCount: Int { [companion, paired, anisette, account].filter { $0 }.count }
    var complete: Bool { doneCount == 4 }

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
                    RAVEScreenHeading("Setup", takesFocus: false)
                    intro(status)
                    StepPanel(number: 1, title: "Companion", systemImage: "macbook.and.visionpro", done: status.companion) {
                        CompanionStep(model: model, account: account, companions: companions)
                    }
                    StepPanel(number: 2, title: "Pair This \(ThisDevice.title)", systemImage: "link", done: status.paired,
                              blocked: status.companion ? nil : "Set up the companion first: pairing goes through it.") {
                        PairStep(model: model)
                    }
                    StepPanel(number: 3, title: "Anisette Server", systemImage: "server.rack", done: status.anisette) {
                        AnisetteStep(account: account, companions: companions)
                    }
                    StepPanel(number: 4, title: "Apple ID", systemImage: "person.crop.circle", done: status.account,
                              blocked: status.anisette ? nil : "Choose an anisette server first: Apple's sign-in needs one.") {
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
            Text("Apparition installs apps on this \(ThisDevice.noun), signed with your own Apple ID, and keeps them refreshed. Four things make that work.")
                .font(.title3)
                .foregroundStyle(.secondary)
            ProgressView(value: Double(status.doneCount), total: 4) {
                Text("\(status.doneCount) of 4 done").font(.callout.weight(.medium))
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

/// One numbered step: a header with its status, then its content.
private struct StepPanel<Content: View>: View {
    let number: Int
    let title: String
    let systemImage: String
    let done: Bool
    var blocked: String? = nil
    @ViewBuilder let content: Content

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
                    Spacer()
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

// MARK: - 1. Companion

struct CompanionStep: View {
    let model: ProbeModel
    let account: AccountModel
    let companions: CompanionFinder

    @State private var relayDraft = ""
    @State private var checking = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("This \(ThisDevice.noun) can't reach its own developer services directly, so Apparition goes through Apparition Companion: a small relay running on your Mac (the menu-bar app) or on Windows or Linux (the command-line companion). It only carries encrypted traffic and never sees keys.")
                .foregroundStyle(.secondary)

            HStack(spacing: 12) {
                Button {
                    Task { await companions.search() }
                } label: {
                    Label(companions.searching ? "Searching…" : "Find Companion", systemImage: "dot.radiowaves.left.and.right")
                }
                .buttonStyle(.borderedProminent)
                .disabled(companions.searching)
                if companions.searching { ProgressView() }
            }
            .focusRow()

            ForEach(companions.found) { companion in
                companionRow(companion)
            }

            VStack(alignment: .leading, spacing: 8) {
                Text("Or type its address").font(.headline)
                HStack(spacing: 12) {
                    TextField("100.x.y.z:49160", text: $relayDraft)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .font(.body.monospaced())
                        .roundedFieldStyle()
                        .frame(maxWidth: 340)
                        .onSubmit(useDraft)
                    Button("Use and Check", action: useDraft)
                        .buttonStyle(.bordered)
                        .disabled(relayDraft.trimmingCharacters(in: .whitespaces).isEmpty || checking)
                    if checking { ProgressView() }
                }
                .focusRow()
                Text("Over Tailscale there is no Bonjour, so enter the companion's Tailscale address with port 49160.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }

            if let relay = model.relay {
                HStack(spacing: 12) {
                    if companions.found.contains(where: { $0.relay == relay }) {
                        Note(text: "Using \(relay), and it answered.", kind: .success)
                    } else {
                        Note(text: "Using \(relay).", kind: .info)
                    }
                    Button("Check") {
                        checking = true
                        Task { _ = await companions.check(relay: relay); checking = false }
                    }
                    .buttonStyle(.borderless)
                    .disabled(checking)
                }
                .focusRow()
            }
            if let error = companions.lastError {
                Note(text: error, kind: .warning)
            }
        }
        .onAppear { relayDraft = model.relay ?? "" }
    }

    private func useDraft() {
        let relay = relayDraft.trimmingCharacters(in: .whitespaces)
        guard !relay.isEmpty else { return }
        model.relay = relay
        checking = true
        Task { _ = await companions.check(relay: relay); checking = false }
    }

    private func companionRow(_ companion: CompanionFinder.Companion) -> some View {
        HStack(spacing: 14) {
            Image(systemName: "desktopcomputer")
                .font(.title2)
                .frame(width: 44)
            VStack(alignment: .leading, spacing: 2) {
                Text(companion.name).font(.headline)
                Text(companion.relay + (companion.anisetteURL != nil ? " · runs an anisette server" : ""))
                    .font(.callout.monospaced())
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if model.relay == companion.relay {
                StatusPill(text: "Relay", systemImage: "checkmark", tint: .green)
            } else {
                Button("Use as Relay") {
                    model.relay = companion.relay
                    relayDraft = companion.relay
                }
                .buttonStyle(.bordered)
            }
        }
        .focusRow()
        .padding(14)
        .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 18, style: .continuous))
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
            if model.hasPairingFile, pairedUDID != nil {
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
                Text("2. On a computer on the same network, run Apparition Installer: apparition-installer pair, then install. The Apple TV shows a code twice: once for the computer, once for Apparition.")
                Text("3. Come back here and confirm the identity.")
            }
            .foregroundStyle(.secondary)
            if !model.hasPairingFile {
                Note(text: "Not paired yet. Install Apparition from a computer with Apparition Installer to pair it.", kind: .info)
            }
            #else
            // iPhone and iPad have no Remote Devices screen to pair from: a computer pairs over
            // the cable and hands Apparition its record when it installs it.
            VStack(alignment: .leading, spacing: 8) {
                Text("1. Connect this \(ThisDevice.noun) to a computer with a cable and tap Trust if it asks.")
                Text("2. On the computer, run Apparition Installer: apparition-installer pair --usb, then install. Installing Apparition gives it the pairing.")
                Text("3. Confirm the identity here; after that the cable isn't needed.")
            }
            .foregroundStyle(.secondary)
            if !model.hasPairingFile {
                Note(text: "Not paired yet. Install Apparition from a computer with Apparition Installer to pair it.", kind: .info)
            }
            #endif

            HStack(spacing: 12) {
                #if os(visionOS)
                Button {
                    guard let port = model.selfManualPairingPort else { return }
                    submitted = false
                    pin = ""
                    log.info("pairing through the relay, port \(port, privacy: .public)")
                    Task {
                        await model.pair(host: ProbeModel.relayHost, port: port)
                        if model.latestAttempt("pair")?.outcome == "ok" {
                            await model.identify(host: ProbeModel.relayHost, port: model.defaultTunnelPort)
                        }
                    }
                } label: {
                    Label(model.hasPairingFile ? "Pair Again" : "Start Pairing", systemImage: "link.badge.plus")
                }
                .buttonStyle(.borderedProminent)
                .disabled(model.busy || model.relay == nil || model.selfManualPairingPort == nil)
                #endif

                Button("Confirm Identity") {
                    Task { await model.identify(host: ProbeModel.relayHost, port: model.defaultTunnelPort) }
                }
                .buttonStyle(.bordered)
                .disabled(model.busy || model.relay == nil || !model.hasPairingFile)
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

// MARK: - 3. Anisette

struct AnisetteStep: View {
    let account: AccountModel
    let companions: CompanionFinder

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Signing in to Apple needs an anisette server (v3). Apparition doesn't pick one for you: use the one your companion runs, any v3 server you trust, or your own. The server sees this \(ThisDevice.noun)'s anisette identity and IP address, never your Apple ID or password.")
                .foregroundStyle(.secondary)

            #if os(tvOS)
            // One menu instead of a row per companion: rows of text and buttons made the step
            // long enough that moving between them scrolled a whole screen.
            let offered = companions.found.filter { $0.anisetteURL != nil }
            if !offered.isEmpty {
                Menu {
                    ForEach(offered) { companion in
                        Button("\(companion.name): \(companion.anisetteURL ?? "")") {
                            account.anisetteURL = companion.anisetteURL ?? ""
                        }
                    }
                } label: {
                    let current = offered.first { $0.anisetteURL == account.anisetteURL }
                    Label(current.map { "Using \($0.name)'s server" } ?? "Use a Companion's Server", systemImage: "desktopcomputer")
                }
                .focusRow()
            }
            #else
            ForEach(companions.found.filter { $0.anisetteURL != nil }) { companion in
                if let url = companion.anisetteURL {
                    HStack(spacing: 12) {
                        Image(systemName: "desktopcomputer")
                        Text("\(companion.name) runs one at \(url)")
                            .font(.callout.monospaced())
                        Spacer()
                        if account.anisetteURL == url {
                            StatusPill(text: "In use", systemImage: "checkmark", tint: .green)
                        } else {
                            Button("Use Companion's Anisette") { account.anisetteURL = url }
                                .buttonStyle(.bordered)
                        }
                    }
                    .focusRow()
                }
            }

            #endif

            TextField("http://your-server:6969", text: Binding(get: { account.anisetteURL },
                                                              set: { account.anisetteURL = $0.trimmingCharacters(in: .whitespaces) }))
                .keyboardType(.URL)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .font(.body.monospaced())
                .roundedFieldStyle()
                .frame(maxWidth: 480)

            Disclosure("Host your own") {
                VStack(alignment: .leading, spacing: 8) {
                    Text("Apparition Companion can run one with your Docker (Docker Desktop, OrbStack or Colima): choose Start in its menu, then Find Companion above and use it.")
                    Text("Or, on any machine with Docker, from the Apparition repository:")
                    Text("docker compose -f selfhost/anisette/compose.yaml up -d")
                        .font(.callout.monospaced())
                        .selectableText()
                        .padding(10)
                        .background(.black.opacity(0.25), in: RoundedRectangle(cornerRadius: 10))
                    Text("and enter http://<that machine>:6969 here.")
                }
                .font(.callout)
                .foregroundStyle(.secondary)
                .padding(.top, 8)
            }
        }
    }
}

// MARK: - 4. Apple ID

struct AppleIDStep: View {
    let account: AccountModel

    @State private var email = ""
    @State private var password = ""
    @State private var code = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            if let team = account.team {
                Note(text: "Signed in\(account.email.map { " as \($0)" } ?? ""), team \(team.name ?? team.id).", kind: .success)
            } else {
                if account.rememberPassword && Keychain.hasPassword {
                    Note(text: "Your password is remembered on this \(ThisDevice.noun), so refreshes sign in on their own. Sign in now to check it.", kind: .success)
                }
                form
            }
            if let request = account.twoFactor {
                twoFactor(request)
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

    private func twoFactor(_ request: AccountModel.TwoFactorRequest) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(request.sms ? "Enter the code Apple texted you" : "Enter the code shown on your other Apple devices")
                .font(.headline)
            if let error = request.lastError {
                Note(text: error, kind: .error)
            }
            HStack(alignment: .top, spacing: 16) {
                DigitCodeField(title: "000000", code: $code) { digits in
                    account.answerTwoFactor("code:" + digits)
                    code = ""
                }
                Button("Verify") {
                    account.answerTwoFactor("code:" + code)
                    code = ""
                }
                .buttonStyle(.borderedProminent)
                .disabled(code.count != 6)
            }
            .focusRow()
            HStack(spacing: 12) {
                if request.sms {
                    Button("Text Again") { account.answerTwoFactor("resend") }
                    Button("Use My Devices") { account.answerTwoFactor("devices") }
                } else {
                    Button("Send to Devices Again") { account.answerTwoFactor("devices") }
                }
                if !request.numbers.isEmpty {
                    Menu("Text a Code To…") {
                        ForEach(request.numbers) { number in
                            Button(number.number) { account.answerTwoFactor("sms:\(number.id)") }
                        }
                    }
                }
                Button("Cancel", role: .cancel) { account.answerTwoFactor("abort") }
            }
            .focusRow()
            .buttonStyle(.bordered)
        }
        .padding(18)
        .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 20, style: .continuous))
    }
}
