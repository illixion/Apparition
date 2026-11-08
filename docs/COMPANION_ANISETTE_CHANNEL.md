# Paired anisette and Apple TLS over the companion channel

Apparition on iOS, iPadOS, tvOS, and visionOS keeps Apple sign-in on the device. When the person
chooses a paired computer for anisette, `CompanionAnisetteProxy` binds a loopback-only HTTP
listener and forwards each connection over a TLS-PSK connection to the companion. The companion
checks the device's `anisette` grant and forwards the v3 protocol to its configured loopback
provider. The same adapter serves HTTPS CONNECT for GrandSlam and developer services, gated
by the separate `appleRelay` grant. The inner Apple TLS terminates in reqwest on the device;
the computer cannot read or alter Apple credentials, tokens, signing keys, or HTTP contents.
It can see the destination, timing and byte counts, supply anisette data, and interrupt traffic.

## Wire agreement

The channel must match `RAVECompanionChannel` and `RAVECompanionIdentify` in RAVESDK:

- Companion relay: TCP 49160; numeric pairing: TCP 49161; anisette channel: TCP 49162.
- TLS 1.2 only, `TLS_PSK_WITH_AES_128_GCM_SHA256`.
- PSK identity base `Apparition/Anisette`, suffixed with `/` and the paired device ID.
- PSK: HKDF-SHA256(token UTF-8, salt `Apparition-Anisette-v1`, info `psk`, 32 bytes).
- First message after TLS is a four-byte little-endian length and Codable JSON `RAVECompanionIdentify`.
  Its identifier key is exactly `deviceID`, including the uppercase `ID`.
- The identify proof uses a 32-byte TLS exporter with label
  `EXPORTER-Apparition-Anisette-Device-v1` and a present, zero-length context (`Some(&[])` in OpenSSL, **not** `None`). Its key is
  HKDF-SHA256(token UTF-8, salt `Apparition-Device-Proof-v1`, empty info, 32 bytes), then HMAC-SHA256.
- Remaining bytes are an HTTP byte stream. `CONNECT <host>:443 HTTP/1.1` opens an opaque
  TCP tunnel only to `gsa.apple.com`, `gsd.apple.com`, `idmsa.apple.com`, or
  `developerservices2.apple.com`. Other hosts, IP literals, userinfo, alternate ports and
  plaintext Apple requests are rejected. The host returns 200 before the device starts inner TLS.
- Ordinary HTTP is anisette. When the built-in provider is paired-only, the host
  prefixes `/t/<token>` to each request path before forwarding it to loopback port 6969.

The token is saved in the device Keychain, never in the saved companion URL or logs. New
addresses contain `?relay=1`; older pairings must pair again to explicitly grant `appleRelay`.
The client checks the returned grant. The host does not upgrade old anisette-only grants.
There is no direct LAN HTTP compatibility mode: the Mac's provider binds loopback; the open-mode
desktop setting and CLI flag were removed. Device sign-in requires
the encrypted companion, including when Local VPN handles installs.

The device-local adapter lives for the signed-in session, because developer signing uses both
anisette and Apple connections after login. Signing out closes its listener and connections.
Provider discovery failures stop login before contacting Apple. Proxied GrandSlam requires HTTPS
and normal certificate verification; debug certificate bypass is refused. Edge-avoidance DNS
middleware runs only on direct desktop connections, since proxied DNS belongs to the companion.

`checkAppleRelay` in Apps MCP checks anisette and the real Apple URL bag with the same adapter
and certificate verification as login, without an Apple account. Unit tests cover the exact
Swift identify key and empty exporter context, invalid proofs, old grants, CONNECT restrictions,
and a fake Apple certificate (rejected before credentials are sent).

## Computers as clients

A computer that isn't a Mac giving its own anisette (Windows, Linux, a Mac on macOS 26 or 27 with SIP on) pairs with the Mac
the same way a device does, through `core/src/companion_client.rs`: the same numeric pairing on
49161 (requesting `anisette`), the token kept in `paired-mac.json` (mode 0600), and the same
TLS-PSK identify on 49162. Signing in, it serves the v3 protocol on a loopback port and carries
each connection over a fresh channel, so isideload's ordinary v3 client works unchanged. It checks
the channel with `/v3/client_info` first, telling apart a Mac that can't be reached, one that no
longer knows this computer (the TLS-PSK handshake fails) and one not offering its anisette (the
channel closes after identify). Unlike a device, the computer makes its Apple TLS connections
itself; it uses only the anisette part of the channel.

## Providers

The channel forwards to the Mac's own anisette (`core/src/anisette`), served on loopback. Only a
Mac offers one: a Windows or Linux companion relays only, and Apparition provides no other
provider. A Mac's anisette needs no provisioning, so the v3 provisioning session answers at once
with a placeholder.
