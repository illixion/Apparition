#if !os(tvOS)  // No share sheet or AirDrop there.
import DebugTrace
import SwiftUI

private let log = DebugLogger(subsystem: Bundle.main.bundleIdentifier ?? "Apparition", category: "Share")

/// The system's share options for a file, shown the way Hypnos shows them (its ShareSheetHelper),
/// because SwiftUI's ShareLink inside a sheet opened nothing on the headset: a sheet with a
/// UIActivityViewController, presented once its view is in the hierarchy.
///
/// A second copy of Hypnos's `shareSheet`, which belongs in RAVEUI, where both apps could share it.
extension View {
    /// The share options for `url` while it is set; cleared, and `onDismiss` called, once they close.
    func shareSheet(url: Binding<URL?>, onDismiss: @escaping () -> Void = {}) -> some View {
        sheet(isPresented: Binding(
            get: { url.wrappedValue != nil },
            set: { if !$0 { url.wrappedValue = nil } }
        ), onDismiss: onDismiss) {
            if let shared = url.wrappedValue {
                ActivityViewController(
                    activityItems: [shared],
                    isPresented: Binding(
                        get: { url.wrappedValue != nil },
                        set: { if !$0 { url.wrappedValue = nil } }
                    )
                )
            }
        }
    }
}

private struct ActivityViewController: UIViewControllerRepresentable {
    let activityItems: [Any]
    @Binding var isPresented: Bool

    func makeUIViewController(context: Context) -> ActivityHostController {
        ActivityHostController(activityItems: activityItems, isPresented: $isPresented)
    }

    func updateUIViewController(_ uiViewController: ActivityHostController, context: Context) {}
}

/// Presents the UIActivityViewController in viewDidAppear, with the view fully in the hierarchy.
private final class ActivityHostController: UIViewController {
    private let activityItems: [Any]
    private var isPresented: Binding<Bool>
    private var didPresent = false

    init(activityItems: [Any], isPresented: Binding<Bool>) {
        self.activityItems = activityItems
        self.isPresented = isPresented
        super.init(nibName: nil, bundle: nil)
        view.backgroundColor = .clear
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        guard !didPresent else { return }
        didPresent = true

        let activityVC = UIActivityViewController(activityItems: activityItems, applicationActivities: nil)
        if let popover = activityVC.popoverPresentationController {
            popover.sourceView = view
            popover.sourceRect = CGRect(x: view.bounds.midX, y: view.bounds.midY, width: 0, height: 0)
            popover.permittedArrowDirections = []
        }
        activityVC.completionWithItemsHandler = { [weak self] activity, completed, _, error in
            log.info("share sheet closed: completed \(completed, privacy: .public), activity \(activity?.rawValue ?? "none", privacy: .public), error \(error.map { "\($0)" } ?? "none", privacy: .private)")
            DispatchQueue.main.async {
                self?.isPresented.wrappedValue = false
            }
        }
        log.info("share sheet opening")
        present(activityVC, animated: true)
    }
}
#endif
