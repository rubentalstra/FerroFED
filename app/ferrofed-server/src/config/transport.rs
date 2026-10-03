// SPDX-FileCopyrightText: Vernum Projecten B.V.
// SPDX-License-Identifier: BUSL-1.1

//! The transport every outbound URL the configuration names must use, by
//! what it carries. Two policies hold, and this module is the only place
//! either is written:
//!
//! - **Protected payload** ([`protected_payload`]): a URL a credential or a
//!   patient identifier is sent to must be `https` outside
//!   `profile = "development"`. Under that profile it may be `http`, and each
//!   such site is reported so the banner, the log and `config check` name it.
//! - **Trust anchor** ([`trust_anchor`]): a URL the gateway verifies its
//!   callers against, a key set or a token introspection endpoint, must be
//!   `https`, or `http` to a loopback host, under every profile, since a
//!   verifier reached in the clear lets a network attacker forge callers.
//!
//! The protected-payload sites are a registry endpoint with a
//! `[credentials."<id>"]` section, the token endpoint of that section's OAuth
//! 2.0 grant, every PIX Manager and every XCPD responding gateway (each is
//! sent the patient identifier, and a credential when one is configured), the
//! care services directory of `[registry.mcsd]` when it has credentials, and
//! `metrics.otlp_endpoint` when it carries a user name or a password. Any
//! other URL may stay `http`. The stored-query PostgreSQL connection string is
//! no site: it is never an `http` URL, and whether libpq encrypts it is its
//! own `sslmode`.
//!
//! The specification assumes transport security and binds it through the
//! security profiles (§2.2, §13, Annex B); no specification governs these
//! policies: our own design.

use ferrofed_identity::dev::Profile;
use ferrofed_registry::snapshot::RegistrySnapshot;
use url::Url;

use crate::config::settings::{Scheme, Settings};

/// Where a protected payload is sent: the key of the URL and what travels to
/// it, by key, never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectedSite {
    /// The key of the URL, such as `pixm.manager[0].url`.
    pub url_key: String,
    /// What travels to the URL, by key, such as
    /// `pixm.manager[0].credentials and patient identifiers`.
    pub payload: String,
}

/// A credential or a patient identifier configured to travel over a URL that
/// is not `https`, outside the development profile.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{} is not an https URL, and {} would travel over it in cleartext: outside profile = \"development\" a credential or a patient identifier is sent only over https",
    site.url_key,
    site.payload
)]
pub struct CleartextError {
    /// The site that would send its payload in cleartext.
    pub site: ProtectedSite,
}

/// A trust anchor configured over plain `http` to a host that is not
/// loopback.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{key} is plain http to a host that is not loopback: the gateway verifies its callers against it, so it is https, or http to a loopback host, under every profile"
)]
pub struct TrustAnchorError {
    /// The key that carries the URL.
    pub key: String,
}

/// Holds one site to the protected-payload policy.
///
/// It returns `Ok(None)` when `url` is `https`, and `Ok(Some(site))` when it
/// is not and `profile` is development, so the caller reports that the
/// payload travels unencrypted.
/// A URL that does not parse cannot be shown to be `https`, so it counts as
/// unprotected. The URL is read for its scheme and never rendered.
///
/// # Errors
///
/// Returns [`CleartextError`] naming `site` when `url` is not `https` and
/// `profile` is not development.
pub fn protected_payload(
    profile: Profile,
    url: &str,
    site: ProtectedSite,
) -> Result<Option<ProtectedSite>, CleartextError> {
    let protected = Url::parse(url).is_ok_and(|parsed| parsed.scheme() == "https");
    if protected {
        return Ok(None);
    }
    match profile {
        Profile::Development => Ok(Some(site)),
        Profile::Production => Err(CleartextError { site }),
    }
}

/// Holds the URL at `key` to the trust-anchor policy: `https`, or `http` to a
/// loopback host, under every profile.
///
/// # Errors
///
/// Returns [`TrustAnchorError`] naming `key` for any other URL.
pub fn trust_anchor(key: &str, url: &Url) -> Result<(), TrustAnchorError> {
    let loopback = match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(name)) => name == "localhost",
        None => false,
    };
    match url.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        _ => Err(TrustAnchorError {
            key: key.to_owned(),
        }),
    }
}

/// Holds every protected payload `settings` send to [`protected_payload`].
///
/// The endpoint URLs are those `registry` declares. The sites that travel in
/// cleartext under the development profile are returned in key order.
/// Without `registry`, the endpoint URLs are not known and only the other
/// sites are held. An endpoint `[credentials]` names that `registry` does not
/// declare has no URL to hold; building the node clients refuses it.
///
/// # Errors
///
/// Returns the [`CleartextError`] of the first site, in key order, that
/// [`protected_payload`] refuses.
pub fn check(
    settings: &Settings,
    registry: Option<&RegistrySnapshot>,
) -> Result<Vec<ProtectedSite>, CleartextError> {
    let profile = settings.profile;
    let mut cleartext = Vec::new();
    let mut hold = |url: &str, site: ProtectedSite| {
        protected_payload(profile, url, site).map(|exposed| cleartext.extend(exposed))
    };
    let site = |url_key: String, payload: String| ProtectedSite { url_key, payload };
    for (endpoint, scheme) in &settings.credentials {
        let section = format!("credentials.{endpoint}");
        // NOTE: no specification governs this: our own design; an endpoint the
        // registry lacks is legitimately absent here, and the client build refuses it.
        if let Some(declared) = registry.and_then(|registry| registry.endpoint(endpoint)) {
            hold(
                declared.url().as_str(),
                site(
                    format!("the url of endpoint {endpoint} in registry.document"),
                    section.clone(),
                ),
            )?;
        }
        if let Scheme::OAuth2(grant) = scheme {
            hold(
                grant.token_endpoint().as_str(),
                site(
                    format!("{section}.oauth2.token_endpoint"),
                    format!("{section}.oauth2"),
                ),
            )?;
        }
    }
    for (index, manager) in settings
        .pixm
        .iter()
        .flat_map(|pixm| pixm.managers.iter().enumerate())
    {
        let key = format!("pixm.manager[{index}]");
        let credentials = manager
            .credentials
            .is_some()
            .then(|| format!("{key}.credentials"));
        hold(
            manager.url.expose(),
            identity_site(&key, credentials.as_deref()),
        )?;
    }
    if let Some(xcpd) = &settings.xcpd {
        let assertion = xcpd.assertion.as_ref().map(|_| xcpd.assertion_key);
        for (index, gateway) in xcpd.gateways.iter().enumerate() {
            let key = format!("xcpd.gateway[{index}]");
            hold(gateway.url.expose(), identity_site(&key, assertion))?;
        }
    }
    if let Some(directory) = settings
        .registry_directory
        .as_ref()
        .filter(|directory| directory.credentials.is_some())
    {
        hold(directory.url.expose(), directory_site())?;
    }
    if let Some(endpoint) = &settings.metrics.otlp_endpoint {
        let carries = Url::parse(endpoint.expose())
            .is_ok_and(|parsed| !parsed.username().is_empty() || parsed.password().is_some());
        if carries {
            hold(
                endpoint.expose(),
                site(
                    String::from("metrics.otlp_endpoint"),
                    String::from("the userinfo of metrics.otlp_endpoint"),
                ),
            )?;
        }
    }
    Ok(cleartext)
}

/// The site of the care services directory of `[registry.mcsd]`: its `url`,
/// sent the credentials of its own section.
#[must_use]
pub fn directory_site() -> ProtectedSite {
    ProtectedSite {
        url_key: String::from("registry.mcsd.url"),
        payload: String::from("registry.mcsd.credentials"),
    }
}

/// The site of the identity service configured at `key`, such as
/// `pixm.manager[0]`: its `url`, sent the patient identifiers it is asked
/// for, with the credential `credential` names when one is configured.
pub(crate) fn identity_site(key: &str, credential: Option<&str>) -> ProtectedSite {
    let payload = match credential {
        Some(credential) => format!("{credential} and patient identifiers"),
        None => format!("the patient identifiers asked of {key}"),
    };
    ProtectedSite {
        url_key: format!("{key}.url"),
        payload,
    }
}

/// Logs one warning per site in `cleartext`, by key, never a value.
pub fn warn(cleartext: &[ProtectedSite]) {
    for site in cleartext {
        tracing::warn!(
            url = site.url_key,
            payload = site.payload,
            "a credential or a patient identifier travels unencrypted over plain http, which only the development profile allows"
        );
    }
}

/// Writes one warning per site in `cleartext` to stderr, by key, never a
/// value, for a command that runs with no log subscriber.
#[expect(
    clippy::print_stderr,
    reason = "`config check` and `admission check` warn the operator who ran them"
)]
pub fn print_warnings(cleartext: &[ProtectedSite]) {
    for site in cleartext {
        eprintln!(
            "ferrofed: warning: {} travels unencrypted to {}, which is not https; only profile = \"development\" allows that",
            site.payload, site.url_key
        );
    }
}

/// Holds every protected payload to the policy and warns on stderr.
///
/// It runs [`check`], then [`print_warnings`] for each site that travels in
/// cleartext, for a command that runs with no log subscriber.
///
/// # Errors
///
/// Returns the [`CleartextError`] [`check`] returns.
pub fn check_and_print(
    settings: &Settings,
    registry: Option<&RegistrySnapshot>,
) -> Result<(), CleartextError> {
    print_warnings(&check(settings, registry)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CleartextError, ProtectedSite, TrustAnchorError, protected_payload, trust_anchor};
    use ferrofed_identity::dev::Profile;
    use url::Url;

    fn site() -> ProtectedSite {
        ProtectedSite {
            url_key: String::from("xcpd.gateway[0].url"),
            payload: String::from("xcpd.assertion and patient identifiers"),
        }
    }

    #[test]
    fn https_passes_under_every_profile() {
        for profile in [Profile::Production, Profile::Development] {
            assert_eq!(
                Ok(None),
                protected_payload(profile, "https://pix.example.org/fhir", site())
            );
        }
    }

    #[test]
    fn http_is_refused_outside_development_and_reported_under_it() {
        assert_eq!(
            Err(CleartextError { site: site() }),
            protected_payload(Profile::Production, "http://pix.example.org/fhir", site())
        );
        assert_eq!(
            Ok(Some(site())),
            protected_payload(Profile::Development, "http://pix.example.org/fhir", site())
        );
    }

    #[test]
    fn a_url_that_does_not_parse_counts_as_unprotected() {
        assert_eq!(
            Err(CleartextError { site: site() }),
            protected_payload(Profile::Production, "not a url", site())
        );
    }

    #[test]
    fn the_refusal_names_both_keys_and_never_the_url() {
        let refused = protected_payload(
            Profile::Production,
            "http://user:synthetic-secret@pix.example.org/fhir",
            site(),
        )
        .expect_err("plain http is refused");
        let text = refused.to_string();
        assert!(text.contains("xcpd.gateway[0].url"), "{text}");
        assert!(text.contains("xcpd.assertion"), "{text}");
        assert!(!text.contains("synthetic-secret"), "{text}");
        assert!(!text.contains("pix.example.org"), "{text}");
    }

    #[test]
    fn a_trust_anchor_is_https_or_loopback_http() {
        for good in [
            "https://issuer.example.test/jwks",
            "http://127.0.0.1:8443/jwks",
            "http://[::1]:8443/jwks",
            "http://localhost/jwks",
        ] {
            let url = Url::parse(good).expect("a test URL parses");
            assert_eq!(
                Ok(()),
                trust_anchor("auth.issuer[0].jwks_uri", &url),
                "{good}"
            );
        }
        let url = Url::parse("http://issuer.example.test/jwks").expect("a test URL parses");
        assert_eq!(
            Err(TrustAnchorError {
                key: String::from("auth.issuer[0].jwks_uri")
            }),
            trust_anchor("auth.issuer[0].jwks_uri", &url)
        );
    }
}
