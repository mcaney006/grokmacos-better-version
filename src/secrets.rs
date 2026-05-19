//! OS-native secret storage (Keychain / Credential Manager / Secret Service).

use crate::error::SecretError;
use zeroize::Zeroizing;

const SERVICE: &str = "com.grokinsane.grok-insane";

/// Store an API key in the OS keyring. Overwrites any existing entry.
pub fn set_api_key(provider: &str, key: &str) -> Result<(), SecretError> {
    let entry = keyring::Entry::new(SERVICE, &key_name(provider))?;
    entry.set_password(key)?;
    Ok(())
}

/// Load an API key from the OS keyring, returning `Ok(None)` if absent.
pub fn get_api_key(provider: &str) -> Result<Option<Zeroizing<String>>, SecretError> {
    let entry = keyring::Entry::new(SERVICE, &key_name(provider))?;
    match entry.get_password() {
        Ok(s) => Ok(Some(Zeroizing::new(s))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn delete_api_key(provider: &str) -> Result<(), SecretError> {
    let entry = keyring::Entry::new(SERVICE, &key_name(provider))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn key_name(provider: &str) -> String {
    format!("api-key:{provider}")
}
