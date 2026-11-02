import DebugTrace
import SwiftUI

private let pairingLog = DebugLogger(subsystem: "Apparition", category: "Pairing")

/// Apple's 2FA step of a sign-in, wherever the sign-in started: the code field, other ways to
/// get a code, and Cancel.
struct TwoFactorPrompt: View {
    let account: AccountModel
    let request: AccountModel.TwoFactorRequest

    @State private var code = ""

    var body: some View {
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

/// The 2FA step of a sign-in started away from Setup (an install, a refresh, the certificates
/// screen), as RootView presents it.
struct TwoFactorSheet: View {
    let account: AccountModel

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    Text("Apple wants to confirm it's you before Apparition signs in\(account.lastAppleID.map { " as \($0)" } ?? "").")
                        .foregroundStyle(.secondary)
                    if let request = account.twoFactor {
                        TwoFactorPrompt(account: account, request: request)
                    }
                }
                .padding(24)
                .frame(maxWidth: 640, alignment: .leading)
                .frame(maxWidth: .infinity)
            }
            .navigationTitle("Two-Factor Code")
        }
        .sheetFrame(minWidth: 640, minHeight: 420)
    }
}

/// Apple's device-pairing PIN prompt is presented over the app so it stays visible while the
/// person reads the code from Settings in another window.
struct DevicePairingPinSheet: View {
    let model: ProbeModel

    @State private var pin = ""
    @State private var rawCharacters = 0
    @State private var rawScalars = 0
    @State private var submitted = false

    var body: some View {
        NavigationStack {
            VStack(alignment: .leading, spacing: 16) {
                Text("Enter the six-digit code shown in Settings > General > Remote Devices.")
                    .foregroundStyle(.secondary)
                HStack(alignment: .top, spacing: 16) {
                    DigitCodeField(title: "000000", code: $pin, contentType: nil,
                                   onRawInput: { rawCharacters = $0; rawScalars = $1 },
                                   onComplete: submit)
                    Button("Pair") { submit(pin) }
                        .buttonStyle(.borderedProminent)
                        .disabled(pin.count != 6 || submitted)
                    Button("Cancel", role: .cancel) { model.cancelPin() }
                        .buttonStyle(.bordered)
                }
                .focusRow()
                Spacer(minLength: 0)
            }
            .padding(24)
            .frame(maxWidth: 640, alignment: .leading)
            .frame(maxWidth: .infinity)
            .navigationTitle("Pair This Device")
            .onAppear { pin = ""; submitted = false }
        }
        .sheetFrame(minWidth: 640, minHeight: 260)
    }

    private func submit(_ code: String) {
        let digits = DigitCodeField.digits(in: code, length: 6)
        pairingLog.info("pairing pin submit: \(digits.count, privacy: .public) digits; field held \(rawCharacters, privacy: .public) characters, \(rawScalars, privacy: .public) scalars")
        guard digits.count == 6, !submitted else { return }
        submitted = true
        model.submitPin(digits)
        pin = ""
    }
}

extension View {
    /// This view shows a pending 2FA request itself, so RootView's sheet stays away while it's on
    /// screen (a sheet can't open over another one this view may be in).
    func hostsTwoFactorPrompt(_ account: AccountModel) -> some View {
        onAppear { account.twoFactorHosts += 1 }
            .onDisappear { account.twoFactorHosts = max(0, account.twoFactorHosts - 1) }
    }
}
