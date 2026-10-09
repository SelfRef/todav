//! WebDAV-Push (dav_push): subscription keys, registration, RFC 8291 decryption, ntfy listener.

use crate::caldav::{Element, PUSH, xml_escape};
use crate::json::Json;
use crate::{Client, Error, Result, b64decode, b64encode, store};
use ring::{aead, hkdf};
use rusqlite::params;
use std::io::BufRead;
use std::sync::atomic::Ordering;

#[derive(Debug, Clone)]
pub struct PushRegistration {
    pub list_href: String,
    pub registration_href: String,
    pub expires: i64,
}

/// dav_push clamps expiry to one week.
const MAX_EXPIRY: i64 = 7 * 86400;

impl Client {
    /// Register (or refresh) a web-push subscription for every list that advertises a push topic.
    /// Lists whose registration for this resource still has >25% of its lifetime left are skipped.
    pub fn push_register(
        &self,
        push_resource: String,
        expires_in_secs: u64,
    ) -> Result<Vec<PushRegistration>> {
        let dav = self.dav()?;
        let lifetime = (expires_in_secs as i64).min(MAX_EXPIRY);
        let (public, auth) = self.push_keys()?;
        let (lists, old_resource) = {
            let db = self.db.lock().unwrap();
            let mut st = db.prepare(
                "SELECT href, push_registration_href, push_expires FROM lists WHERE push_topic IS NOT NULL",
            )?;
            let lists: Vec<(String, Option<String>, Option<i64>)> = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            (lists, store::kv_get(&db, "push_resource")?)
        };
        if old_resource.as_deref().is_some_and(|r| r != push_resource) {
            self.push_unregister()?;
            return self.push_register(push_resource, expires_in_secs);
        }
        let now = crate::now();
        let mut out = Vec::new();
        for (href, reg, expires) in lists {
            if let (Some(reg), Some(exp)) = (&reg, expires)
                && exp - now > lifetime / 4
            {
                out.push(PushRegistration {
                    list_href: href,
                    registration_href: reg.clone(),
                    expires: exp,
                });
                continue;
            }
            let expires = now + lifetime;
            let body = format!(
                r#"<?xml version="1.0" encoding="utf-8"?><push-register xmlns="{PUSH}"><subscription><web-push-subscription><push-resource>{}</push-resource><subscription-public-key type="p256dh">{public}</subscription-public-key><auth-secret>{auth}</auth-secret></web-push-subscription></subscription><expires>{}</expires></push-register>"#,
                xml_escape(&push_resource),
                http_date(expires)
            );
            let r = dav.request("POST", &href, &[], &body)?;
            let Some(location) = r.location.filter(|_| (200..300).contains(&r.status)) else {
                return Err(Error::Http(r.status, r.body));
            };
            let db = self.db.lock().unwrap();
            db.execute(
                "UPDATE lists SET push_registration_href = ?2, push_expires = ?3 WHERE href = ?1",
                params![href, location, expires],
            )?;
            store::kv_set(&db, "push_resource", &push_resource)?;
            out.push(PushRegistration {
                list_href: href,
                registration_href: location,
                expires,
            });
        }
        Ok(out)
    }

    pub fn push_unregister(&self) -> Result<()> {
        let dav = self.dav()?;
        let regs: Vec<String> = {
            let db = self.db.lock().unwrap();
            let mut st = db.prepare(
                "SELECT push_registration_href FROM lists WHERE push_registration_href IS NOT NULL",
            )?;
            st.query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        for reg in regs {
            let r = dav.request("DELETE", &reg, &[], "")?;
            if !(200..300).contains(&r.status) && r.status != 404 {
                return Err(Error::Http(r.status, r.body));
            }
        }
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE lists SET push_registration_href = NULL, push_expires = NULL",
            [],
        )?;
        db.execute("DELETE FROM kv WHERE key = 'push_resource'", [])?;
        Ok(())
    }

    /// Topics from a push message body: plaintext `<push-message>` XML or an aes128gcm-encrypted one.
    pub fn push_decode(&self, body: Vec<u8>) -> Result<Vec<String>> {
        let plain = if body.first() == Some(&b'<') {
            body
        } else {
            let db = self.db.lock().unwrap();
            let key =
                store::kv_get(&db, "push_private")?.ok_or(Error::NotFound("push keys".into()))?;
            let auth =
                store::kv_get(&db, "push_auth")?.ok_or(Error::NotFound("push keys".into()))?;
            decrypt(&b64decode(&key)?, &b64decode(&auth)?, &body)?
        };
        let xml = Element::parse(&String::from_utf8_lossy(&plain))?;
        Ok(xml
            .children(PUSH, "topic")
            .map(|t| t.text.trim().to_string())
            .collect())
    }

    /// (public key, auth secret) as base64url, generated once and kept in kv.
    fn push_keys(&self) -> Result<(String, String)> {
        let db = self.db.lock().unwrap();
        let private = match store::kv_get(&db, "push_private")? {
            Some(k) => b64decode(&k)?,
            None => {
                let k = loop {
                    let b = crate::random_bytes::<32>();
                    if p256::SecretKey::from_slice(&b).is_ok() {
                        break b.to_vec();
                    }
                };
                store::kv_set(&db, "push_private", &b64encode(&k, true))?;
                store::kv_set(
                    &db,
                    "push_auth",
                    &b64encode(&crate::random_bytes::<16>(), true),
                )?;
                k
            }
        };
        let auth = store::kv_get(&db, "push_auth")?.unwrap_or_default();
        Ok((b64encode(&public_key(&private)?, true), auth))
    }

    /// Make running `listen` calls return. They act on nothing from now on, but each holds its
    /// connection until the next ntfy keepalive (up to 45 s) before returning.
    pub fn stop_listening(&self) {
        self.listen_gen.fetch_add(1, Ordering::SeqCst);
    }

    /// Blocking: register with an ntfy topic and sync on every message. Reconnects until `stop_listening`.
    pub fn listen(
        &self,
        ntfy_url: String,
        on_event: &dyn Fn(Result<crate::SyncReport>),
    ) -> Result<()> {
        let topic = {
            let db = self.db.lock().unwrap();
            match store::kv_get(&db, "ntfy_topic")? {
                Some(t) => t,
                None => {
                    let t = format!("todav{}", b64encode(&crate::random_bytes::<18>(), true));
                    store::kv_set(&db, "ntfy_topic", &t)?;
                    t
                }
            }
        };
        let base = ntfy_url.trim_end_matches('/');
        let resource = format!("{base}/{topic}?up=1");
        let mut since = "all".to_string();
        let mut last_register = 0;
        let started = self.listen_gen.load(Ordering::SeqCst);
        let stopped = || self.listen_gen.load(Ordering::SeqCst) != started;
        loop {
            if stopped() {
                return Ok(());
            }
            if crate::now() - last_register > 3600 {
                on_event(
                    self.push_register(resource.clone(), MAX_EXPIRY as u64)
                        .map(|_| Default::default()),
                );
                last_register = crate::now();
            }
            let res = ntfy_stream(
                &format!("{base}/{topic}/json?since={since}"),
                &stopped,
                &mut |id, body| {
                    since = id.to_string();
                    let r = self.push_decode(body).and_then(|topics| {
                        let mut total = crate::SyncReport::default();
                        for t in topics {
                            let r = self.sync_topic(t)?;
                            total.pulled += r.pulled;
                            total.pushed += r.pushed;
                            total.deleted += r.deleted;
                        }
                        Ok(total)
                    });
                    on_event(r);
                },
            );
            if stopped() {
                return Ok(());
            }
            on_event(res.map(|_| Default::default()));
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }
}

/// Read an ntfy JSON stream until it ends or `stop()`; calls `f(id, body)` for every message event.
fn ntfy_stream(url: &str, stop: &dyn Fn() -> bool, f: &mut dyn FnMut(&str, Vec<u8>)) -> Result<()> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(None)
        .timeout_recv_body(Some(std::time::Duration::from_secs(120))) // ntfy sends keepalives every 45 s
        .build()
        .new_agent();
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| Error::Net(e.to_string()))?;
    let reader = std::io::BufReader::new(resp.into_body().into_reader());
    for line in reader.lines() {
        let line = line.map_err(|e| Error::Net(e.to_string()))?;
        if stop() {
            return Ok(());
        }
        let Some(m) = Json::parse(&line) else {
            continue;
        };
        let field = |k: &str| m.get(k).and_then(Json::str).unwrap_or_default().to_string();
        if field("event") != "message" {
            continue;
        }
        let body = if field("encoding") == "base64" {
            b64decode(&field("message"))?
        } else {
            field("message").into_bytes()
        };
        f(&field("id"), body);
    }
    Ok(())
}

/// Check that `ntfy_url` answers like an ntfy server (`/v1/health`).
pub fn ntfy_check(ntfy_url: String) -> Result<()> {
    let url = format!("{}/v1/health", ntfy_url.trim_end_matches('/'));
    let mut r = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent("Todav")
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .new_agent()
        .get(&url)
        .call()
        .map_err(|e| Error::Net(e.to_string()))?;
    let status = r.status().as_u16();
    let body = r
        .body_mut()
        .read_to_string()
        .map_err(|e| Error::Net(e.to_string()))?;
    match Json::parse(&body).and_then(|j| j.get("healthy").cloned()) {
        Some(Json::Bool(true)) => Ok(()),
        _ => Err(Error::Http(status, "not a healthy ntfy server".into())),
    }
}

fn public_key(private: &[u8]) -> Result<Vec<u8>> {
    let sk = p256::SecretKey::from_slice(private).map_err(|e| Error::Parse(e.to_string()))?;
    Ok(sk.public_key().to_sec1_bytes().to_vec()) // uncompressed for P-256
}

struct Len(usize);
impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0; len];
    hkdf::Salt::new(hkdf::HKDF_SHA256, salt)
        .extract(ikm)
        .expand(&[info], Len(len))
        .and_then(|okm| okm.fill(&mut out))
        .expect("hkdf length is valid");
    out
}

/// RFC 8291 / RFC 8188 aes128gcm decryption with the subscription's private key and auth secret.
pub fn decrypt(private: &[u8], auth: &[u8], body: &[u8]) -> Result<Vec<u8>> {
    let bad = || Error::Parse("malformed aes128gcm push body".into());
    let salt = body.get(..16).ok_or_else(bad)?;
    let rs = u32::from_be_bytes(body.get(16..20).ok_or_else(bad)?.try_into().unwrap()) as usize;
    let idlen = *body.get(20).ok_or_else(bad)? as usize;
    let sender = body.get(21..21 + idlen).ok_or_else(bad)?;
    let ciphertext = &body[21 + idlen..];
    if rs < 18 {
        return Err(bad());
    }

    let sk = p256::SecretKey::from_slice(private).map_err(|_| bad())?;
    let pk = p256::PublicKey::from_sec1_bytes(sender).map_err(|_| bad())?;
    let shared = p256::ecdh::diffie_hellman(sk.to_nonzero_scalar(), pk.as_affine());
    let key_info = [b"WebPush: info\0".as_slice(), &public_key(private)?, sender].concat();
    let ikm = hkdf(auth, shared.raw_secret_bytes(), &key_info, 32);
    let cek = hkdf(salt, &ikm, b"Content-Encoding: aes128gcm\0", 16);
    let nonce = hkdf(salt, &ikm, b"Content-Encoding: nonce\0", 12);
    let key =
        aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek).map_err(|_| bad())?);

    let mut out = Vec::new();
    for (seq, record) in ciphertext.chunks(rs).enumerate() {
        let mut n: [u8; 12] = nonce.clone().try_into().unwrap();
        for (i, b) in (seq as u64).to_be_bytes().iter().enumerate() {
            n[4 + i] ^= b;
        }
        let mut buf = record.to_vec();
        let plain = key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(n),
                aead::Aad::empty(),
                &mut buf,
            )
            .map_err(|_| Error::Parse("push decryption failed (wrong keys?)".into()))?;
        // Strip padding: content, then a 0x01/0x02 delimiter, then zeros.
        let end = plain.iter().rposition(|&b| b != 0).ok_or_else(bad)?;
        out.extend_from_slice(&plain[..end]);
    }
    Ok(out)
}

/// RFC 7231 IMF-fixdate, e.g. `Thu, 09 Oct 2026 10:00:00 GMT`.
pub fn http_date(t: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let s = crate::ical::fmt_utc(t); // YYYYMMDDTHHMMSSZ
    let day = DAYS[t.div_euclid(86400).rem_euclid(7) as usize];
    let month = MONTHS[s[4..6].parse::<usize>().unwrap() - 1];
    format!(
        "{day}, {} {month} {} {}:{}:{} GMT",
        &s[6..8],
        &s[..4],
        &s[9..11],
        &s[11..13],
        &s[13..15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc8291_vector() {
        let ua_private = b64decode("q1dXpw3UpT5VOmu_cf_v6ih07Aems3njxI-JWgLcM94").unwrap();
        let auth = b64decode("BTBZMqHH6r4Tts7J_aSIgg").unwrap();
        let body = b64decode("DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN").unwrap();
        assert_eq!(
            b64encode(&public_key(&ua_private).unwrap(), true),
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4"
        );
        let plain = decrypt(&ua_private, &auth, &body).unwrap();
        assert_eq!(plain, b"When I grow up, I want to be a watermelon");
    }

    #[test]
    fn http_date_format() {
        assert_eq!(http_date(1791460800), "Thu, 08 Oct 2026 12:00:00 GMT");
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
    }
}
