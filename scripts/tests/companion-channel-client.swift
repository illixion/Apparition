import Foundation
import Network

@main
struct CompanionChannelInterop {
  static func main() {
    let namespace = RAVECompanionNamespace(
      serviceStem: "apparition", labelPrefix: "Apparition", urlScheme: "apparition",
      defaultPairPort: 49161, hostAppName: "Test")
    let credential = RAVECompanionCredential(
      token: "test-only-pairing-token", deviceID: "test-device")
    let channel = RAVECompanionChannel(
      identityBase: "Apparition/Anisette", salt: "Apparition-Anisette-v1")
    let connection = NWConnection(
      host: "127.0.0.1", port: NWEndpoint.Port(rawValue: UInt16(CommandLine.arguments[1])!)!,
      using: channel.tcpParameters(credentials: [credential]))
    let queue = DispatchQueue(label: "interop")
    func finish(_ good: Bool) -> Never {
      print(good ? "Swift channel OK" : "Swift channel FAIL")
      exit(good ? 0 : 1)
    }
    var response = Data()
    func receive() {
      connection.receive(minimumIncompleteLength: 1, maximumLength: 16384) {
        data, _, complete, error in
        if let data { response.append(data) }
        if response.suffix(2) == Data("ok".utf8) { finish(true) }
        if complete || error != nil { finish(false) }
        receive()
      }
    }
    connection.stateUpdateHandler = { state in
      switch state {
      case .ready:
        guard
          let identify = RAVECompanionIdentify.make(
            namespace, credential: credential,
            exporterLabel: "EXPORTER-Apparition-Anisette-Device-v1", connection: connection)
        else { finish(false) }
        let body = identify.encoded()
        var length = UInt32(body.count).littleEndian
        var data = withUnsafeBytes(of: &length) { Data($0) }
        data.append(body)
        data.append(
          Data("GET /v3/client_info HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".utf8))
        connection.send(
          content: data,
          completion: .contentProcessed { error in
            if error != nil { finish(false) }
            receive()
          })
      case .failed: finish(false)
      default: break
      }
    }
    connection.start(queue: queue)
    queue.asyncAfter(deadline: .now() + 10) { finish(false) }
    dispatchMain()

  }
}
