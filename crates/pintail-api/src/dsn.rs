//! Source connection strings as operators actually hold them.

use mysql_async::Opts;

/// Query parameters that belong to a client driver rather than to the
/// connection, and that Pintail therefore ignores.
///
/// These configure how a driver decodes or batches on its own side - node
/// `mysql2` spells them this way - so they say nothing about how Pintail
/// should talk to the source. `mysql_async` does not recognise them and
/// refuses the whole URL, which meant an operator could not paste the
/// connection string their application already uses: Chitti LMS reported
/// building their pools "from parsed connection-string components with the
/// query parameters dropped", which is that refusal seen from outside.
const CLIENT_ONLY_PARAMETERS: &[&str] = &[
    "multiplestatements",
    "datestrings",
    "supportbignumbers",
    "bignumberstrings",
    "decimalnumbers",
    "typecast",
    "rowsasarray",
    "namedplaceholders",
    "nestrables",
    "timezone",
    "charset",
    "debug",
    "trace",
    "insecureauth",
    "connectionlimit",
    "queuelimit",
    "waitforconnections",
    "enablekeepalive",
    "keepaliveinitialdelay",
    "maxpreparedstatements",
    "dateformat",
    "flags",
];

/// Parses a source DSN, ignoring parameters that only configure a client
/// driver.
///
/// Anything else unrecognised still fails. Dropping every unknown parameter
/// would silently swallow a misspelled one that matters - `require_ssl` typed
/// as `requiressl` would connect in plaintext while the operator believed
/// otherwise - so only names known to be client-side are removed.
///
/// # Errors
///
/// Returns the underlying parse failure, with the ignored parameters named so
/// the caller can say what it dropped.
pub(crate) fn source_opts(dsn: &str) -> Result<Opts, String> {
    match Opts::from_url(dsn) {
        Ok(opts) => Ok(opts),
        Err(error) => {
            let (stripped, dropped) = strip_client_parameters(dsn);
            if dropped.is_empty() {
                return Err(error.to_string());
            }
            Opts::from_url(&stripped).map_err(|error| error.to_string())
        }
    }
}

/// Returns the DSN without its client-only parameters, and the names removed.
fn strip_client_parameters(dsn: &str) -> (String, Vec<String>) {
    let Some((base, query)) = dsn.split_once('?') else {
        return (dsn.to_owned(), Vec::new());
    };
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let name = pair.split_once('=').map_or(pair, |(name, _)| name);
        if CLIENT_ONLY_PARAMETERS.contains(&name.to_ascii_lowercase().as_str()) {
            dropped.push(name.to_owned());
        } else {
            kept.push(pair);
        }
    }
    let rebuilt = if kept.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    (rebuilt, dropped)
}

#[cfg(test)]
mod tests {
    use super::{source_opts, strip_client_parameters};

    /// The connection string Chitti LMS holds in their application.
    const REPORTED: &str =
        "mysql://root:root@127.0.0.1:3306/chitti_common?multipleStatements=true&dateStrings=date";

    #[test]
    fn a_connection_string_from_the_application_parses() {
        let opts = source_opts(REPORTED).expect("client-only parameters must not refuse the DSN");
        assert_eq!(opts.db_name(), Some("chitti_common"));
        assert_eq!(opts.ip_or_hostname(), "127.0.0.1");
        assert_eq!(opts.tcp_port(), 3306);
        assert_eq!(opts.user(), Some("root"));
    }

    #[test]
    fn parameters_the_connection_needs_are_kept() {
        let (stripped, dropped) = strip_client_parameters(
            "mysql://h/db?multipleStatements=true&require_ssl=true&dateStrings=date",
        );
        assert_eq!(stripped, "mysql://h/db?require_ssl=true");
        assert_eq!(dropped, vec!["multipleStatements", "dateStrings"]);
    }

    #[test]
    fn an_unrecognised_parameter_still_fails() {
        // A misspelled require_ssl must not be silently dropped: the operator
        // would believe the connection is encrypted when it is not.
        let error = source_opts("mysql://h/db?requiressl=true").expect_err("must reject");
        assert!(error.contains("requiressl"), "unexpected error: {error}");
    }

    #[test]
    fn a_dsn_without_parameters_is_untouched() {
        let (stripped, dropped) = strip_client_parameters("mysql://root@h:3306/db");
        assert_eq!(stripped, "mysql://root@h:3306/db");
        assert!(dropped.is_empty());
    }
}
