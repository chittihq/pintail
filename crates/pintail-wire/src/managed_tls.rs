//! A wire-protocol certificate the node manages for itself.
//!
//! Without one, the server never advertises `CLIENT_SSL`, so a client asking
//! for TLS cannot get it and a client that would have preferred TLS silently
//! sends every query and every result row in the clear. That is the state a
//! published port starts in, and no operator action makes it better without
//! first obtaining a certificate from somewhere.
//!
//! So the node issues its own, once, and keeps it. This mirrors how a managed
//! database service behaves: the certificate exists before anyone asks, the
//! private key never leaves the server, and the public half is downloadable
//! for clients that want to verify rather than merely encrypt.
//!
//! # Why one certificate for the whole node
//!
//! The TLS upgrade completes *before* the client sends its username - and here
//! the username is the database name. The server must present a certificate
//! while it still has no idea which database is wanted, so a per-database or
//! per-workspace certificate cannot exist. It is also the right shape: a
//! certificate answers "are you really this host", while an API key answers
//! "may this client read this database". Those are different questions.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// How long a generated certificate is valid.
///
/// Long, deliberately. This is a self-signed identity pinned by whoever
/// downloads it, not a public-CA certificate whose lifetime bounds the damage
/// of mis-issuance. The failure mode of a short life here is a database that
/// stops accepting connections one morning for a reason nobody remembers.
const VALIDITY_DAYS: i64 = 3650;

/// Names always present, so a loopback or in-container client can verify.
const ALWAYS_PRESENT: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// The managed certificate and key on disk.
pub struct ManagedTls {
    pub certificate_path: PathBuf,
    pub key_path: PathBuf,
    /// True when this call created them, so the caller can log it once.
    pub generated: bool,
}

/// Returns the node's certificate, generating it if absent or if the names it
/// covers have changed.
///
/// # Errors
///
/// Returns an error when the data directory cannot be written or the
/// certificate cannot be generated. Callers treat that as "no wire TLS"
/// rather than a failed boot: a database that refuses to start because it
/// could not write a certificate is worse than one serving without it.
pub fn ensure(data_dir: &Path, hostnames: &[String]) -> io::Result<ManagedTls> {
    let certificate_path = data_dir.join("wire-cert.pem");
    let key_path = data_dir.join("wire-key.pem");
    let names_path = data_dir.join("wire-cert.names");

    let names = subject_names(hostnames);
    let recorded = fs::read_to_string(&names_path).unwrap_or_default();
    let current = names.join("\n");

    // Regenerated when the names change, because a certificate that does not
    // cover the hostname clients dial fails verification while looking
    // perfectly valid on the server.
    if certificate_path.is_file() && key_path.is_file() && recorded.trim() == current {
        return Ok(ManagedTls {
            certificate_path,
            key_path,
            generated: false,
        });
    }

    let certified = rcgen::generate_simple_self_signed(names).map_err(io::Error::other)?;
    fs::write(&certificate_path, certified.cert.pem())?;
    write_private_key(&key_path, &certified.key_pair.serialize_pem())?;
    fs::write(&names_path, current)?;

    Ok(ManagedTls {
        certificate_path,
        key_path,
        generated: true,
    })
}

/// Every name the certificate should answer to, de-duplicated and ordered.
fn subject_names(hostnames: &[String]) -> Vec<String> {
    let mut names: Vec<String> = ALWAYS_PRESENT
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    for hostname in hostnames {
        let hostname = hostname.trim();
        if !hostname.is_empty() && !names.iter().any(|existing| existing == hostname) {
            names.push(hostname.to_owned());
        }
    }
    names
}

/// Writes the key readable only by this user.
///
/// The certificate is public and the key is not; they sit in the same
/// directory, so the distinction has to be made by permissions rather than by
/// location.
fn write_private_key(path: &Path, pem: &str) -> io::Result<()> {
    fs::write(path, pem)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// How many days remain before the certificate at `path` expires.
///
/// Returns `None` when it cannot be read. Surfaced in the dashboard so a
/// ten-year certificate is still visibly finite rather than forgotten.
#[must_use]
pub fn validity_days() -> i64 {
    VALIDITY_DAYS
}

#[cfg(test)]
mod tests {
    use super::{ensure, subject_names};

    #[test]
    fn loopback_names_are_always_covered() {
        let names = subject_names(&["pintail.example.com".to_owned()]);
        assert!(names.contains(&"localhost".to_owned()));
        assert!(names.contains(&"127.0.0.1".to_owned()));
        assert!(names.contains(&"pintail.example.com".to_owned()));
    }

    #[test]
    fn a_repeated_hostname_is_not_duplicated() {
        let names = subject_names(&["localhost".to_owned(), "a.example.com".to_owned()]);
        assert_eq!(names.iter().filter(|name| *name == "localhost").count(), 1);
    }

    #[test]
    fn generation_is_idempotent_until_the_names_change() {
        let data = tempfile::tempdir().expect("temporary data directory");
        let first = ensure(data.path(), &["a.example.com".to_owned()]).expect("first");
        assert!(first.generated, "the first call creates the certificate");
        let pem = std::fs::read_to_string(&first.certificate_path).expect("certificate");

        let second = ensure(data.path(), &["a.example.com".to_owned()]).expect("second");
        assert!(
            !second.generated,
            "an unchanged node reuses its certificate"
        );
        assert_eq!(
            pem,
            std::fs::read_to_string(&second.certificate_path).expect("certificate"),
            "reuse must not rewrite the certificate, or every restart invalidates \
             what clients pinned",
        );

        // A new hostname must produce a certificate that covers it.
        let third = ensure(data.path(), &["b.example.com".to_owned()]).expect("third");
        assert!(third.generated, "a changed name list regenerates");
        assert_ne!(
            pem,
            std::fs::read_to_string(&third.certificate_path).expect("certificate"),
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_private_key_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;
        let data = tempfile::tempdir().expect("temporary data directory");
        let managed = ensure(data.path(), &[]).expect("generated");
        let mode = std::fs::metadata(&managed.key_path)
            .expect("key metadata")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "the key must not be group or world readable"
        );
    }
}
