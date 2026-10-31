//! Keys and hashes: text keys, translation IDs, salted IP hashes.
//!
//! Raw IPs are never stored: only `HMAC-SHA256(IP_SALT, ...)` of the address
//! and of its network prefix (IPv4 /24, IPv6 /48), so a whole range can be
//! banned without knowing the addresses in it.

use std::net::IpAddr;

use sha2::{Digest, Sha256};

pub fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&sha256(data))
}

/// Key of an English text: first 128 bits of SHA-256 of its UTF-8 bytes, hex.
pub fn text_key(en: &str) -> String {
    hex(&sha256(en.as_bytes())[..16])
}

/// ID of a translation: same translation of the same text in the same
/// language always gets the same ID (duplicates from many players merge).
pub fn translation_id(text_key: &str, lang: &str, tr: &str) -> String {
    let mut h = Sha256::new();
    for part in [text_key, lang, tr] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    hex(&h.finalize()[..16])
}

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// Network prefix of an address as text: `a.b.c.0/24` or the first 48 bits of
/// an IPv6 address (`x:y:z::/48`). IPv4-mapped IPv6 counts as IPv4.
pub fn net_prefix(ip: &IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.{}.0/24", o[0], o[1], o[2])
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => net_prefix(&IpAddr::V4(v4)),
            None => {
                let s = v6.segments();
                format!("{:x}:{:x}:{:x}::/48", s[0], s[1], s[2])
            }
        },
    }
}

/// Salted hashes of a client address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpHashes {
    /// Hash of the exact address (votes, reporters, rate limits).
    pub ip: String,
    /// Hash of its network prefix (range bans).
    pub net: String,
}

/// Hashes the client address with the secret salt. An unparsable address is
/// hashed as given (both values equal).
pub fn ip_hashes(salt: &str, ip: &str) -> IpHashes {
    let ip = ip.trim();
    match ip.parse::<IpAddr>() {
        Ok(addr) => {
            let canonical = match addr {
                IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(addr),
                a => a,
            };
            IpHashes {
                ip: hash_ip_text(salt, &canonical.to_string()),
                net: hash_net_text(salt, &net_prefix(&canonical)),
            }
        }
        Err(_) => {
            let h = hash_ip_text(salt, ip);
            IpHashes { ip: h.clone(), net: h }
        }
    }
}

pub fn hash_ip_text(salt: &str, ip: &str) -> String {
    hex(&hmac_sha256(salt.as_bytes(), format!("ip:{ip}").as_bytes())[..16])
}

pub fn hash_net_text(salt: &str, prefix: &str) -> String {
    hex(&hmac_sha256(salt.as_bytes(), format!("net:{prefix}").as_bytes())[..16])
}

/// Constant-time comparison (admin token).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A 32-char lowercase hex key (text keys, translation IDs, IP hashes).
pub fn is_key(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn keys_are_stable_and_distinct() {
        assert_eq!(text_key("Ingredient"), text_key("Ingredient"));
        assert_ne!(text_key("Ingredient"), text_key("Ingredient "));
        assert!(is_key(&text_key("x")));
        let k = text_key("Sword");
        assert_ne!(
            translation_id(&k, "pt-BR", "Espada"),
            translation_id(&k, "es", "Espada")
        );
        // The separator prevents ambiguity between fields.
        assert_ne!(translation_id("a", "bc", "d"), translation_id("ab", "c", "d"));
    }

    #[test]
    fn ip_hashes_group_ranges_and_never_contain_the_ip() {
        let a = ip_hashes("salt", "203.0.113.7");
        let b = ip_hashes("salt", "203.0.113.200");
        let c = ip_hashes("salt", "::ffff:203.0.113.7");
        assert_ne!(a.ip, b.ip);
        assert_eq!(a.net, b.net);
        assert_eq!(a, c);
        assert!(!a.ip.contains("203"));
        assert_ne!(ip_hashes("other", "203.0.113.7").ip, a.ip);
        let v6 = ip_hashes("salt", "2001:db8:1234:5678::1");
        assert_eq!(v6.net, ip_hashes("salt", "2001:db8:1234:ffff::2").net);
        assert_eq!(net_prefix(&"2001:db8:1234::1".parse().unwrap()), "2001:db8:1234::/48");
    }

    #[test]
    fn constant_time_eq() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }
}
