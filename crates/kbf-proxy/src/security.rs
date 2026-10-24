//! What the serving code decides a call with: the `buck2_auth` mode, the tokens, the secret
//! the children are told, and the per-address limit on wrong tokens.
//!
//! [`Security::authorize`] reads the `Authorization` header, applies the limit before any
//! hashing, and calls [`decide`].

use std::net::IpAddr;
use std::time::Instant;

use http::HeaderMap;

use crate::auth::{
    Buck2Auth, FailureLimit, Presented, Refusal, Tokens, bearer_of, checks_token, decide,
};
use crate::identity::{Identity, ProxySecret};
use crate::route::AuthClass;

/// The mode, the tokens, the secret the children are told and the limit on wrong tokens.
#[derive(Debug)]
pub(crate) struct Security {
    pub(crate) mode: Buck2Auth,
    pub(crate) tokens: Tokens,
    pub(crate) secret: ProxySecret,
    pub(crate) failures: FailureLimit,
}

impl Security {
    pub(crate) fn new(mode: Buck2Auth, tokens: Tokens, secret: ProxySecret) -> Self {
        Self {
            mode,
            tokens,
            secret,
            failures: FailureLimit::new(),
        }
    }

    /// Decides a call from `ip` that presented `identity` (the verified certificate, if any)
    /// and whatever `headers` carry.
    ///
    /// The limit comes first: an address that has presented too many wrong tokens is refused
    /// without its token being hashed. A wrong token counts against its address; a call with
    /// no token, or for a class that takes none, costs nothing and counts nothing.
    pub(crate) fn authorize(
        &self,
        class: AuthClass,
        headers: &HeaderMap,
        identity: Option<&Identity>,
        ip: IpAddr,
        now: Instant,
    ) -> Result<(), Refusal> {
        let bearer = bearer_of(headers);
        let hashed = bearer.is_some() && checks_token(class, self.mode);
        if hashed && !self.failures.allows(ip, now) {
            return Err(Refusal::RateLimited);
        }
        let verdict = decide(
            class,
            self.mode,
            &self.tokens,
            Presented { identity, bearer },
        );
        if hashed && verdict.is_err() {
            self.failures.record(ip, now);
        }
        verdict
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{FAILURE_BURST, Token};

    fn headers(authorization: &[u8]) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_bytes(authorization).expect("a value"),
        );
        h
    }

    fn ip(n: u32) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::from(0xc000_0200 + n))
    }

    fn security() -> Security {
        let secret = ProxySecret::from_slice(&[1; 32]).expect("32 bytes");
        let tokens = Tokens {
            buck2: Some(Token::new(b"buck2-key")),
            operator: Some(Token::new(b"op-key")),
        };
        Security::new(Buck2Auth::Bearer, tokens, secret)
    }

    #[test]
    fn a_call_with_no_token_costs_the_limit_nothing() {
        let security = security();
        let now = Instant::now();
        for _ in 0..100 {
            let verdict =
                security.authorize(AuthClass::Operator, &HeaderMap::new(), None, ip(1), now);
            assert_eq!(verdict, Err(Refusal::Unauthenticated));
        }
        assert_eq!(security.failures.tracked(), 0);
        // A class that takes no token never looks at one.
        for _ in 0..100 {
            let verdict =
                security.authorize(AuthClass::Worker, &headers(b"Bearer x"), None, ip(1), now);
            assert_eq!(verdict, Err(Refusal::Unauthenticated));
        }
        assert_eq!(security.failures.tracked(), 0);
    }

    #[test]
    fn the_limit_is_asked_before_the_token_is_hashed() {
        let security = security();
        let now = Instant::now();
        let wrong = headers(b"Bearer wrong");
        for _ in 0..FAILURE_BURST {
            let verdict = security.authorize(AuthClass::Reapi, &wrong, None, ip(1), now);
            assert_eq!(verdict, Err(Refusal::Unauthenticated));
        }
        let right = headers(b"Bearer buck2-key");
        let verdict = security.authorize(AuthClass::Reapi, &right, None, ip(1), now);
        assert_eq!(verdict, Err(Refusal::RateLimited));
        let verdict = security.authorize(AuthClass::Reapi, &right, None, ip(2), now);
        assert_eq!(verdict, Ok(()));
    }
}
