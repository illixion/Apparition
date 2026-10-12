import SwiftUI

/// An app's icon from its source's iconURL, or a lettered tile when there is none (github:
/// sources carry no icon) or it fails to load.
struct AppIconView: View {
    let url: URL?
    let name: String
    var size: CGFloat = 64

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: size * 0.225, style: .continuous)
        Group {
            if let url {
                AsyncImage(url: url) { phase in
                    if case .success(let image) = phase {
                        image.resizable().scaledToFill()
                    } else {
                        placeholder
                    }
                }
            } else {
                placeholder
            }
        }
        .frame(width: size, height: size)
        .clipShape(shape)
        .overlay(shape.strokeBorder(.white.opacity(0.15), lineWidth: 0.5))
        .accessibilityHidden(true)
    }

    private var placeholder: some View {
        ZStack {
            LinearGradient(colors: [.indigo.opacity(0.8), .purple.opacity(0.6)], startPoint: .topLeading, endPoint: .bottomTrailing)
            Text(name.prefix(1).uppercased())
                .font(.system(size: size * 0.45, weight: .semibold, design: .rounded))
                .foregroundStyle(.white)
        }
    }
}

/// A small capsule carrying a state: an expiry, a step's status.
struct StatusPill: View {
    let text: String
    let systemImage: String
    let tint: Color

    var body: some View {
        Label(text, systemImage: systemImage)
            .font(.caption.weight(.semibold))
            .foregroundStyle(tint)
            .padding(.horizontal, 10)
            .padding(.vertical, 5)
            .background(tint.opacity(0.18), in: Capsule())
    }
}

/// The rounded platter cards and step panels sit on inside the window's glass.
struct Card<Content: View>: View {
    var padding: CGFloat = 24
    @ViewBuilder let content: Content

    var body: some View {
        content
            .padding(padding)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 28, style: .continuous))
    }
}

/// A line of secondary text with an icon, for errors and hints under a control.
struct Note: View {
    enum Kind { case info, warning, error, success }
    let text: String
    var kind: Kind = .info

    var body: some View {
        Label {
            Text(text).selectableText()
        } icon: {
            Image(systemName: icon)
        }
        .font(.callout)
        .foregroundStyle(color)
    }

    private var icon: String {
        switch kind {
        case .info: "info.circle"
        case .warning: "exclamationmark.triangle"
        case .error: "xmark.octagon"
        case .success: "checkmark.circle"
        }
    }

    private var color: Color {
        switch kind {
        case .info: .secondary
        case .warning: .orange
        case .error: .red
        case .success: .green
        }
    }
}

/// A one-time code field: keeps ASCII digits only (a pasted or system-keyboard code can carry
/// spaces, separators or non-ASCII digits that the device rejects), stops at `length`, and
/// calls `onComplete` once when the last digit arrives. `onRawInput` sees the character and
/// unicode-scalar counts of what the field actually received, for diagnostics; never the text.
struct DigitCodeField: View {
    let title: String
    @Binding var code: String
    var length = 6
    var contentType: UITextContentType? = .oneTimeCode
    var onRawInput: ((_ characters: Int, _ scalars: Int) -> Void)? = nil
    let onComplete: (String) -> Void
    @FocusState private var focused: Bool

    /// ASCII 0-9 only: `isNumber` alone also admits fullwidth, Arabic-Indic and other digits.
    static func digits(in raw: String, length: Int) -> String {
        String(raw.filter { $0.isASCII && $0.isNumber }.prefix(length))
    }

    var body: some View {
        TextField(title, text: Binding(get: { code }, set: { raw in
            onRawInput?(raw.count, raw.unicodeScalars.count)
            let digits = Self.digits(in: raw, length: length)
            let completes = digits.count == length && code.count != length
            code = digits
            if completes { onComplete(digits) }
        }))
        .font(.system(.title, design: .monospaced).weight(.semibold))
        .multilineTextAlignment(.center)
        .keyboardType(.numberPad)
        .textContentType(contentType)
        .autocorrectionDisabled()
        .textInputAutocapitalization(.never)
        .focused($focused)
        .frame(width: 260)
        .padding(.vertical, 12)
        .background(.thinMaterial, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        .overlay(alignment: .bottom) {
            Text("\(code.count) of \(length) digits")
                .font(.caption2)
                .foregroundStyle(.secondary)
                .offset(y: 22)
        }
        .padding(.bottom, 20)
        .onAppear { focused = true }
    }
}

extension Date {
    /// "in 5 days", "2 hours ago".
    var relativeToNow: String {
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .full
        formatter.dateTimeStyle = .named
        return formatter.localizedString(for: self, relativeTo: Date())
    }

    static func iso(_ text: String?) -> Date? {
        text.flatMap { try? Date($0, strategy: Date.ISO8601FormatStyle(includingFractionalSeconds: true)) }
    }
}

// MARK: - Platform fallbacks

extension View {
    /// Selectable text where the platform has selection (not tvOS, which moves focus instead).
    @ViewBuilder
    func selectableText() -> some View {
        #if os(tvOS)
        self
        #else
        textSelection(.enabled)
        #endif
    }

    /// A bordered text field; tvOS draws its own focusable field.
    @ViewBuilder
    func roundedFieldStyle() -> some View {
        #if os(tvOS)
        self
        #else
        textFieldStyle(.roundedBorder)
        #endif
    }
}

/// DisclosureGroup, which tvOS lacks: there the content is simply shown under its title.
struct Disclosure<Content: View>: View {
    let title: String
    @ViewBuilder let content: () -> Content

    init(_ title: String, @ViewBuilder content: @escaping () -> Content) {
        self.title = title
        self.content = content
    }

    var body: some View {
        #if os(tvOS)
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.headline)
            content()
        }
        #else
        DisclosureGroup(title, content: content)
        #endif
    }
}

extension View {
    /// A sheet; full screen on tvOS, where a sheet is a small floating card that clips a form.
    /// The remote's Menu button dismisses it there.
    @ViewBuilder
    func panel<Content: View>(isPresented: Binding<Bool>, @ViewBuilder content: @escaping () -> Content) -> some View {
        #if os(tvOS)
        // A cover draws no backdrop of its own on tvOS, so the screen behind would show through.
        fullScreenCover(isPresented: isPresented) {
            content().background(Color(white: 0.07).ignoresSafeArea())
        }
        #else
        sheet(isPresented: isPresented, content: content)
        #endif
    }

    /// A sheet's minimum size on visionOS and iPad; tvOS presents it full screen instead.
    @ViewBuilder
    func sheetFrame(minWidth: CGFloat, minHeight: CGFloat) -> some View {
        #if os(tvOS)
        self
        #else
        frame(minWidth: minWidth, minHeight: minHeight)
        #endif
    }
}
