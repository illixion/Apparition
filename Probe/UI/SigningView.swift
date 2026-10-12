import SwiftUI
import UniformTypeIdentifiers

/// What signs the library's apps: the Apple ID (free, 7 days, slots), or the user's own
/// certificate and provisioning profiles from a paid developer account (a year, no sign-in).
/// Importing, checking what fits this headset, and removing them.
struct SigningView: View {
    let account: AccountModel
    let library: LibraryModel
    let onSignIn: () -> Void

    @AppStorage("pairedUDID") private var pairedUDID: String?
    @State private var importing: ImportKind?
    @State private var working = false
    @State private var message: (text: String, kind: Note.Kind)?
    @State private var pendingIdentity: SigningStore.Identity?
    @State private var pendingProfile: SigningStore.Profile?

    private var signing: SigningStore { library.signing }

    enum ImportKind: Identifiable {
        case certificate, profiles
        var id: Self { self }

        var types: [UTType] {
            switch self {
            case .certificate: [.pkcs12]
            case .profiles: [UTType(filenameExtension: "mobileprovision") ?? .data]
            }
        }
    }

    var body: some View {
        Form {
            if let message {
                Section { Note(text: message.text, kind: message.kind) }
            }
            signerSection
            certificatesSection
            profilesSection
            appleIDSection
            howToSection
        }
        .navigationTitle("Signing")
        .onAppear { signing.load() }
        #if !os(tvOS)
        // tvOS has no Files app: certificates and profiles arrive from a computer instead
        // (Apparition Installer puts them in the Signing inbox).
        .fileImporter(isPresented: Binding(get: { importing != nil }, set: { if !$0 { importing = nil } }),
                      allowedContentTypes: importing?.types ?? [.data],
                      allowsMultipleSelection: importing == .profiles) { result in
            let kind = importing
            importing = nil
            handlePicked(result, kind: kind)
        }
        #endif
        .confirmationDialog("Remove \(pendingIdentity?.name ?? "this certificate")?",
                            isPresented: Binding(get: { pendingIdentity != nil }, set: { if !$0 { pendingIdentity = nil } }),
                            titleVisibility: .visible, presenting: pendingIdentity) { identity in
            Button("Remove Certificate", role: .destructive) { remove(identity) }
        } message: { identity in
            Text(removeMessage(identity))
        }
        .confirmationDialog("Remove \(pendingProfile?.name ?? "this profile")?",
                            isPresented: Binding(get: { pendingProfile != nil }, set: { if !$0 { pendingProfile = nil } }),
                            titleVisibility: .visible, presenting: pendingProfile) { profile in
            Button("Remove Profile", role: .destructive) { remove(profile) }
        } message: { _ in
            Text("Installed apps keep working until it expires; the next refresh needs another profile that fits.")
        }
    }

    // MARK: Signer

    private var signerSection: some View {
        Section {
            Picker("Sign apps with", selection: Binding(get: { signing.defaultSigner }, set: { signing.defaultSigner = $0 })) {
                Text("Apple ID").tag(SigningStore.appleID)
                ForEach(signing.identities) { identity in
                    Text(identity.name).tag(identity.id)
                }
            }
            .pickerStyle(.inline)
            .labelsHidden()
        } header: {
            Text("Sign Apps With")
        } footer: {
            Text((signing.defaultSigner == SigningStore.appleID
                 ? "Your Apple ID signs for 7 days at a time and needs the anisette server. A free account has two certificates and ten App IDs a week. Each app can choose otherwise in its menu in the Library."
                 : "Your own certificate signs without signing in, for as long as its profile lasts (usually a year). Each app can choose otherwise in its menu in the Library; the change applies at its next refresh.")
                 + " Switching changes the app's bundle ID (the Apple ID adds your team ID to it), so the app installs as a separate copy with its own data.")
        }
    }

    // MARK: Certificates

    private var certificatesSection: some View {
        Section {
            if signing.identities.isEmpty {
                Text("No certificates imported.").foregroundStyle(.secondary)
            }
            ForEach(signing.identities) { identity in
                identityRow(identity)
            }
            Button {
                importing = .certificate
            } label: {
                Label("Import Certificate (.p12)…", systemImage: "square.and.arrow.down")
            }
            .disabled(working)
        } header: {
            Text("Your Certificates")
        } footer: {
            Text("An Apple Development or Distribution certificate exported with its private key. Its password opens the file once and isn't kept. The key stays in Apparition's storage on this \(ThisDevice.noun).")
        }
    }

    private func identityRow(_ identity: SigningStore.Identity) -> some View {
        let usable = signing.usableProfiles(for: identity)
        return HStack(alignment: .top, spacing: 16) {
            Image(systemName: "checkmark.seal.fill")
                .font(.title2)
                .foregroundStyle(identity.expired ? Color.secondary : Color.accentColor)
                .frame(width: 40)
            VStack(alignment: .leading, spacing: 6) {
                Text(identity.name).font(.headline)
                Text([identity.teamName, identity.teamId.map { "Team \($0)" }].compactMap { $0 }.joined(separator: " · "))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                HStack(spacing: 8) {
                    StatusPill(text: identity.kind == "distribution" ? "Distribution" : "Development",
                               systemImage: "person.badge.key", tint: .blue)
                    expiryText(identity.expires)
                }
                if identity.expired {
                    Note(text: "Expired. Make a new certificate in your developer account and import it.", kind: .error)
                } else if usable.isEmpty {
                    Note(text: "No imported profile lets this certificate sign for this \(ThisDevice.noun) yet. Import one that includes it and this \(ThisDevice.noun)'s UDID.", kind: .warning)
                } else {
                    Note(text: "\(Self.profiles(usable.count)) for this \(ThisDevice.noun)", kind: .success)
                }
            }
            Spacer()
            Button("Remove", role: .destructive) { pendingIdentity = identity }
                .buttonStyle(.bordered)
                .disabled(working)
        }
        .padding(.vertical, 6)
    }

    private func removeMessage(_ identity: SigningStore.Identity) -> String {
        let users = library.installed.filter { signing.signer(for: $0) == identity.id }
        var text = "Its private key is deleted from this \(ThisDevice.noun). Installed apps keep working until their profiles expire."
        if !users.isEmpty {
            text += " \(users.map(\.name).formatted(.list(type: .and))) will sign with \(users.count == 1 ? "its" : "their") default again at the next refresh."
        }
        return text
    }

    // MARK: Profiles

    private var profilesSection: some View {
        Section {
            if signing.profiles.isEmpty {
                Text("No profiles imported.").foregroundStyle(.secondary)
            }
            ForEach(signing.profiles) { profile in
                profileRow(profile)
            }
            Button {
                importing = .profiles
            } label: {
                Label("Import Profiles (.mobileprovision)…", systemImage: "square.and.arrow.down")
            }
            .disabled(working)
        } header: {
            Text("Provisioning Profiles")
        } footer: {
            Text("Apparition picks, for each app and extension, a profile for your certificate that lists this \(ThisDevice.noun): one made for the app's own ID first, then the closest wildcard. An explicit App ID renames the app to it; a wildcard (*) keeps the app's own ID. Development profiles allow Launch with JIT; ad hoc and enterprise ones don't.")
        }
    }

    private func profileRow(_ profile: SigningStore.Profile) -> some View {
        let certificates = profile.identityIds.compactMap(signing.identity)
        return HStack(alignment: .top, spacing: 16) {
            Image(systemName: "doc.badge.gearshape")
                .font(.title2)
                .frame(width: 40)
            VStack(alignment: .leading, spacing: 6) {
                Text(profile.name).font(.headline)
                Text(profile.appIdPattern)
                    .font(.callout.monospaced())
                    .foregroundStyle(.secondary)
                    .selectableText()
                HStack(spacing: 8) {
                    kindPill(profile)
                    if profile.debuggable {
                        StatusPill(text: "JIT", systemImage: "bolt.fill", tint: .purple)
                    }
                    expiryText(profile.expires)
                }
                if let problem = problem(profile, certificates: certificates) {
                    Note(text: problem, kind: profile.expired || profile.kind == "appStore" || profile.forPlatform == false ? .error : .warning)
                } else {
                    Note(text: "Signs with \(certificates.map(\.name).formatted(.list(type: .or)))", kind: .success)
                }
            }
            Spacer()
            Button("Remove", role: .destructive) { pendingProfile = profile }
                .buttonStyle(.bordered)
                .disabled(working)
        }
        .padding(.vertical, 6)
    }

    private func kindPill(_ profile: SigningStore.Profile) -> StatusPill {
        switch profile.kind {
        case "development": StatusPill(text: "Development", systemImage: "hammer", tint: .green)
        case "adHoc": StatusPill(text: "Ad Hoc", systemImage: "shippingbox", tint: .teal)
        case "enterprise": StatusPill(text: "Enterprise", systemImage: "building.2", tint: .indigo)
        default: StatusPill(text: "App Store", systemImage: "bag", tint: .secondary)
        }
    }

    private func problem(_ profile: SigningStore.Profile, certificates: [SigningStore.Identity]) -> String? {
        if profile.expired { return "Expired." }
        if profile.kind == "appStore" { return "An App Store profile can't install apps on a device. Use a development or ad hoc one." }
        if profile.forPlatform == false {
            return "Made for \(profile.platforms.joined(separator: ", ")), not \(ThisDevice.os). Make the profile for an App ID that includes \(ThisDevice.os)."
        }
        if profile.includesDevice == false {
            return "Doesn't list this \(ThisDevice.noun). Add its UDID to the profile's devices in your developer account, download the profile again and import it."
        }
        if certificates.isEmpty { return "None of your imported certificates can sign with it. Import the certificate it was made for." }
        return nil
    }

    // MARK: Apple ID and how-to

    private var appleIDSection: some View {
        Section {
            NavigationLink {
                DeveloperSlotsView(account: account, library: library, onSignIn: onSignIn)
            } label: {
                Label("Apple ID Certificates & App IDs", systemImage: "person.badge.key")
            }
        } footer: {
            Text("What the Apple ID's signing uses up: its certificate slots and App IDs, and freeing them.")
        }
    }

    private var howToSection: some View {
        Section {
            if let pairedUDID {
                LabeledContent("This \(ThisDevice.noun)'s UDID") {
                    Text(pairedUDID).font(.callout.monospaced()).selectableText()
                }
            }
            Text("""
            1. On developer.apple.com, register this headset under Devices with the UDID above.
            2. Make an Apple Development certificate (or use Xcode's), then on your Mac export it from Keychain Access (My Certificates, Export…) as a .p12 with a password.
            3. Make a development profile for an App ID (a wildcard covers every app) that includes the certificate and this headset, and download it.
            4. Bring both here: share or AirDrop them to Apparition, put them in Files > On My Apple Vision Pro > Apparition > Signing, or import them above.
            """)
            .font(.callout)
            .foregroundStyle(.secondary)
        } header: {
            Text("Using Your Own Developer Account")
        }
    }

    // MARK: Import

    /// Picked files go the same way as shared ones: profiles import at once, a certificate
    /// asks for its password in IncomingSigningSheet.
    private func handlePicked(_ result: Result<[URL], Error>, kind: ImportKind?) {
        switch result {
        case .failure(let error):
            message = ("\(error.localizedDescription)", .error)
        case .success(let urls):
            message = nil
            Task {
                for url in urls { await signing.receive(url) }
            }
        }
    }

    private func remove(_ identity: SigningStore.Identity) {
        do {
            try signing.deleteIdentity(identity)
            message = nil
        } catch {
            message = ("\(error)", .error)
        }
    }

    private func remove(_ profile: SigningStore.Profile) {
        do {
            try signing.deleteProfile(profile)
            message = nil
        } catch {
            message = ("\(error)", .error)
        }
    }

    // MARK: Helpers

    private static func profiles(_ count: Int) -> String {
        count == 1 ? "1 profile" : "\(count) profiles"
    }

    @ViewBuilder
    private func expiryText(_ text: String?) -> some View {
        if let expires = DeveloperSlotsView.date(text) {
            Text(expires < Date() ? "Expired \(expires.relativeToNow)" : "Expires \(expires.relativeToNow)")
                .font(.callout)
                .foregroundStyle(expires < Date() ? .red : .secondary)
        }
    }
}

