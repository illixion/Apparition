//! Just enough DNS over HTTPS (RFC 8484, wire format) to read a host's HTTPS record (RFC 9460):
//! its ECH config and address hints. Asked of an endpoint by IP (`https://1.1.1.1/dns-query`), so
//! reaching the resolver sends no name at all, and the lookup never touches the system's DNS.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use base64::prelude::*;

use rustls::client::danger::ServerCertVerifier;

use crate::error::{Error, Result, fail};

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const TYPE_HTTPS: u16 = 65;

/// What a host's HTTPS record says: the ECH config list (raw, as `ECHConfigList`) and where it lives.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Https {
    pub ech: Option<Vec<u8>>,
    pub addrs: Vec<IpAddr>,
}

pub(crate) fn build_query(name: &str, rtype: u16) -> Result<Vec<u8>> {
    // id 0 (RFC 8484 asks for it, for cacheability), recursion desired, one question.
    let mut query = vec![0, 0, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            fail!("{name} isn't a host name");
        }
        query.push(label.len() as u8);
        query.extend_from_slice(label.as_bytes());
    }
    query.push(0);
    query.extend_from_slice(&rtype.to_be_bytes());
    query.extend_from_slice(&1u16.to_be_bytes()); // class IN
    Ok(query)
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16> {
    bytes.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]])).ok_or_else(|| Error::new("truncated DNS answer"))
}

/// The position after the name at `at` (a compression pointer ends it).
fn skip_name(message: &[u8], mut at: usize) -> Result<usize> {
    for _ in 0..128 {
        let len = *message.get(at).ok_or("truncated DNS name")? as usize;
        if len == 0 {
            return Ok(at + 1);
        }
        if len & 0xC0 == 0xC0 {
            return Ok(at + 2);
        }
        at += 1 + len;
    }
    fail!("DNS name too long")
}

/// The answers of `message` as (type, rdata), after checking it is a successful response.
fn answers(message: &[u8]) -> Result<Vec<(u16, &[u8])>> {
    if message.len() < 12 || message[2] & 0x80 == 0 {
        fail!("not a DNS response");
    }
    if message[3] & 0x0F != 0 {
        fail!("the resolver answered with error {}", message[3] & 0x0F);
    }
    let (questions, count) = (u16_at(message, 4)?, u16_at(message, 6)?);
    let mut at = 12;
    for _ in 0..questions {
        at = skip_name(message, at)? + 4;
    }
    let mut found = Vec::new();
    for _ in 0..count {
        at = skip_name(message, at)?;
        let rtype = u16_at(message, at)?;
        let len = u16_at(message, at + 8)? as usize;
        let rdata = message.get(at + 10..at + 10 + len).ok_or("truncated DNS record")?;
        found.push((rtype, rdata));
        at += 10 + len;
    }
    Ok(found)
}

/// The best (lowest priority) service-mode HTTPS record in `message`.
pub(crate) fn parse_https(message: &[u8]) -> Result<Https> {
    let mut best: Option<(u16, Https)> = None;
    for (rtype, rdata) in answers(message)? {
        if rtype != TYPE_HTTPS {
            continue;
        }
        let priority = u16_at(rdata, 0)?;
        if priority == 0 {
            continue; // alias mode: another name holds the parameters
        }
        let mut https = Https::default();
        let mut at = skip_name(rdata, 2)?;
        while at < rdata.len() {
            let (key, len) = (u16_at(rdata, at)?, u16_at(rdata, at + 2)? as usize);
            let value = rdata.get(at + 4..at + 4 + len).ok_or("truncated HTTPS record")?;
            match key {
                4 => https.addrs.extend(value.chunks_exact(4).map(|c| IpAddr::V4(Ipv4Addr::new(c[0], c[1], c[2], c[3])))),
                5 => https.ech = Some(value.to_vec()),
                6 => https.addrs.extend(value.chunks_exact(16).map(|c| IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(c).expect("16 bytes"))))),
                _ => {}
            }
            at += 4 + len;
        }
        if best.as_ref().is_none_or(|(p, _)| priority < *p) {
            best = Some((priority, https));
        }
    }
    best.map(|(_, https)| https).ok_or_else(|| Error::new("the host has no HTTPS record"))
}

fn parse_addrs(message: &[u8]) -> Result<Vec<IpAddr>> {
    Ok(answers(message)?
        .into_iter()
        .filter_map(|(rtype, rdata)| match (rtype, rdata.len()) {
            (TYPE_A, 4) => Some(IpAddr::V4(Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]))),
            (TYPE_AAAA, 16) => Some(IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(rdata).expect("16 bytes")))),
            _ => None,
        })
        .collect())
}

async fn ask(client: &reqwest::Client, endpoint: &str, name: &str, rtype: u16) -> Result<Vec<u8>> {
    let query = BASE64_URL_SAFE_NO_PAD.encode(build_query(name, rtype)?);
    let url = format!("{endpoint}?dns={query}");
    let mut response = client
        .get(&url)
        .header("accept", "application/dns-message")
        .send()
        .await
        .map_err(|e| format!("can't reach {endpoint}: {e}"))?;
    if !response.status().is_success() {
        fail!("{endpoint}: {}", response.status());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("{endpoint}: {e}"))? {
        if body.len() + chunk.len() > 64 * 1024 {
            fail!("{endpoint} sent too large an answer");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// `host`'s HTTPS record through the first endpoint that answers, with its addresses (the record's
/// hints, else an A and AAAA lookup through the same endpoint).
pub(crate) async fn lookup(host: &str, endpoints: &[String], user_agent: &str, verifier: Arc<dyn ServerCertVerifier>) -> Result<Https> {
    let client = reqwest::Client::builder()
        .tls_backend_preconfigured(crate::tls::client_config(verifier)?)
        .user_agent(user_agent)
        // Straight to the resolver's IP: a proxy from the environment would see which name we ask.
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| Error::new(e.to_string()))?;
    let mut failures = Vec::new();
    for endpoint in endpoints {
        let attempt = async {
            let mut https = parse_https(&ask(&client, endpoint, host, TYPE_HTTPS).await?)?;
            if https.addrs.is_empty() {
                for rtype in [TYPE_A, TYPE_AAAA] {
                    https.addrs.extend(parse_addrs(&ask(&client, endpoint, host, rtype).await?)?);
                }
            }
            Ok::<_, Error>(https)
        };
        match attempt.await {
            Ok(https) => return Ok(https),
            Err(error) => failures.push(error.to_string()),
        }
    }
    fail!("no DNS-over-HTTPS resolver answered for {host}: {}", failures.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real answer of 1.1.1.1 for apps.illixion.com's HTTPS record.
    const APPS: &[u8] = include_bytes!("../tests/fixtures/apps-https.bin");

    #[test]
    fn the_query_is_what_a_resolver_expects() {
        assert_eq!(
            hex(&build_query("apps.illixion.com", TYPE_HTTPS).unwrap()),
            "000001000001000000000000046170707308696c6c6978696f6e03636f6d0000410001"
        );
        assert!(build_query("a..b", TYPE_A).is_err());
        assert!(build_query(&"x".repeat(64), TYPE_A).is_err());
    }

    #[test]
    fn reads_the_ech_config_and_hints_from_a_real_answer() {
        let https = parse_https(APPS).unwrap();
        let ech = https.ech.expect("apps.illixion.com publishes an ECH config");
        // ECHConfigList: u16 length, then a config of version 0xfe0d.
        assert_eq!(u16::from_be_bytes([ech[0], ech[1]]) as usize, ech.len() - 2);
        assert_eq!(&ech[2..4], &[0xfe, 0x0d]);
        assert!(ech.windows(18).any(|w| w == b"cloudflare-ech.com".as_slice()), "Cloudflare's shared public name");
        assert_eq!(https.addrs.len(), 4);
        assert!(https.addrs.contains(&"104.21.28.28".parse().unwrap()));
        assert!(https.addrs.iter().any(IpAddr::is_ipv6));
    }

    #[test]
    fn damaged_answers_are_errors_not_panics() {
        for cut in 0..APPS.len() {
            let _ = parse_https(&APPS[..cut]); // must not panic
        }
        assert!(parse_https(&APPS[..40]).is_err());
        let mut error = APPS.to_vec();
        error[3] = 0x83; // NXDOMAIN
        assert!(parse_https(&error).unwrap_err().to_string().contains("error 3"));
        let mut query = APPS.to_vec();
        query[2] &= 0x7f; // not a response
        assert!(parse_https(&query).is_err());
        // A name that points at itself ends after a bounded number of steps.
        let mut looping = vec![0u8; 12];
        looping[5] = 1;
        looping.extend_from_slice(&[0xC0, 12]);
        assert!(parse_https(&looping).is_err());
    }

    #[test]
    fn a_host_with_no_record_is_an_error() {
        let mut empty = APPS[..12].to_vec();
        empty[7] = 0; // no answers
        empty.extend_from_slice(&APPS[12..12 + 17 + 4]);
        assert!(parse_https(&empty).unwrap_err().to_string().contains("no HTTPS record"));
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
