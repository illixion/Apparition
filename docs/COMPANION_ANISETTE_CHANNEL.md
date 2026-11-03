# Paired anisette over the companion channel

Apparition on iOS, iPadOS, tvOS, and visionOS keeps Apple sign-in on the device. When the person
chooses a paired computer for anisette, `CompanionAnisetteProxy` binds a loopback-only HTTP
listener and forwards each connection over a TLS-PSK connection to the companion. The companion
checks the device's `anisette` grant and forwards the v3 protocol to its configured provider.

## Wire agreement

The channel must match `RAVECompanionChannel` and `RAVECompanionIdentify` in RAVESDK:

- Companion relay: TCP 49160; numeric pairing: TCP 49161; anisette channel: TCP 49162.
- TLS 1.2 only, `TLS_PSK_WITH_AES_128_GCM_SHA256`.
- PSK identity base `Apparition/Anisette`, suffixed with `/` and the paired device ID.
- PSK: HKDF-SHA256(token UTF-8, salt `Apparition-Anisette-v1`, info `psk`, 32 bytes).
- First message after TLS is a four-byte little-endian length and Codable JSON `RAVECompanionIdentify`.
- The identify proof uses a 32-byte TLS exporter with label
  `EXPORTER-Apparition-Anisette-Device-v1` and empty context. Its key is
  HKDF-SHA256(token UTF-8, salt `Apparition-Device-Proof-v1`, empty info, 32 bytes), then HMAC-SHA256.
- Remaining bytes are an HTTP byte stream. When the built-in provider is paired-only, the host
  prefixes `/t/<token>` to each request path before forwarding it to loopback port 6969.

The token is saved in the device Keychain, never in the saved companion URL or logs. Existing
`http://host:6969/t/<token>` settings remain readable for compatibility, but new pairing fails
closed when the host does not advertise the secure channel.

## Providers

The channel forwards to the provider already selected on the computer: the built-in provider or a
local v3 service such as Docker. This keeps the transport independent of how anisette was made.
Dionysus and rosetta-free-wine are not required: macOS already has its native anisette provider,
and a Windows CoreADI provider would be a separate optional provider implementation. It should
not change pairing, the channel protocol, or move Apple credentials off the device.
