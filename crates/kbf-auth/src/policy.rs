//! A whole REAPI policy, and the JSON file it is read from (`docs/reapi-auth.md`).

use std::fmt;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::authenticate::{
    AllAuthenticator, AllowAuthenticator, AnyAuthenticator, Authenticator, DenyAuthenticator,
};
use crate::authorize::{
    AllowAuthorizer, Authorizer, Authorizers, DenyAuthorizer, InstanceNamePrefixAuthorizer,
};
use crate::metadata::AuthenticationMetadata;

/// How a REAPI listener authenticates its calls and authorizes each kind of call.
pub struct Policy {
    /// Run once per call, before routing ([`crate::AuthenticateLayer`]).
    pub authenticator: Arc<dyn Authenticator>,
    /// Asked by the services, per call.
    pub authorizers: Authorizers,
}

impl fmt::Debug for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Policy").finish_non_exhaustive()
    }
}

/// Why a policy file is refused.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// The text is not JSON.
    #[error("not JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The JSON is not a policy: where, as a path from the root `$`, and why.
    #[error("{path}: {why}")]
    Invalid {
        /// Where, such as `$.authenticationPolicy.any.policies[1]`.
        path: String,
        /// What is wrong there.
        why: String,
    },
}

/// The authentication policies Buildbarn has that kbf does not build yet.
const LATER_AUTHENTICATION: &[&str] = &[
    "tlsClientCertificate",
    "jwt",
    "peerCredentialsJmespathExpression",
    "remote",
];

/// The authorizers Buildbarn has that kbf does not build yet.
const LATER_AUTHORIZERS: &[&str] = &["jmespathExpression", "remote"];

impl Policy {
    /// Every call accepted with empty metadata, and every authorizer allows: what a
    /// server runs with no policy file.
    #[must_use]
    pub fn allow_all() -> Self {
        Self {
            authenticator: Arc::new(AllowAuthenticator::default()),
            authorizers: Authorizers::allow_all(),
        }
    }

    /// The policy a JSON policy file describes (`docs/reapi-auth.md`).
    ///
    /// # Errors
    /// The text is not JSON, or not a policy: an unknown key, an object that names no
    /// variant or more than one, a variant not built yet, a value of the wrong type,
    /// a bad instance-name prefix, `actionCache.putAuthorizer` (clients never write
    /// the action cache), or no `authenticationPolicy`.
    pub fn from_json(text: &str) -> Result<Self, PolicyError> {
        let root: Value = serde_json::from_str(text)?;
        let path = Path::root();
        let top = object(&root, &path)?;
        known(
            top,
            &path,
            &[
                "authenticationPolicy",
                "capabilitiesAuthorizer",
                "contentAddressableStorage",
                "actionCache",
                "executeAuthorizer",
            ],
        )?;
        let authenticator = match top.get("authenticationPolicy") {
            Some(v) => authentication(v, &path.key("authenticationPolicy"))?,
            None => return Err(path.invalid("`authenticationPolicy` is required")),
        };
        let mut authorizers = Authorizers::allow_all();
        if let Some(v) = top.get("capabilitiesAuthorizer") {
            authorizers.capabilities = authorizer(v, &path.key("capabilitiesAuthorizer"))?;
        }
        if let Some(v) = top.get("executeAuthorizer") {
            authorizers.execute = authorizer(v, &path.key("executeAuthorizer"))?;
        }
        if let Some(v) = top.get("contentAddressableStorage") {
            let path = path.key("contentAddressableStorage");
            let cas = object(v, &path)?;
            known(
                cas,
                &path,
                &["getAuthorizer", "putAuthorizer", "findMissingAuthorizer"],
            )?;
            for (key, slot) in [
                ("getAuthorizer", &mut authorizers.cas_get),
                ("putAuthorizer", &mut authorizers.cas_put),
                ("findMissingAuthorizer", &mut authorizers.cas_find_missing),
            ] {
                if let Some(v) = cas.get(key) {
                    *slot = authorizer(v, &path.key(key))?;
                }
            }
        }
        if let Some(v) = top.get("actionCache") {
            let path = path.key("actionCache");
            let ac = object(v, &path)?;
            if ac.contains_key("putAuthorizer") {
                return Err(path.key("putAuthorizer").invalid(
                    "clients never write the action cache (UpdateActionResult is always \
                     PERMISSION_DENIED), so it has no authorizer",
                ));
            }
            known(ac, &path, &["getAuthorizer"])?;
            if let Some(v) = ac.get("getAuthorizer") {
                authorizers.ac_get = authorizer(v, &path.key("getAuthorizer"))?;
            }
        }
        Ok(Self {
            authenticator,
            authorizers,
        })
    }
}

/// A place in the JSON, for errors.
struct Path(String);

impl Path {
    fn root() -> Self {
        Self("$".to_owned())
    }

    fn key(&self, key: &str) -> Self {
        Self(format!("{}.{key}", self.0))
    }

    fn index(&self, i: usize) -> Self {
        Self(format!("{}[{i}]", self.0))
    }

    fn invalid(&self, why: impl Into<String>) -> PolicyError {
        PolicyError::Invalid {
            path: self.0.clone(),
            why: why.into(),
        }
    }
}

fn object<'a>(v: &'a Value, path: &Path) -> Result<&'a Map<String, Value>, PolicyError> {
    v.as_object()
        .ok_or_else(|| path.invalid(format!("expected an object, found {}", kind(v))))
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Refuses a key not in `keys`.
fn known(obj: &Map<String, Value>, path: &Path, keys: &[&str]) -> Result<(), PolicyError> {
    let Some(k) = obj.keys().find(|k| !keys.contains(&k.as_str())) else {
        return Ok(());
    };
    let why = if keys.is_empty() {
        "unknown key; this object takes none".to_owned()
    } else {
        format!("unknown key; the keys here are {}", keys.join(", "))
    };
    Err(path.key(k).invalid(why))
}

/// The one variant a oneof object names, refusing none, several, one not built yet
/// (`later`), and one that does not exist.
fn variant<'a>(
    v: &'a Value,
    path: &Path,
    built: &[&str],
    later: &[&str],
) -> Result<(&'a str, &'a Value), PolicyError> {
    let obj = object(v, path)?;
    let mut entries = obj.iter();
    let (Some((name, value)), None) = (entries.next(), entries.next()) else {
        return Err(path.invalid(format!(
            "names {} variants; exactly one of {} is needed",
            obj.len(),
            built.join(", ")
        )));
    };
    if later.contains(&name.as_str()) {
        return Err(path.key(name).invalid(format!(
            "`{name}` is not supported yet; the variants built are {}",
            built.join(", ")
        )));
    }
    if !built.contains(&name.as_str()) {
        return Err(path.key(name).invalid(format!(
            "unknown variant; the variants are {}",
            built.join(", ")
        )));
    }
    Ok((name, value))
}

fn authentication(v: &Value, path: &Path) -> Result<Arc<dyn Authenticator>, PolicyError> {
    let (name, value) = variant(
        v,
        path,
        &["allow", "deny", "any", "all"],
        LATER_AUTHENTICATION,
    )?;
    let path = path.key(name);
    match name {
        "allow" => {
            let md = object(value, &path)?;
            if md.contains_key("tracingAttributes") {
                return Err(path
                    .key("tracingAttributes")
                    .invalid("`tracingAttributes` is not supported yet"));
            }
            known(md, &path, &["public", "private"])?;
            Ok(Arc::new(AllowAuthenticator::new(
                AuthenticationMetadata::new(md.get("public").cloned(), md.get("private").cloned()),
            )))
        }
        "deny" => match value {
            Value::String(message) => Ok(Arc::new(DenyAuthenticator::new(message.clone()))),
            other => Err(path.invalid(format!(
                "expected the refusal's message, a string; found {}",
                kind(other)
            ))),
        },
        "any" => {
            let mut children = children(value, &path)?;
            if children.len() == 1 {
                return Ok(children.remove(0));
            }
            Ok(Arc::new(AnyAuthenticator::new(children)))
        }
        _ => {
            let mut children = children(value, &path)?;
            match children.len() {
                0 => Err(path
                    .key("policies")
                    .invalid("`all` needs at least one policy")),
                1 => Ok(children.remove(0)),
                _ => Ok(Arc::new(AllAuthenticator::new(children))),
            }
        }
    }
}

/// The `policies` of an `any` or `all`.
fn children(v: &Value, path: &Path) -> Result<Vec<Arc<dyn Authenticator>>, PolicyError> {
    let obj = object(v, path)?;
    known(obj, path, &["policies"])?;
    let path = path.key("policies");
    let Some(list) = obj.get("policies") else {
        return Err(path.invalid("`policies` is required"));
    };
    let Value::Array(list) = list else {
        return Err(path.invalid(format!("expected an array, found {}", kind(list))));
    };
    list.iter()
        .enumerate()
        .map(|(i, child)| authentication(child, &path.index(i)))
        .collect()
}

fn authorizer(v: &Value, path: &Path) -> Result<Arc<dyn Authorizer>, PolicyError> {
    let (name, value) = variant(
        v,
        path,
        &["allow", "deny", "instanceNamePrefix"],
        LATER_AUTHORIZERS,
    )?;
    let path = path.key(name);
    let obj = object(value, &path)?;
    match name {
        "allow" | "deny" => {
            known(obj, &path, &[])?;
            Ok(if name == "allow" {
                Arc::new(AllowAuthorizer)
            } else {
                Arc::new(DenyAuthorizer)
            })
        }
        _ => {
            known(obj, &path, &["allowedInstanceNamePrefixes"])?;
            let path = path.key("allowedInstanceNamePrefixes");
            let prefixes = match obj.get("allowedInstanceNamePrefixes") {
                Some(Value::Array(list)) => list
                    .iter()
                    .enumerate()
                    .map(|(i, p)| match p {
                        Value::String(p) => Ok(p.clone()),
                        other => Err(path
                            .index(i)
                            .invalid(format!("expected a string, found {}", kind(other)))),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                Some(other) => {
                    return Err(path.invalid(format!("expected an array, found {}", kind(other))));
                }
                None => return Err(path.invalid("`allowedInstanceNamePrefixes` is required")),
            };
            InstanceNamePrefixAuthorizer::new(prefixes)
                .map(|a| Arc::new(a) as Arc<dyn Authorizer>)
                .map_err(|e| path.invalid(e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests;
