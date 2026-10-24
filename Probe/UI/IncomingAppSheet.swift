import RAVEUI
import SwiftUI

/// Shown over any tab when an .ipa or .deb arrives (shared from Safari or Files, opened in
/// Apparition, or picked in the Library): names the app inside and asks before signing and
/// installing it.
struct IncomingAppSheet: View {
    let library: LibraryModel
    /// Where the install's progress shows, once the person has said go.
    let onInstall: () -> Void

    var body: some View {
        NavigationStack {
            Form {
                RAVEScreenHeading(title)
                switch library.incoming {
                case .reading(let file):
                    Section {
                        HStack(spacing: 12) {
                            ProgressView()
                            Text("Reading \(file)").foregroundStyle(.secondary)
                        }
                    } footer: {
                        Text("A large app or a .deb package takes a moment: Apparition keeps a copy and unpacks the app from a package.")
                    }
                case .ready(let app):
                    Section {
                        LabeledContent("App", value: app.name)
                        if !app.version.isEmpty { LabeledContent("Version", value: app.version) }
                        LabeledContent("Bundle ID", value: app.bundleId)
                        LabeledContent("File", value: app.fileName)
                    } footer: {
                        Text("Apparition signs it with \(library.signing.signerName(library.signing.defaultSigner)), installs it on this \(ThisDevice.noun) and keeps it refreshed like the apps from your sources. Only install apps you trust.")
                    }
                    Section {
                        Button("Install \(app.name)") {
                            onInstall()
                            Task { await library.installIncoming() }
                        }
                        Button("Cancel", role: .cancel) { library.discardIncoming() }
                    }
                case .failed(let file, let message):
                    Section {
                        Note(text: message, kind: .error)
                    } header: {
                        Text(file)
                    }
                    Section {
                        Button("Close", role: .cancel) { library.discardIncoming() }
                    }
                case nil:
                    EmptyView()
                }
            }
            .raveScreenTitle(title)
        }
        .sheetFrame(minWidth: 560, minHeight: 360)
    }

    private var title: String {
        switch library.incoming {
        case .ready: "Install App"
        case .failed: "Couldn't Read the File"
        default: "App File"
        }
    }
}
