import SwiftUI

/// The developer probe that was the whole UI before the app had screens: every step by hand,
/// Bonjour results, reachability, raw attempts and signing. Hidden behind Settings > Advanced.
struct DiagnosticsView: View {
    let model: ProbeModel
    let account: AccountModel
    let library: LibraryModel
    let companions: CompanionFinder
    @State private var sourceURL = "github:utmapp/UTM"
    @State private var pin = ""
    @State private var email = ""
    @State private var password = ""
    @State private var twoFactorCode = ""
    @State private var ipaURL = "https://github.com/utmapp/UTM/releases/download/v5.0.6/UTM-visionOS.ipa"

    var body: some View {
        NavigationStack {
            List {
                Section("Steps") {
                    Text("1. Open Settings > General > Remote Devices in a window beside this one.")
                    Text("2. Pair against the manual-pairing service on the device marked \"this device\". Type the code Settings shows.")
                    Text("3. Then open a tunnel against the paired service (usually port 49152).")
                }
                .font(.footnote)

                if model.pinRequested {
                    Section("Code from Settings") {
                        TextField("6-digit code", text: $pin)
                            .keyboardType(.numberPad)
                        HStack {
                            Button("Submit") { model.submitPin(DigitCodeField.digits(in: pin, length: 6)); pin = "" }
                            Button("Cancel", role: .cancel) { model.cancelPin() }
                        }
                    }
                }

                accountSection
                librarySection

                Section("Advertised services") {
                    if model.advertised.isEmpty { Text("Nothing yet").foregroundStyle(.secondary) }
                    ForEach(model.advertised.values.sorted { $0.id < $1.id }) { service in
                        serviceRow(service)
                    }
                }

                Section("Reachability") {
                    Button("Scan all candidates") { Task { await model.scan() } }
                        .disabled(model.busy)
                    ForEach(Array(model.reachability.enumerated()), id: \.offset) { _, row in
                        Text("\(row.host):\(row.port)  \(row.result)  \(row.elapsedMs) ms")
                            .font(.caption.monospaced())
                    }
                }

                Section("Attempts") {
                    ForEach(Array(model.attempts.enumerated()), id: \.offset) { _, attempt in
                        VStack(alignment: .leading, spacing: 4) {
                            Text("\(attempt.action) \(attempt.host):\(attempt.port)").font(.headline)
                            Text(attempt.outcome ?? "running…").font(.caption.monospaced())
                            if let peer = attempt.peer { Text("peer: \(peer.name) \(peer.model)").font(.caption) }
                            if let services = attempt.services {
                                Text("\(services.count) RSD services").font(.caption)
                                ForEach(services, id: \.name) { service in
                                    Text("\(service.name) :\(service.port)").font(.caption2.monospaced())
                                }
                            }
                        }
                    }
                }

                Section("This device") {
                    Text(model.hasPairingFile ? "Pairing file saved" : "No pairing file")
                    if model.hasPairingFile {
                        Button("Forget pairing", role: .destructive) { model.forgetPairing() }
                    }
                    ForEach(model.interfaces, id: \.self) { Text($0).font(.caption.monospaced()) }
                }
            }
            .navigationTitle("Diagnostics")
            .toolbar {
                Button("Browse") { model.startBrowsing() }
            }
        }
        .onAppear { model.startBrowsing() }
    }

    @ViewBuilder
    private var librarySection: some View {
        Section("Library (spike 5)") {
            TextField("Companion relay (ip:49160), empty for LocalDevVPN",
                      text: Binding(get: { model.relay ?? "" },
                                    set: { model.relay = $0.trimmingCharacters(in: .whitespaces).isEmpty ? nil : $0 }))
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .font(.caption.monospaced())
            HStack {
                Button(companions.searching ? "Searching…" : "Find companion") { Task { await companions.search() } }
                    .disabled(companions.searching)
                if let relay = model.relay {
                    Button("Check \(relay)") { Task { await companions.check(relay: relay) } }
                }
            }
            ForEach(companions.found) { companion in
                VStack(alignment: .leading, spacing: 4) {
                    Text("\(companion.name) at \(companion.relay)").font(.caption)
                    HStack {
                        if model.relay != companion.relay {
                            Button("Use as relay") { model.relay = companion.relay }
                        }
                        // The user's own server, used only when they choose it here.
                        if let anisette = companion.anisetteURL, account.anisetteURL != anisette {
                            Button("Use its anisette server") { account.anisetteURL = anisette }
                        }
                    }
                    .buttonStyle(.borderless)
                }
            }
            if let error = companions.lastError { Text(error).font(.caption).foregroundStyle(.secondary) }
            ForEach(library.installed) { app in
                VStack(alignment: .leading) {
                    Text("\(app.name) \(app.version)")
                    Text("expires \(app.expiresAt ?? "?")").font(.caption)
                    if let error = app.lastError { Text(error).font(.caption).foregroundStyle(.red) }
                }
            }
            Button("Refresh all now") { Task { await library.refresh(force: true, trigger: "button") } }
                .disabled(library.busy || library.installed.isEmpty)
            ForEach(library.sources) { source in
                ForEach(source.apps) { app in
                    Button("Install \(app.name) \(app.version)") {
                        Task { await library.install(sourceId: source.id, appId: app.id, stripExtensions: true) }
                    }
                    .disabled(library.busy)
                }
                if let error = source.error { Text(error).font(.caption).foregroundStyle(.red) }
            }
            HStack {
                TextField("Source URL or github:owner/repo", text: $sourceURL)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                Button("Add") {
                    let url = sourceURL
                    let pattern = url.hasPrefix("github:") ? ThisDevice.utmAssetPattern : nil
                    Task { await library.addSource(url, assetPattern: pattern, prereleases: true) }
                }
            }
            if let status = library.status { Text(status).font(.caption) }
        }
    }

    @ViewBuilder
    private var accountSection: some View {
        Section("Apple ID (spike 2)") {
            if let team = account.team {
                Text("Signed in: team \(team.name ?? "?") (\(team.id))")
            } else {
                TextField("Apple ID", text: $email)
                    .textContentType(.username)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                SecureField("Password", text: $password)
                    .textContentType(.password)
                Toggle("Remember password on this device (for automatic refresh)",
                       isOn: Binding(get: { account.rememberPassword }, set: { account.rememberPassword = $0 }))
                TextField("Anisette server URL (v3), e.g. http://your-server:6969", text: Binding(get: { account.anisetteURL }, set: { account.anisetteURL = $0 }))
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .font(.caption.monospaced())
                Button("Sign in") {
                    let (address, secret) = (email, password)
                    password = ""
                    Task { await account.signIn(email: address, password: secret) }
                }
                .disabled(account.busy || email.isEmpty || password.isEmpty)
            }
            if let request = account.twoFactor {
                Text(request.sms ? "Enter the code sent by SMS" : "Enter the code shown on your other Apple devices")
                if let error = request.lastError { Text(error).font(.caption).foregroundStyle(.red) }
                TextField("2FA code", text: $twoFactorCode).keyboardType(.numberPad)
                HStack {
                    Button("Submit") { account.answerTwoFactor("code:" + twoFactorCode); twoFactorCode = "" }
                    Button("Send to devices") { account.answerTwoFactor("devices") }
                    Button("Cancel", role: .cancel) { account.answerTwoFactor("abort") }
                }
                .buttonStyle(.bordered)
            }
            if account.busy { ProgressView(account.downloadProgress ?? "Working…") }
            if let error = account.lastError { Text(error).font(.caption).foregroundStyle(.red) }
        }

        Section("Sign an IPA") {
            TextField("IPA URL", text: $ipaURL)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .font(.caption.monospaced())
            Button("Download") { Task { await account.download(ipaURL) } }
                .disabled(account.busy)
            ForEach(account.downloads, id: \.self) { file in
                HStack {
                    Text(file).font(.caption.monospaced())
                    Spacer()
                    Button("Sign") {
                        let udid = UserDefaults.standard.string(forKey: "pairedUDID") ?? ""
                        let name = UserDefaults.standard.string(forKey: "pairedName") ?? ThisDevice.name
                        Task { await account.sign(file: file, udid: udid, deviceName: name) }
                    }
                    .buttonStyle(.bordered)
                    .disabled(account.busy || account.team == nil || UserDefaults.standard.string(forKey: "pairedUDID") == nil)
                }
            }
            ForEach(Array(account.signs.enumerated()), id: \.offset) { _, result in
                VStack(alignment: .leading) {
                    Text("\(result.file): \(result.outcome ?? "signing…")").font(.caption.monospaced())
                    ForEach(result.bundles ?? [], id: \.path) { bundle in
                        Text("\(bundle.path)  \(bundle.bundleId ?? "?")  get-task-allow=\(bundle.getTaskAllow.map(String.init) ?? "-")")
                            .font(.caption2.monospaced())
                    }
                }
            }
        }
    }

    @ViewBuilder
    private func serviceRow(_ service: ProbeModel.Advertised) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("\(service.name)\(service.isSelf ? "  (this device)" : "")").font(.headline)
            Text("\(service.type)  port \(service.port.map(String.init) ?? "?")").font(.caption.monospaced())
            if let port = service.port, port > 0 {
                ForEach(model.candidateHosts(for: service), id: \.self) { host in
                    HStack {
                        Text(host).font(.caption.monospaced())
                        Spacer()
                        if service.type == ProbeModel.manualPairingType {
                            Button("Pair") { Task { await model.pair(host: host, port: port) } }
                        } else {
                            Button("Tunnel") { Task { await model.tunnel(host: host, port: port) } }
                        }
                    }
                    .buttonStyle(.bordered)
                    .disabled(model.busy)
                }
            }
        }
    }
}
