//! The account's development certificates and App IDs, so the user can see what uses up a
//! free account's slots and get them back.
//!
//! A free account has room for two development certificates and ten App IDs a week. Every
//! installation of a sideloader makes its own certificate (its private key never leaves the
//! device that made it), so a reinstall, a second device or another tool (Xcode, SideStore)
//! can use both up, and signing then fails with Apple's error 7460. Nothing here revokes or
//! deletes on its own: the app asks the user first.

use std::ffi::c_char;

use isideload::dev::{app_ids::AppIdsApi, certificates::CertificatesApi, device_type::DeveloperDeviceType};
use rootcause::prelude::*;
use serde_json::{Value, json};

use crate::{SESSION, Session, reply, runtime, text};

/// The machine name this app's certificates are made under (see `apparition_login`).
pub const MACHINE_NAME: &str = "Apparition";

fn date(value: &Option<plist::Date>) -> Value {
    value.as_ref().map(|date| Value::String(date.to_xml_format())).unwrap_or(Value::Null)
}

/// Every development certificate on the team, whatever its platform, since all of them count
/// against the free account's two slots (a Mac certificate Xcode made fills one as well):
/// `{ok, certificates: [{name, serialNumber, machineName, status, expires, platform, typeName,
/// madeByApparition, inUseHere}]}`; `inUseHere` marks the certificate whose key this install keeps.
///
/// Apple's `ios/` listing (what isideload's `list_ios_certs` reads, then filters to iOS) and
/// the platform-less one Xcode uses for Mac certificates are both read and merged by serial
/// number; the second is best effort, since a free account may not be allowed it.
#[unsafe(no_mangle)]
pub extern "C" fn apparition_list_certificates() -> *mut c_char {
    reply((|| {
        let mut guard = SESSION.lock().unwrap();
        let session = guard.as_mut().ok_or_else(|| report!("Not signed in"))?;
        runtime().block_on(list_certificates(session))
    })())
}

/// See `apparition_list_certificates`.
pub async fn list_certificates(session: &mut Session) -> Result<Value, Report> {
    let team = session.team.clone();
    let dev = session.sideloader.get_dev_session();
    let mut certs = dev.list_all_development_certs(&team, DeveloperDeviceType::Ios).await?;
    match dev.list_all_development_certs(&team, DeveloperDeviceType::Any).await {
        Ok(more) => {
            for cert in more {
                let known = cert.serial_number.is_some()
                    && certs.iter().any(|c| c.serial_number == cert.serial_number);
                if !known {
                    certs.push(cert);
                }
            }
        }
        Err(error) => tracing::warn!("platform-less certificate listing failed: {error}"),
    }
    // The certificate whose key this install holds: the one it signs with.
    let ours = crate::share::stored_public_key(&session.data_dir, &session.email);
    let public_key = |der: &[u8]| {
        use x509_cert::der::Decode;
        x509_cert::Certificate::from_der(der)
            .ok()
            .and_then(|c| c.tbs_certificate.subject_public_key_info.subject_public_key.as_bytes().map(<[u8]>::to_vec))
    };
    let list: Vec<Value> = certs
        .iter()
        .map(|cert| {
            let kind = cert.certificate_type.as_ref();
            let in_use = ours.is_some() && cert.cert_content.as_ref().and_then(|c| public_key(c.as_ref())) == ours;
            json!({
                "name": cert.name,
                "serialNumber": cert.serial_number,
                "machineName": cert.machine_name,
                "status": cert.status,
                "expires": date(&cert.expiration_date),
                "platform": cert.certificate_platform.clone().or_else(|| kind.and_then(|k| k.platform.clone())),
                "typeName": kind.and_then(|k| k.name.clone()),
                "madeByApparition": cert.machine_name.as_deref() == Some(MACHINE_NAME),
                "inUseHere": in_use,
            })
        })
        .collect();
    Ok(json!({ "certificates": list }))
}

/// Revokes the development certificate with `serial_number`, of any platform. Apps signed
/// with it stop launching until they are signed again. Returns `{ok}`.
///
/// Tried through the `ios/` endpoint first, then the platform-less one, which is where a Mac
/// certificate lives; when both refuse, the first error is the one reported.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_revoke_certificate(serial_number: *const c_char) -> *mut c_char {
    reply((|| {
        let serial = unsafe { text(serial_number) }?;
        let mut guard = SESSION.lock().unwrap();
        let session = guard.as_mut().ok_or_else(|| report!("Not signed in"))?;
        runtime().block_on(revoke_certificate(session, &serial))
    })())
}

/// See `apparition_revoke_certificate`.
pub async fn revoke_certificate(session: &mut Session, serial: &str) -> Result<Value, Report> {
    let team = session.team.clone();
    let dev = session.sideloader.get_dev_session();
    if let Err(first) = dev.revoke_development_cert(&team, &serial, DeveloperDeviceType::Ios).await {
        if let Err(second) = dev.revoke_development_cert(&team, &serial, DeveloperDeviceType::Any).await {
            tracing::warn!("platform-less revoke failed too: {second}");
            return Err(first);
        }
    }
    Ok(json!({}))
}

/// The team's App IDs and how many more it may make:
/// `{ok, appIds: [{appIdId, identifier, name, expires}], maxQuantity, availableQuantity}`.
#[unsafe(no_mangle)]
pub extern "C" fn apparition_list_app_ids() -> *mut c_char {
    reply((|| {
        let mut guard = SESSION.lock().unwrap();
        let session = guard.as_mut().ok_or_else(|| report!("Not signed in"))?;
        runtime().block_on(list_app_ids(session))
    })())
}

/// See `apparition_list_app_ids`.
pub async fn list_app_ids(session: &mut Session) -> Result<Value, Report> {
    let team = session.team.clone();
    let response = session.sideloader.get_dev_session().list_app_ids(&team, None).await?;
    let list: Vec<Value> = response
        .app_ids
        .iter()
        .map(|id| {
            json!({
                "appIdId": id.app_id_id,
                "identifier": id.identifier,
                "name": id.name,
                "expires": date(&id.expiration_date),
            })
        })
        .collect();
    Ok(json!({
        "appIds": list,
        "maxQuantity": response.max_quantity,
        "availableQuantity": response.available_quantity,
    }))
}

/// Deletes an App ID (by its `appIdId`). On a free account Apple may refuse until the ID
/// expires, and a deleted ID may still count against the week's ten. Returns `{ok}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn apparition_delete_app_id(app_id_id: *const c_char) -> *mut c_char {
    reply((|| {
        let id = unsafe { text(app_id_id) }?;
        let mut guard = SESSION.lock().unwrap();
        let session = guard.as_mut().ok_or_else(|| report!("Not signed in"))?;
        runtime().block_on(delete_app_id(session, &id))
    })())
}

/// See `apparition_delete_app_id`.
pub async fn delete_app_id(session: &mut Session, id: &str) -> Result<Value, Report> {
    let team = session.team.clone();
    session.sideloader.get_dev_session().delete_app_id(&team, &id, None).await?;
    Ok(json!({}))
}
