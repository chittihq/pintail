//! Connection handshake and authentication.
//!
//! The server opens with `HandshakeV10` carrying a random scramble; the
//! client answers with `HandshakeResponse41` carrying its capabilities and a
//! digest of its password mixed with that scramble. The password never
//! crosses the wire, so verification recomputes the digest from a stored
//! hash rather than comparing secrets.

use sha1::{Digest as _, Sha1};
use sha2::{Digest as _, Sha256};

use crate::packet::{length_encoded_bytes, put_length_encoded_bytes};

/// Scramble length both supported plugins use.
pub const SCRAMBLE_SIZE: usize = 20;

/// Capability bits negotiated during the handshake.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CapabilityFlags(u32);

impl CapabilityFlags {
    /// Long password support; set by every modern client.
    pub const CLIENT_LONG_PASSWORD: Self = Self(0x0000_0001);
    /// Client sends a default schema in the handshake.
    pub const CLIENT_CONNECT_WITH_DB: Self = Self(0x0000_0008);
    /// Client understands the 4.1 protocol. Required.
    pub const CLIENT_PROTOCOL_41: Self = Self(0x0000_0200);
    /// Client requests a TLS upgrade before sending credentials.
    pub const CLIENT_SSL: Self = Self(0x0000_0800);
    /// 4.1-style authentication.
    pub const CLIENT_SECURE_CONNECTION: Self = Self(0x0000_8000);
    /// Client can be told which authentication plugin to use.
    pub const CLIENT_PLUGIN_AUTH: Self = Self(0x0008_0000);
    /// Client sends connection attributes after the auth response.
    pub const CLIENT_CONNECT_ATTRS: Self = Self(0x0010_0000);
    /// Auth response is length-encoded rather than one length byte.
    pub const CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA: Self = Self(0x0020_0000);
    /// EOF packets are replaced by OK packets. Changes how every result set
    /// terminates, so it must be honoured on write.
    pub const CLIENT_DEPRECATE_EOF: Self = Self(0x0100_0000);

    /// An empty set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Builds from raw protocol bits.
    #[must_use]
    pub const fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// The raw protocol bits.
    #[must_use]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether every bit in `other` is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl std::ops::BitOr for CapabilityFlags {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Everything the server advertises in its opening packet.
pub struct Handshake<'a> {
    /// Version string clients surface as `@@version`.
    pub server_version: &'a str,
    /// Connection id, echoed by `CONNECTION_ID()`.
    pub connection_id: u32,
    /// Random scramble the client mixes into its digest.
    pub scramble: [u8; SCRAMBLE_SIZE],
    /// Capabilities the server offers.
    pub capabilities: CapabilityFlags,
    /// Default collation id.
    pub character_set: u8,
    /// Plugin name the client should use.
    pub auth_plugin: &'a str,
}

impl Handshake<'_> {
    /// Encodes the opening `HandshakeV10` payload.
    ///
    /// The scramble is split: eight bytes before the reserved field and the
    /// remainder after it, a layout inherited from before scrambles grew.
    /// Clients rejoin the halves, so sending it contiguously fails
    /// authentication with no useful diagnostic.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = vec![10];
        payload.extend_from_slice(self.server_version.as_bytes());
        payload.push(0);
        payload.extend_from_slice(&self.connection_id.to_le_bytes());
        payload.extend_from_slice(&self.scramble[..8]);
        payload.push(0);
        let capabilities = self.capabilities.bits();
        payload.extend_from_slice(&capabilities.to_le_bytes()[..2]);
        payload.push(self.character_set);
        payload.extend_from_slice(&[0x02, 0x00]);
        payload.extend_from_slice(&capabilities.to_le_bytes()[2..]);
        payload.push(u8::try_from(SCRAMBLE_SIZE + 1).unwrap_or(21));
        payload.extend_from_slice(&[0_u8; 10]);
        payload.extend_from_slice(&self.scramble[8..]);
        payload.push(0);
        payload.extend_from_slice(self.auth_plugin.as_bytes());
        payload.push(0);
        payload
    }
}

/// The client's answer to the handshake.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HandshakeResponse {
    /// Capabilities the client accepted.
    pub capabilities: CapabilityFlags,
    /// Collation the client requested.
    pub character_set: u8,
    /// Username.
    pub username: Vec<u8>,
    /// Digest of the password mixed with the server's scramble.
    pub auth_response: Vec<u8>,
    /// Requested default schema, when the client sent one.
    pub database: Option<Vec<u8>>,
    /// Plugin the client actually used.
    pub auth_plugin: Option<Vec<u8>>,
}

impl HandshakeResponse {
    /// Parses a `HandshakeResponse41` payload.
    ///
    /// Returns `None` when the payload is truncated. A client that only
    /// negotiated TLS sends a short 32-byte packet and then repeats the full
    /// response over the encrypted stream; [`Self::is_ssl_request`] separates
    /// that case, which is not an error.
    #[must_use]
    pub fn parse(payload: &[u8]) -> Option<Self> {
        let raw = payload.get(..4)?;
        let capabilities =
            CapabilityFlags::from_bits(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]));
        let character_set = *payload.get(8)?;
        // 4 capability + 4 max packet + 1 charset + 23 reserved.
        let mut cursor = 32;
        let rest = payload.get(cursor..)?;
        let end = rest.iter().position(|byte| *byte == 0)?;
        let username = rest.get(..end)?.to_vec();
        cursor += end + 1;

        let rest = payload.get(cursor..)?;
        let (auth_response, consumed) =
            if capabilities.contains(CapabilityFlags::CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA) {
                let (value, consumed) = length_encoded_bytes(rest)?;
                (value.unwrap_or_default().to_vec(), consumed)
            } else {
                let length = usize::from(*rest.first()?);
                (rest.get(1..1 + length)?.to_vec(), 1 + length)
            };
        cursor += consumed;

        let database = if capabilities.contains(CapabilityFlags::CLIENT_CONNECT_WITH_DB) {
            let rest = payload.get(cursor..)?;
            let end = rest.iter().position(|byte| *byte == 0)?;
            cursor += end + 1;
            Some(rest.get(..end)?.to_vec())
        } else {
            None
        };

        let auth_plugin = if capabilities.contains(CapabilityFlags::CLIENT_PLUGIN_AUTH) {
            payload.get(cursor..).and_then(|rest| {
                let end = rest
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(rest.len());
                rest.get(..end).map(<[u8]>::to_vec)
            })
        } else {
            None
        };

        Some(Self {
            capabilities,
            character_set,
            username,
            auth_response,
            database,
            auth_plugin,
        })
    }

    /// Whether this payload is a bare TLS upgrade request rather than a full
    /// response. Such a packet carries capabilities and nothing else.
    #[must_use]
    pub fn is_ssl_request(payload: &[u8]) -> bool {
        payload.len() <= 32
            && payload.get(..4).is_some_and(|raw| {
                CapabilityFlags::from_bits(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
                    .contains(CapabilityFlags::CLIENT_SSL)
            })
    }
}

/// Appends a length-encoded auth response, the form modern clients send.
pub fn put_auth_response(output: &mut Vec<u8>, response: &[u8]) {
    put_length_encoded_bytes(output, response);
}

/// Verifies a `mysql_native_password` response.
///
/// The client sends `SHA1(password) XOR SHA1(scramble ++ SHA1(SHA1(password)))`.
/// The server stores only `SHA1(SHA1(password))`, so it recovers
/// `SHA1(password)` from the response and checks that hashing it again
/// reproduces the stored value — the plaintext is never needed.
#[must_use]
pub fn verify_native_password(response: &[u8], scramble: &[u8], stored_double_sha1: &[u8]) -> bool {
    if response.len() != SCRAMBLE_SIZE || stored_double_sha1.len() != SCRAMBLE_SIZE {
        return false;
    }
    let mut hasher = Sha1::new();
    hasher.update(scramble);
    hasher.update(stored_double_sha1);
    let mixed = hasher.finalize();
    let recovered: Vec<u8> = response
        .iter()
        .zip(mixed.iter())
        .map(|(left, right)| left ^ right)
        .collect();
    Sha1::digest(&recovered).as_slice() == stored_double_sha1
}

/// Verifies a `caching_sha2_password` fast-auth response.
///
/// The client sends `SHA256(password) XOR SHA256(SHA256(SHA256(password)) ++
/// scramble)`, so the server recovers `SHA256(password)` and checks its
/// double hash against the stored value.
#[must_use]
pub fn verify_caching_sha2(response: &[u8], scramble: &[u8], stored_double_sha256: &[u8]) -> bool {
    if response.len() != 32 || stored_double_sha256.len() != 32 {
        return false;
    }
    let mut hasher = Sha256::new();
    hasher.update(stored_double_sha256);
    hasher.update(scramble);
    let mixed = hasher.finalize();
    let recovered: Vec<u8> = response
        .iter()
        .zip(mixed.iter())
        .map(|(left, right)| left ^ right)
        .collect();
    Sha256::digest(&recovered).as_slice() == stored_double_sha256
}

#[cfg(test)]
mod tests {
    use super::{
        CapabilityFlags, Handshake, HandshakeResponse, SCRAMBLE_SIZE, verify_caching_sha2,
        verify_native_password,
    };
    use sha1::{Digest as _, Sha1};
    use sha2::{Digest as _, Sha256};

    fn scramble() -> [u8; SCRAMBLE_SIZE] {
        let mut value = [0_u8; SCRAMBLE_SIZE];
        for (index, byte) in value.iter_mut().enumerate() {
            *byte = u8::try_from(index).unwrap_or(0).wrapping_add(1);
        }
        value
    }

    #[test]
    fn the_handshake_splits_its_scramble_the_way_clients_rejoin_it() {
        let handshake = Handshake {
            server_version: "8.4.0-pintail",
            connection_id: 7,
            scramble: scramble(),
            capabilities: CapabilityFlags::CLIENT_PROTOCOL_41 | CapabilityFlags::CLIENT_PLUGIN_AUTH,
            character_set: 255,
            auth_plugin: "mysql_native_password",
        };
        let encoded = handshake.encode();
        assert_eq!(encoded[0], 10);
        // Version, NUL, then the connection id.
        let version_end = 1 + "8.4.0-pintail".len();
        assert_eq!(encoded[version_end], 0);
        // First eight scramble bytes sit immediately after the id, and the
        // remaining twelve follow the ten reserved bytes.
        let first = version_end + 1 + 4;
        assert_eq!(&encoded[first..first + 8], &scramble()[..8]);
        let second = first + 8 + 1 + 2 + 1 + 2 + 2 + 1 + 10;
        assert_eq!(&encoded[second..second + 12], &scramble()[8..]);
        assert!(encoded.ends_with(b"mysql_native_password\0"));
    }

    fn response_payload(capabilities: CapabilityFlags, auth: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&capabilities.bits().to_le_bytes());
        payload.extend_from_slice(&16_777_216_u32.to_le_bytes());
        payload.push(45);
        payload.extend_from_slice(&[0_u8; 23]);
        payload.extend_from_slice(b"analytics\0");
        payload.push(u8::try_from(auth.len()).unwrap_or(0));
        payload.extend_from_slice(auth);
        payload.extend_from_slice(b"reporting\0");
        payload.extend_from_slice(b"mysql_native_password\0");
        payload
    }

    #[test]
    fn a_full_response_yields_username_database_and_plugin() {
        let capabilities = CapabilityFlags::CLIENT_PROTOCOL_41
            | CapabilityFlags::CLIENT_CONNECT_WITH_DB
            | CapabilityFlags::CLIENT_PLUGIN_AUTH;
        let parsed = HandshakeResponse::parse(&response_payload(capabilities, b"0123456789"))
            .expect("parse");
        assert_eq!(parsed.username, b"analytics");
        assert_eq!(parsed.auth_response, b"0123456789");
        assert_eq!(parsed.database.as_deref(), Some(&b"reporting"[..]));
        assert_eq!(
            parsed.auth_plugin.as_deref(),
            Some(&b"mysql_native_password"[..])
        );
        assert_eq!(parsed.character_set, 45);
    }

    #[test]
    fn a_tls_upgrade_request_is_recognised_rather_than_rejected() {
        let mut payload = Vec::new();
        payload.extend_from_slice(
            &(CapabilityFlags::CLIENT_PROTOCOL_41 | CapabilityFlags::CLIENT_SSL)
                .bits()
                .to_le_bytes(),
        );
        payload.extend_from_slice(&16_777_216_u32.to_le_bytes());
        payload.push(45);
        payload.extend_from_slice(&[0_u8; 23]);
        assert!(HandshakeResponse::is_ssl_request(&payload));
        // A full response must not be mistaken for one.
        assert!(!HandshakeResponse::is_ssl_request(&response_payload(
            CapabilityFlags::CLIENT_PROTOCOL_41,
            b"0123456789"
        )));
    }

    #[test]
    fn a_truncated_response_reports_none_instead_of_panicking() {
        for length in 0..32 {
            assert!(
                HandshakeResponse::parse(&vec![0_u8; length]).is_none(),
                "{length}"
            );
        }
    }

    #[test]
    fn native_password_verifies_without_ever_holding_the_password() {
        let password = b"pk_wire_secret";
        let stage1 = Sha1::digest(password);
        let stored = Sha1::digest(stage1);
        let mut hasher = Sha1::new();
        hasher.update(scramble());
        hasher.update(stored);
        let mixed = hasher.finalize();
        let response: Vec<u8> = stage1
            .iter()
            .zip(mixed.iter())
            .map(|(left, right)| left ^ right)
            .collect();

        assert!(verify_native_password(&response, &scramble(), &stored));
        // A wrong password, a wrong scramble, and a wrong length all fail.
        let mut wrong = response.clone();
        wrong[0] ^= 0xff;
        assert!(!verify_native_password(&wrong, &scramble(), &stored));
        assert!(!verify_native_password(&response, &[0_u8; 20], &stored));
        assert!(!verify_native_password(
            &response[..19],
            &scramble(),
            &stored
        ));
    }

    #[test]
    fn caching_sha2_verifies_its_fast_auth_response() {
        let password = b"pk_wire_secret";
        let stage1 = Sha256::digest(password);
        let stored = Sha256::digest(stage1);
        let mut hasher = Sha256::new();
        hasher.update(stored);
        hasher.update(scramble());
        let mixed = hasher.finalize();
        let response: Vec<u8> = stage1
            .iter()
            .zip(mixed.iter())
            .map(|(left, right)| left ^ right)
            .collect();

        assert!(verify_caching_sha2(&response, &scramble(), &stored));
        let mut wrong = response.clone();
        wrong[31] ^= 0x01;
        assert!(!verify_caching_sha2(&wrong, &scramble(), &stored));
    }
}
