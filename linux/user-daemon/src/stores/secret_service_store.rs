use std::collections::HashMap;
use std::io;
use std::io::ErrorKind;
use std::time::{SystemTime, UNIX_EPOCH};

use failure::Error;
use secret_service::{Collection, EncryptionType, Item, SecretService, SsError};
use serde_json;
use u2f_core::{try_reverse_app_id, AppId, ApplicationKey, Counter, KeyHandle, SecretStore};
use u2f_core::PrivateKey;
use stores::{Secret, UserSecretStore};
use std::convert::TryInto;

#[derive(Debug, Fail)]
pub enum SecretServiceError {
    #[fail(display = "crypto error {}", _0)]
    Crypto(String),
    #[fail(display = "D-Bus error {} {}", _0, _1)]
    DBus(String, String),
    #[fail(display = "object locked")]
    Locked,
    #[fail(display = "no result found")]
    NoResult,
    #[fail(display = "failed to parse D-Bus output")]
    Parse,
    #[fail(display = "prompt dismissed")]
    Prompt,
}

impl From<secret_service::SsError> for SecretServiceError {
    fn from(err: SsError) -> Self {
        match err {
            SsError::Crypto(err) => SecretServiceError::Crypto(err),
            SsError::Dbus(err) => SecretServiceError::DBus(
                err.name().unwrap_or("").into(),
                err.message().unwrap_or("").into(),
            ),
            SsError::Locked => SecretServiceError::Locked,
            SsError::NoResult => SecretServiceError::NoResult,
            SsError::Parse => SecretServiceError::Parse,
            SsError::Prompt => SecretServiceError::Prompt,
        }
    }
}

pub struct SecretServiceStore {
    service: SecretService,
}

impl SecretServiceStore {
    pub fn new() -> Result<SecretServiceStore, Error> {
        let service =
            SecretService::new(EncryptionType::Dh).map_err(|err| SecretServiceError::from(err))?;
        Ok(SecretServiceStore { service })
    }

    pub fn is_supported() -> bool {
        SecretServiceStore::new().is_ok()
    }
}

impl UserSecretStore for SecretServiceStore {
    fn add_secret(&self, secret: Secret) -> io::Result<()> {
        let collection = self
            .service
            .get_default_collection()
            .map_err(|_error| io::Error::new(ErrorKind::Other, "get_default_collection"))?;
        unlock_if_locked(&collection)?;
        let attributes = registration_attributes(
            &secret.application_key.application,
            &secret.application_key.handle,
        );
        let attributes = attributes.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let label = match try_reverse_app_id(&secret.application_key.application) {
            Some(app_id) => format!("Universal 2nd Factor token for {}", app_id),
            None => format!(
                "Universal 2nd Factor token for {}",
                secret.application_key.application.to_base64()
            ),
        };
        let secret = serde_json::to_string(&Secret {
            application_key: secret.application_key.clone(),
            counter: secret.counter,
        })
        .map_err(|error| io::Error::new(ErrorKind::Other, error))?;
        let content_type = "application/json";
        let _item = collection
            .create_item(&label, attributes, secret.as_bytes(), false, content_type)
            .map_err(|_error| io::Error::new(ErrorKind::Other, "create_item"))?;
        Ok(())
    }

    fn into_u2f_store(self: Box<Self>) -> Box<dyn SecretStore> {
        self
    }
}

impl SecretStore for SecretServiceStore {
    fn add_application_key(&self, key: &ApplicationKey) -> io::Result<()> {
        self.add_secret(Secret {
            application_key: key.clone(),
            counter: 0,
        })
    }

    fn get_and_increment_counter(
        &self,
        application: &AppId,
        handle: &KeyHandle,
    ) -> io::Result<Counter> {
        Ok(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs().try_into().unwrap())
    }

    fn retrieve_application_key(
        &self,
        application: &AppId,
        handle: &KeyHandle,
    ) -> io::Result<Option<ApplicationKey>> {
        dbg!("return defulat key");
        let defkey = ApplicationKey::new(*application, handle.clone(), PrivateKey::from_pem(
"-----BEGIN EC PRIVATE KEY-----
MHcCAQEEILoFuwW6BboFugW3BbkFuQW5BbkFuQW5BbkFuQW5BboFoAoGCCqGSM49
AwEHoUQDQgAEj31WNnTfgCzWc5HK86YBgkgwmV+zQdWIlWMdAdiCJBafa4niVwKE
cglOAKlIDU4uVrBxVgzgcE67wpSPVZzjVg==
-----END EC PRIVATE KEY-----"));
        return Ok(Some(defkey.clone()));
        let collection = self
            .service
            .get_default_collection()
            .map_err(|error| io::Error::new(ErrorKind::Other, error.to_string()))?;
        let option = find_item(&collection, application, handle)
            .map_err(|error| io::Error::new(ErrorKind::Other, error.to_string()))?;
        if option.is_none() {
            return Ok(None);
        }
        let item = option.unwrap();
        let secret_bytes = item
            .get_secret()
            .map_err(|error| io::Error::new(ErrorKind::Other, error.to_string()))?;
        let secret: Secret = serde_json::from_slice(&secret_bytes)
            .map_err(|error| io::Error::new(ErrorKind::Other, error))?;
        Ok(Some(secret.application_key))
    }
}

fn search_attributes(app_id: &AppId, handle: &KeyHandle) -> Vec<(&'static str, String)> {
    vec![
        ("application", "com.github.danstiner.rust-u2f".to_string()),
        ("u2f_app_id_hash", app_id.to_base64()),
        ("u2f_key_handle", handle.to_base64()),
        ("xdg:schema", "com.github.danstiner.rust-u2f".to_string()),
    ]
}

fn registration_attributes(app_id: &AppId, handle: &KeyHandle) -> Vec<(&'static str, String)> {
    let mut attributes = search_attributes(app_id, handle);
    attributes.push(("times_used", 0.to_string()));

    let start = SystemTime::now();
    let since_the_epoch = start
        .duration_since(UNIX_EPOCH)
        .expect("time moved backwards");
    attributes.push(("date_registered", since_the_epoch.as_secs().to_string()));

    match try_reverse_app_id(app_id) {
        Some(id) => attributes.push(("u2f_app_id", id)),
        None => {}
    };

    attributes
}

fn find_item<'a>(
    collection: &'a Collection<'a>,
    app_id: &AppId,
    handle: &KeyHandle,
) -> io::Result<Option<Item<'a>>> {
    unlock_if_locked(collection)?;
    let attributes = search_attributes(app_id, handle);
    let attributes = attributes.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut result = collection
        .search_items(attributes)
        .map_err(|_error| io::Error::new(ErrorKind::Other, "search_items"))?;
    Ok(result.pop())
}

fn unlock_if_locked(collection: &Collection) -> io::Result<()> {
    if collection
        .is_locked()
        .map_err(|_error| io::Error::new(ErrorKind::Other, "is_locked"))?
    {
        collection
            .unlock()
            .map_err(|_error| io::Error::new(ErrorKind::Other, "unlock"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn todo() {}
}
