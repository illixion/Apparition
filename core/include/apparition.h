// Apparition core C ABI (core/src/lib.rs). Every call blocks; call off the main thread.
// Strings returned by apparition_* calls are JSON and must be freed with apparition_string_free.
#ifndef APPARITION_H
#define APPARITION_H

#include <stdbool.h>
#include <stdint.h>

struct AdapterHandle;
struct RsdHandshakeHandle;
struct RpPairingFileHandle;

typedef const char *(*apparition_two_factor_fn)(void *context, const char *params_json);

void apparition_string_free(char *string);
void apparition_init_logging(const char *log_path);

char *apparition_login(const char *email, const char *password, const char *anisette_url,
                   const char *data_dir, apparition_two_factor_fn two_factor, void *context);
char *apparition_session(void);
// Forgets the signed-in session in this process. Returns {ok}.
char *apparition_logout(void);
// Repacks the app in a .deb package (Kodi and many older apps are published that way) as an IPA at
// out_path (core/src/package.rs). Reads the whole package; takes about as long as downloading it.
// {ok, app, files, bytes, others}.
char *apparition_convert_package(const char *package_path, const char *out_path);
// Reads the main app's name, version and bundle identifier from the IPA at path, for listing an app
// picked from a file (core/src/identity.rs). {ok, bundleId, name, version}.
char *apparition_app_info(const char *path);
char *apparition_sign(const char *app_path, const char *udid, const char *device_name,
                  bool strip_extensions);
// Signing with the Apple ID stops with code "sharedCertificate" rather than make a certificate
// while Apparition on another device has one on the account: its key, shared, costs no slot.
// This lets the next signing with email make one anyway. {ok}.
char *apparition_allow_new_certificate(const char *email);
// Offline signing with an imported certificate and profiles (core/src/identity.rs). data_dir is
// the same directory apparition_login takes; udid may be NULL where noted.
// {ok, identities: [...], profiles: [...]}; udid (nullable) fills each profile's includesDevice.
char *apparition_signing_list(const char *data_dir, const char *udid);
// Imports a .p12's certificate and key; the password is used to open it and not kept. {ok, identity}.
char *apparition_identity_import(const char *data_dir, const char *p12_path, const char *password);
char *apparition_identity_delete(const char *data_dir, const char *identity_id);
// {ok, profile}; udid is nullable.
char *apparition_profile_import(const char *data_dir, const char *path, const char *udid);
char *apparition_profile_delete(const char *data_dir, const char *uuid);

// Imports a .apparitionsigning file, or a sealed .apparitionkey with the five words its sender
// showed (passphrase; NULL for a plain file): an Apple ID's certificate key shared by another
// Apparition install, so both use one certificate. Ends the signed-in session (only once the
// file has opened). Returns {ok, kind, appleId, teamId, from, replaced}.
char *apparition_import_signing_file(const char *data_dir, const char *path, const char *passphrase);
// Who sealed a .apparitionkey file, readable without the words: {ok, from}.
char *apparition_signing_file_from(const char *path);
// Seals the Apple ID's certificate key for another device into
// <out_dir>/Apple ID signing.apparitionkey. team_id may be NULL. Returns {ok, path, passphrase}:
// the five words to show, which open it.
char *apparition_export_signing_share(const char *data_dir, const char *email, const char *team_id,
                                      const char *from, const char *out_dir);

// Writes the signed-in Apple ID's development certificate and its private key as a .p12 into
// <out_dir>/Apparition Certificate.p12, for an app that signs with one (LiveContainer, SideStore).
// Needs a signed-in session. Makes no certificate: it fails where Apple has none for this install's
// key. Returns {ok, path, password, serialNumber}: the password is six new words joined by hyphens,
// to show and copy. Whoever has both can sign as this Apple ID.
char *apparition_export_certificate_p12(const char *out_dir);

// Handing the certificate key to a nearby Apparition, checked by a six-digit code both screens
// show (core/src/nearby.rs). The app moves the messages: each is JSON text, framed on the wire by
// its length (4 bytes, little-endian, at most 16 KiB). Every reply but end's is
// {ok, send: [message], state, code, peer, result, reason}: send each message in order; state is
// waiting, compare (show code and ask whether the other device shows it), confirmed, done
// (result: the import, for a receiver) or cancelled (reason says why). An error ends the exchange.
// The sender connects; begin's first message goes out once connected. team_id may be NULL.
char *apparition_nearby_send_begin(const char *data_dir, const char *email, const char *team_id,
                                   const char *name);
// The receiver listens. A key the person accepts is imported into data_dir and ends the session,
// as apparition_import_signing_file does.
char *apparition_nearby_receive_begin(const char *data_dir, const char *name);
char *apparition_nearby_handle(uint64_t handle, const char *message);
// The person's answer: whether the other device shows the same code.
char *apparition_nearby_confirm(uint64_t handle, bool matches);
void apparition_nearby_end(uint64_t handle);

// Experiment: signs app_path with the free profile of an App ID the team already has, keeping the
// app's bundle ID (or bundle_id, may be NULL). Needs the session. Returns {ok, signedPath, bundleId,
// profileAppId, expires}.
char *apparition_sign_with_existing_profile(const char *app_path, const char *profile_app_id, const char *bundle_id,
                                           bool minimal);
// Experiment control: the same with an imported certificate and profile (by UUID).
char *apparition_sign_offline_keeping_id(const char *data_dir, const char *app_path, const char *identity_id,
                                         const char *profile_uuid, const char *bundle_id, bool minimal);
// Signs with identity_id and the best imported profile (profile_uuid, nullable, overrides it for
// the main app). {ok, signedPath, bundleId, profile, profileName, expires, debuggable, strippedExtensions}.
// previous_app (nullable): a signed .app whose keychain groups to keep, for an unsigned update of it.
char *apparition_sign_offline(const char *data_dir, const char *app_path, const char *identity_id,
                              const char *profile_uuid, const char *udid, bool strip_extensions,
                              const char *previous_app);
// Handles from idevice's tunnel_create_rppairing; borrowed, not freed.
char *apparition_install(struct AdapterHandle *adapter, struct RsdHandshakeHandle *handshake,
                     const char *app_path);
// Every user app on the device, whoever installed it: {ok, apps: [{bundleId, name, version,
// debuggable, teamId, signer}]}. debuggable apps (get-task-allow) can be launched with JIT.
char *apparition_list_apps(struct AdapterHandle *adapter, struct RsdHandshakeHandle *handshake);
// Deletes bundle_id and its data from the device: {ok}.
char *apparition_uninstall(struct AdapterHandle *adapter, struct RsdHandshakeHandle *handshake,
                           const char *bundle_id);
// Copies the data of the installed app `from` to the installed app `to` (the same app signed
// under another bundle ID) through house_arrest: {ok, files, bytes, folders, skipped, failed, ms}.
// Both must be development-signed. Remove the old copy only when failed is empty.
char *apparition_migrate_container(struct AdapterHandle *adapter, struct RsdHandshakeHandle *handshake,
                                   const char *from, const char *to);
// Launches bundle_id suspended, attaches debugserver over the tunnel and serves the
// brk #0x69 (UTM) and brk #0xf00d (JIT26) protocols. Waits up to wait_seconds (0: no limit)
// for the first request; once the app has asked, serves it until it exits or detaches.
// {ok, pid, events, preparedBytes, detached, ended}.
char *apparition_launch_jit(struct AdapterHandle *adapter, struct RsdHandshakeHandle *handshake,
                        const char *bundle_id, unsigned int wait_seconds);
// Asks the session serving key (a bundle ID or a pid) to detach now, leaving the app running;
// its call then returns with ended "stopped". {ok, stopped}.
char *apparition_jit_stop(const char *key);
// The same for the running process pid (0: bundle_id's running process). With bundle_id,
// that app is brought back to the front once attached. bundle_id may be null if pid isn't 0.
char *apparition_attach_jit(struct AdapterHandle *adapter, struct RsdHandshakeHandle *handshake,
                        unsigned long long pid, const char *bundle_id, unsigned int wait_seconds);

// Opens the RemotePairing tunnel at device_host:device_port, or, when relay ("ip:port") is
// non-null, through a companion (Apparition on a computer) that connects back to this device (device_host is
// then ignored). Fills the same handles as idevice's tunnel_create_rppairing; free them with
// adapter_free / rsd_handshake_free. Returns {ok} or {ok: false, error}. With pin null it only
// pair-verifies: a pairing the device turns down fails at once with code "pairingRejected",
// rather than starting a pair-setup that waits for a PIN.
char *apparition_tunnel_create(const char *device_host, unsigned short device_port, const char *relay,
                           const char *hostname, struct RpPairingFileHandle *pairing_file,
                           const char *(*pin)(void *context), void *pin_context,
                           struct AdapterHandle **out_adapter, struct RsdHandshakeHandle **out_handshake);

// Pair-setup (or pair-verify) with the RemotePairing service on `port` through the companion at
// relay ("ip:port"), without a tunnel; updates pairing_file in place. Returns {ok} or {ok: false, error}.
char *apparition_pair_relay(const char *relay, unsigned short port, const char *hostname,
                            struct RpPairingFileHandle *pairing_file,
                            const char *(*pin)(void *context), void *pin_context);

// Packs the signed app at app_path into one IPA (reusing source_ipa's unchanged entries when it is
// non-null), uploads it and installs it; far fewer bytes than apparition_install's file-by-file upload.
// detach_after_ms > 0 stops waiting that long after the install request and replies queued: true
// (an update of the calling app itself waits in installd until the app closes).
char *apparition_install_ipa(struct AdapterHandle *adapter, struct RsdHandshakeHandle *handshake,
                             const char *app_path, const char *source_ipa, uint64_t detach_after_ms);

// The account's development certificates and App IDs (core/src/account.rs). Revoking a
// certificate stops apps signed with it from launching; ask the user first.
char *apparition_list_certificates(void);
char *apparition_revoke_certificate(const char *serial_number);
char *apparition_list_app_ids(void);
char *apparition_delete_app_id(const char *app_id_id);

// Official releases (core/src/release.rs), verified against the pinned release keys. site and
// signers may be NULL (apparition.illixion.com, the compiled-in keys); only development builds pass
// others. platform is "visionos", "ios" or "tvos".
// {ok, version, build, date, notes, signer, asset: {name, size} | null}
char *apparition_release_latest(const char *site, const char *signers, const char *platform);
// Downloads the latest release's app into dir (emptied first), checks it against the signed
// manifest and unpacks it: {ok, version, build, ipaPath, appPath}.
char *apparition_release_download_app(const char *site, const char *signers, const char *platform,
                                      const char *dir);

#endif
