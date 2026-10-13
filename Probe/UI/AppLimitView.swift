import SwiftUI

/// In place of installd's raw error when an install hits the free-profile app limit: the apps
/// holding the slots, each removable but Apparition itself, and the install retried after.
/// Bare content: a List section in Browse, a Card in the Library.
struct AppLimitView: View {
    let model: ProbeModel
    let library: LibraryModel
    let limit: AppLimit

    @State private var pendingRemoval: String?

    private var working: Bool { library.busy || model.busy }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Label("This \(ThisDevice.noun)'s free-account app limit is reached", systemImage: "square.stack.3d.up.slash")
                .font(.headline)
                .foregroundStyle(.orange)
            Text("\(ThisDevice.os) lets only 3 apps signed by free Apple IDs be installed at once, and Apparition counts as one. Remove one to install \(limit.appName).")
                .font(.callout)
                .foregroundStyle(.secondary)
            ForEach(limit.blocking, id: \.self, content: row)
            if limit.blocking.isEmpty {
                Note(text: "\(ThisDevice.os) didn't say which apps they are. Delete one from the \(ThisDevice.home), then try again.", kind: .info)
            }
            if let error = limit.removalError {
                Note(text: "Couldn't remove it: \(error)", kind: .error).lineLimit(4)
            }
            HStack(spacing: 12) {
                Button("Try Again") { Task { await library.retryAfterAppLimit() } }
                    .buttonStyle(.bordered)
                    .disabled(working)
                Button("Dismiss") { library.dismissAppLimit() }
                    .buttonStyle(.borderless)
            }
            .focusRow()
            Text("A paid developer account's certificate (Settings > Signing) has no such limit.")
                .font(.footnote)
                .foregroundStyle(.secondary)
                .focusableText()
        }
        .padding(.vertical, 6)
        .task(id: limit.blocking) {
            // Names for apps the last listing doesn't have (installed since, or never listed).
            let known = Set(model.deviceApps.map(\.bundleId))
            guard limit.blocking.contains(where: { !known.contains($0) && $0 != Bundle.main.bundleIdentifier }),
                  !working else { return }
            await model.listApps(host: model.defaultTunnelHost, port: model.defaultTunnelPort)
        }
    }

    private func row(_ bundleId: String) -> some View {
        let name = library.displayName(of: bundleId)
        let isSelf = bundleId == Bundle.main.bundleIdentifier
        return HStack(spacing: 14) {
            AppIconView(url: library.iconURL(of: bundleId), name: name, size: 44)
            VStack(alignment: .leading, spacing: 2) {
                Text(name).font(.body.weight(.semibold)).lineLimit(1)
                if name != bundleId {
                    Text(bundleId)
                        .font(.caption.monospaced())
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
            }
            Spacer(minLength: 0)
            if isSelf {
                StatusPill(text: "This app", systemImage: "sparkles", tint: .accentColor)
            } else {
                Button("Remove", role: .destructive) { pendingRemoval = bundleId }
                    .buttonStyle(.bordered)
                    .disabled(working)
                    // On the button, so iOS's popover points at it rather than the card.
                    .confirmationDialog("Remove \(library.displayName(of: bundleId))?",
                                        isPresented: Binding(get: { pendingRemoval == bundleId }, set: { if !$0 { pendingRemoval = nil } }),
                                        titleVisibility: .visible) {
                        Button("Remove and Install \(limit.appName)", role: .destructive) {
                            Task { await library.removeForAppLimit(bundleId) }
                        }
                    } message: {
                        Text("Its data on this \(ThisDevice.noun) is deleted too. \(limit.appName) installs right after.")
                    }
            }
        }
        .focusRow()
    }
}
