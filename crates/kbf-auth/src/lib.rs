//! REAPI authentication and authorization, in Buildbarn's two layers.
//!
//! - **Authentication** runs once per call, before the call reaches a service: an
//!   [`Authenticator`] reads the call's headers and extensions and answers who is
//!   calling, as [`AuthenticationMetadata`], or refuses the call (UNAUTHENTICATED for
//!   "no"). [`AuthenticateLayer`] runs it on a tonic server and puts the metadata in
//!   the request's extensions, where [`metadata`] reads it.
//! - **Authorization** runs in the services, once per call, after the instance name
//!   is decoded and before any store work: an [`Authorizer`] answers whether this
//!   metadata may do this to this instance name. A refusal is PERMISSION_DENIED.
//!   [`Authorizers`] holds one per kind of call, as Buildbarn's per-store
//!   `getAuthorizer`, `putAuthorizer` and `findMissingAuthorizer` and its
//!   `executeAuthorizer` do.
//!
//! A [`Policy`] is both: an authenticator and the authorizers. [`Policy::from_json`]
//! reads one from the JSON file `docs/reapi-auth.md` describes, whose names follow
//! Buildbarn's configuration (`authenticationPolicy`, `allow`, `any`, `all`, `deny`,
//! `instanceNamePrefix`). [`Policy::allow_all`] is what a server runs with no policy
//! file: every call is accepted with empty metadata, and every authorizer allows.
//!
//! The policies built so far: authenticators `allow` (fixed metadata), `deny`, `any`
//! and `all`; authorizers `allow`, `deny` and `instanceNamePrefix`. A policy file that
//! names one of Buildbarn's other variants (`tlsClientCertificate`, `jwt`, `remote`,
//! `jmespathExpression`, ...) is refused as not supported yet, so a file written for a
//! later server is never half-read.

mod authenticate;
mod authorize;
mod layer;
mod metadata;
mod policy;

pub use crate::authenticate::{
    AllAuthenticator, AllowAuthenticator, AnyAuthenticator, Authenticator, DenyAuthenticator,
};
pub use crate::authorize::{
    AllowAuthorizer, Authorizer, Authorizers, DenyAuthorizer, InstanceNamePrefixAuthorizer,
    PrefixError, authorize,
};
pub use crate::layer::{Authenticate, AuthenticateLayer};
pub use crate::metadata::{AuthenticationMetadata, metadata};
pub use crate::policy::{Policy, PolicyError};

/// A boxed future, as the traits return them.
pub type BoxFuture<'a, T> = futures::future::BoxFuture<'a, T>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a public type without a `Debug` that names it, and a `Debug` of an
    /// error that loses what was wrong.
    #[test]
    fn every_public_type_has_a_debug() {
        let prefix =
            InstanceNamePrefixAuthorizer::new(vec!["ci".to_owned()]).expect("a valid prefix");
        let shown = format!(
            "{:?} {:?} {AllowAuthorizer:?} {DenyAuthorizer:?} {prefix:?}",
            AllowAuthenticator::default(),
            DenyAuthenticator::new("closed"),
        );
        for part in [
            "AllowAuthenticator",
            "DenyAuthenticator(\"closed\")",
            "AllowAuthorizer",
            "DenyAuthorizer",
            "InstanceNamePrefixAuthorizer { prefixes: [\"ci\"] }",
        ] {
            assert!(shown.contains(part), "{part} not in {shown}");
        }
        let bad = InstanceNamePrefixAuthorizer::new(vec!["/".to_owned()]).expect_err("bad");
        assert!(format!("{bad:?}").contains("PrefixError"), "{bad:?}");
        let e = Policy::from_json("[").expect_err("not JSON");
        assert!(format!("{e:?}").starts_with("Json("), "{e:?}");
    }
}
