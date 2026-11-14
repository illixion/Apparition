import Foundation
import Testing
@testable import Apparition

@MainActor
struct HookJITRequestTests {
    @Test func ordinaryJITRequestsKeepTheirPersistentSessionPolicy() throws {
        let request = try #require(LibraryModel.jitRequest(URL(string: "apparition://enable-jit?bundle-id=com.example.app&pid=42")!))
        #expect(!request.hooksOnly)
        #expect(request.pid == 42)
    }

    @Test func hooksCanRequestAttachAndDetachWithoutRelaunching() throws {
        let request = try #require(LibraryModel.jitRequest(URL(string: "apparition://enable-jit?bundle-id=com.example.app&pid=42&hooks-only=1")!))
        #expect(request.hooksOnly)
        #expect(request.pid == 42)
    }

    @Test func hookOnlyPolicyDoesNotOverrideScriptsOrLaunchRequests() throws {
        for link in [
            "apparition://enable-jit?bundle-id=com.example.app&hooks-only=1",
            "stikdebug://enable-jit?bundle-id=com.example.app&pid=42&hooks-only=1",
            "apparition://enable-jit?bundle-id=com.example.app&pid=42&hooks-only=1&script-data=MSsx",
        ] {
            let request = try #require(LibraryModel.jitRequest(URL(string: link)!))
            #expect(!request.hooksOnly)
        }
    }
}
