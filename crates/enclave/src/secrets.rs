//! The provider keys. In an enclave they arrive sealed — encrypted to a key only attested code
//! can obtain — so they never exist in the clear outside it, not in the host's environment and
//! not on its disk. On a developer's machine they come from the environment.

pub trait SecretSource: Send + Sync {
    fn get(&self, name: &str) -> Option<String>;
}

/// Development: environment variables.
pub struct EnvSecrets;

impl SecretSource for EnvSecrets {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
    }
}

/// Tests: none at all.
pub struct NoSecrets;

impl SecretSource for NoSecrets {
    fn get(&self, _name: &str) -> Option<String> {
        None
    }
}
