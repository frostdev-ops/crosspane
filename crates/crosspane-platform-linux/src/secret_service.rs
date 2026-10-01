//! Secret Service key store, without unlocking or displaying prompts.
//!
//! The `plain` session algorithm is acceptable here because Secret Service messages are
//! unicast between processes of this user on the user's private session bus. It does not
//! protect against a compromised bus or privileged monitoring. Using
//! `dh-ietf1024-sha256-aes128-cbc-pkcs7` is a later hardening step.

use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crosspane_platform::{KeyStore, PlatformError};
use zbus::blocking::{Connection, connection::Builder};
use zbus::message::Message;
use zbus::zvariant::{DynamicType, ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zeroize::Zeroizing;

const SERVICE: &str = "org.freedesktop.secrets";
const SERVICE_PATH: &str = "/org/freedesktop/secrets";
const SERVICE_INTERFACE: &str = "org.freedesktop.Secret.Service";
const COLLECTION_INTERFACE: &str = "org.freedesktop.Secret.Collection";
const ITEM_INTERFACE: &str = "org.freedesktop.Secret.Item";
const TIMEOUT: Duration = Duration::from_secs(2);

// Borrow secret bytes from the reply, so the only application-owned copy is zeroizing.
type Secret<'a> = (ObjectPath<'a>, &'a [u8], &'a [u8], &'a str);

/// Small secrets in Crosspane's namespace in the default Secret Service collection.
///
/// Calls never unlock the collection or invoke a prompt. Each D-Bus reply has a two-second
/// timeout, and the entire public operation (including connection setup) is bounded to two
/// seconds as required by the shared platform contract.
#[derive(Debug)]
pub struct SecretServiceStore {
    connection: Connection,
    session: OwnedObjectPath,
}

impl SecretServiceStore {
    /// Connect to the session bus and `org.freedesktop.secrets`.
    pub fn new() -> Result<SecretServiceStore, PlatformError> {
        bounded(move |deadline| {
            let connection = Builder::session()
                .map_err(dbus_error)?
                .method_timeout(TIMEOUT)
                .build()
                .map_err(dbus_error)?;
            let reply = call(
                &connection,
                deadline,
                SERVICE_PATH,
                SERVICE_INTERFACE,
                "OpenSession",
                &("plain", Value::from("")),
            )?;
            let (output, session): (OwnedValue, OwnedObjectPath) =
                reply.body().deserialize().map_err(dbus_error)?;
            let output =
                String::try_from(output).map_err(|_| backend("invalid plain session output"))?;
            if !output.is_empty() || session.as_str() == "/" {
                return Err(backend("invalid plain session reply"));
            }
            Ok(Self {
                connection,
                session,
            })
        })
    }

    fn run<T, F>(&self, operation: F) -> Result<T, PlatformError>
    where
        T: Send + 'static,
        F: FnOnce(Self, Instant) -> Result<T, PlatformError> + Send + 'static,
    {
        let store = Self {
            connection: self.connection.clone(),
            session: self.session.clone(),
        };
        bounded(move |deadline| operation(store, deadline))
    }

    fn call<B>(
        &self,
        deadline: Instant,
        path: &str,
        interface: &str,
        method: &str,
        body: &B,
    ) -> Result<Message, PlatformError>
    where
        B: serde::Serialize + DynamicType,
    {
        call(&self.connection, deadline, path, interface, method, body)
    }

    fn default_collection(
        &self,
        deadline: Instant,
    ) -> Result<Option<OwnedObjectPath>, PlatformError> {
        let reply = self.call(
            deadline,
            SERVICE_PATH,
            SERVICE_INTERFACE,
            "ReadAlias",
            &("default",),
        )?;
        let collection: OwnedObjectPath = reply.body().deserialize().map_err(dbus_error)?;
        // ReadAlias explicitly confirms absence with '/'. Never create a collection: that
        // can require interaction, which belongs to the caller.
        if collection.as_str() == "/" {
            return Ok(None);
        }
        self.require_unlocked(deadline, collection.as_str(), COLLECTION_INTERFACE)?;
        Ok(Some(collection))
    }

    fn require_unlocked(
        &self,
        deadline: Instant,
        path: &str,
        interface: &str,
    ) -> Result<(), PlatformError> {
        // Query fresh state instead of caching a Locked property that can change at any time.
        let reply = self.call(
            deadline,
            path,
            "org.freedesktop.DBus.Properties",
            "Get",
            &(interface, "Locked"),
        )?;
        let value: OwnedValue = reply.body().deserialize().map_err(dbus_error)?;
        let locked = bool::try_from(value).map_err(|_| backend("invalid lock-state reply"))?;
        if locked {
            Err(PlatformError::InteractionRequired)
        } else {
            Ok(())
        }
    }

    fn search(
        &self,
        deadline: Instant,
        collection: &OwnedObjectPath,
        name: &str,
    ) -> Result<Vec<OwnedObjectPath>, PlatformError> {
        // Collection.SearchItems keeps lookups in the default collection. Unlike
        // Service.SearchItems, it returns a single list, so check each item's Locked state.
        let reply = self.call(
            deadline,
            collection.as_str(),
            COLLECTION_INTERFACE,
            "SearchItems",
            &(attributes(name),),
        )?;
        let items: Vec<OwnedObjectPath> = reply.body().deserialize().map_err(dbus_error)?;
        for item in &items {
            self.require_unlocked(deadline, item.as_str(), ITEM_INTERFACE)?;
        }
        Ok(items)
    }
}

impl KeyStore for SecretServiceStore {
    fn load(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, PlatformError> {
        let name = name.to_owned();
        self.run(move |store, deadline| {
            let Some(collection) = store.default_collection(deadline)? else {
                return Ok(None);
            };
            let items = store.search(deadline, &collection, &name)?;
            let Some(item) = items.first() else {
                return Ok(None);
            };
            let reply = store.call(
                deadline,
                SERVICE_PATH,
                SERVICE_INTERFACE,
                "GetSecrets",
                &(&items[..1], &store.session),
            )?;
            let body = reply.body();
            let secrets: HashMap<ObjectPath<'_>, Secret<'_>> =
                body.deserialize().map_err(dbus_error)?;
            let Some((session, parameters, value, _)) = secrets.get(&item.as_ref()) else {
                // GetSecrets can omit an item locked between the search and retrieval. An
                // omitted entry is never evidence of absence.
                store.require_unlocked(deadline, collection.as_str(), COLLECTION_INTERFACE)?;
                store.require_unlocked(deadline, item.as_str(), ITEM_INTERFACE)?;
                return Err(backend("GetSecrets omitted the requested item"));
            };
            if session.as_str() != store.session.as_str() {
                return Err(backend("secret reply used another session"));
            }
            if !parameters.is_empty() {
                return Err(backend("plain secret reply included encoding parameters"));
            }
            // gnome-keyring's gkd_secret_secret_append reports text/plain even for binary
            // secrets; ignore the returned MIME type and preserve the exact bytes.
            // https://github.com/GNOME/gnome-keyring/blob/master/daemon/dbus/gkd-secret-secret.c
            Ok(Some(Zeroizing::new(value.to_vec())))
        })
    }

    fn store(&self, name: &str, secret: &[u8]) -> Result<(), PlatformError> {
        let name = name.to_owned();
        let secret = Zeroizing::new(secret.to_vec());
        self.run(move |store, deadline| {
            let collection = store
                .default_collection(deadline)?
                .ok_or(PlatformError::InteractionRequired)?;
            // Do not try to replace a locked item, even when its collection is unlocked.
            store.search(deadline, &collection, &name)?;
            let label = label(&name);
            let properties = HashMap::from([
                (
                    "org.freedesktop.Secret.Item.Label",
                    Value::from(label.as_str()),
                ),
                (
                    "org.freedesktop.Secret.Item.Attributes",
                    Value::from(attributes(&name)),
                ),
            ]);
            let secret = (
                &store.session,
                &[] as &[u8],
                secret.as_slice(),
                "application/octet-stream",
            );
            let reply = store.call(
                deadline,
                collection.as_str(),
                COLLECTION_INTERFACE,
                "CreateItem",
                &(properties, secret, true),
            )?;
            let (item, prompt): (OwnedObjectPath, OwnedObjectPath) =
                reply.body().deserialize().map_err(dbus_error)?;
            require_no_prompt(&prompt)?;
            if item.as_str() == "/" {
                return Err(backend("CreateItem returned no item"));
            }
            Ok(())
        })
    }

    fn delete(&self, name: &str) -> Result<(), PlatformError> {
        let name = name.to_owned();
        self.run(move |store, deadline| {
            let Some(collection) = store.default_collection(deadline)? else {
                return Ok(());
            };
            for item in store.search(deadline, &collection, &name)? {
                let reply = store.call(deadline, item.as_str(), ITEM_INTERFACE, "Delete", &())?;
                let prompt: OwnedObjectPath = reply.body().deserialize().map_err(dbus_error)?;
                require_no_prompt(&prompt)?;
            }
            Ok(())
        })
    }
}

fn attributes(name: &str) -> HashMap<&str, &str> {
    HashMap::from([
        ("application", "io.frostdev.crosspane"),
        ("crosspane-name", name),
    ])
}

fn label(name: &str) -> String {
    format!("Crosspane: {name}")
}

fn require_no_prompt(prompt: &OwnedObjectPath) -> Result<(), PlatformError> {
    if prompt.as_str() == "/" {
        Ok(())
    } else {
        // Never call Prompt or Unlock. Returning the prompt path does not display UI.
        Err(PlatformError::InteractionRequired)
    }
}

fn call<B>(
    connection: &Connection,
    deadline: Instant,
    path: &str,
    interface: &str,
    method: &str,
    body: &B,
) -> Result<Message, PlatformError>
where
    B: serde::Serialize + DynamicType,
{
    check_deadline(deadline)?;
    let reply = connection
        .call_method(Some(SERVICE), path, Some(interface), method, body)
        .map_err(dbus_error)?;
    // A timed-out public operation must never proceed to another call or mutation.
    check_deadline(deadline)?;
    Ok(reply)
}

fn check_deadline(deadline: Instant) -> Result<(), PlatformError> {
    if Instant::now() >= deadline {
        Err(PlatformError::Timeout)
    } else {
        Ok(())
    }
}

fn bounded<T, F>(operation: F) -> Result<T, PlatformError>
where
    T: Send + 'static,
    F: FnOnce(Instant) -> Result<T, PlatformError> + Send + 'static,
{
    let deadline = Instant::now() + TIMEOUT;
    let (tx, rx) = mpsc::sync_channel(1);
    // zbus's method timeout covers waiting for a reply, not connection/authentication or
    // sending a message. Bound the caller's whole operation too. An in-flight D-Bus mutation
    // cannot be cancelled and may complete after Timeout; no subsequent calls are issued.
    thread::Builder::new()
        .name("crosspane-secret-service".to_owned())
        .spawn(move || {
            let _ = tx.send(operation(deadline));
        })
        .map_err(|_| backend("could not start Secret Service worker"))?;
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(PlatformError::Timeout),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(backend("Secret Service worker stopped")),
    }
}

fn backend(message: &str) -> PlatformError {
    PlatformError::Backend(format!("Secret Service: {message}"))
}

fn dbus_error(error: zbus::Error) -> PlatformError {
    match error {
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "org.freedesktop.Secret.Error.IsLocked" =>
        {
            PlatformError::InteractionRequired
        }
        zbus::Error::MethodError(name, _, _)
            if matches!(
                name.as_str(),
                "org.freedesktop.DBus.Error.Timeout"
                    | "org.freedesktop.DBus.Error.TimedOut"
                    | "org.freedesktop.DBus.Error.NoReply"
            ) =>
        {
            PlatformError::Timeout
        }
        zbus::Error::FDO(error)
            if matches!(
                *error,
                zbus::fdo::Error::Timeout(_)
                    | zbus::fdo::Error::TimedOut(_)
                    | zbus::fdo::Error::NoReply(_)
            ) =>
        {
            PlatformError::Timeout
        }
        zbus::Error::InputOutput(error) | zbus::Error::Connection(error, _)
            if error.kind() == std::io::ErrorKind::TimedOut =>
        {
            PlatformError::Timeout
        }
        // A remote error description can echo secret bytes. Do not include it (or the
        // message body) in a PlatformError, Debug output or log.
        _ => backend("D-Bus operation failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::{attributes, label};
    use std::collections::HashMap;

    #[test]
    fn attributes_and_label_formatting() {
        for name in ["device-key", "", "spaces / : λ"] {
            assert_eq!(
                attributes(name),
                HashMap::from([
                    ("application", "io.frostdev.crosspane"),
                    ("crosspane-name", name),
                ])
            );
            assert_eq!(label(name), format!("Crosspane: {name}"));
        }
    }
}
