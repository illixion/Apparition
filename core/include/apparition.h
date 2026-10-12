// Apparition core C ABI (core/src/lib.rs). Every call blocks; call off the main thread.
// Strings returned by apparition_* calls are JSON and must be freed with apparition_string_free.
#ifndef APPARITION_H
#define APPARITION_H

#include <stdbool.h>

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
char *apparition_sign(const char *app_path, const char *udid, const char *device_name,
                  bool strip_extensions);
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

// Imports a .apparitionsigning file: an Apple ID's certificate key shared by another Apparition
// install (the desktop installer), so both use one certificate. Ends the signed-in session.
// Returns {ok, kind, appleId, teamId, from, replaced}.
char *apparition_import_signing_file(const char *data_dir, const char *path);

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
// Launches bundle_id suspended, attaches debugserver over the tunnel and serves the
// brk #0x69 (UTM) and brk #0xf00d (JIT26) protocols, waiting up to wait_seconds.
char *apparition_launch_jit(struct AdapterHandle *adapter, struct RsdHandshakeHandle *handshake,
                        const char *bundle_id, unsigned int wait_seconds);

// Opens the RemotePairing tunnel at device_host:device_port, or, when relay ("ip:port") is
// non-null, through a companion (Apparition on a computer) that connects back to this device (device_host is
// then ignored). Fills the same handles as idevice's tunnel_create_rppairing; free them with
// adapter_free / rsd_handshake_free. Returns {ok} or {ok: false, error}.
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
