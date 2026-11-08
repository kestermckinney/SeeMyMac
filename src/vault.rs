//! Credential vault: passwords live in the system keyring (Secret Service on Linux),
//! never in connections.json. Tests use an in-memory map instead.

const SERVICE: &str = "SeeMyMac";

#[cfg(not(test))]
pub fn get(id: &str) -> Result<Option<String>, String> {
    // The Secret Service transport encryption occasionally fails to decrypt a
    // reply ("Crypto error: Unpad Error"); a fresh attempt succeeds.
    let mut last = String::new();
    for _ in 0..3 {
        let entry = keyring::Entry::new(SERVICE, id).map_err(|e| e.to_string())?;
        match entry.get_password() {
            Ok(p) => return Ok(Some(p)),
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

#[cfg(not(test))]
pub fn set(id: &str, password: &str) -> Result<(), String> {
    keyring::Entry::new(SERVICE, id).and_then(|e| e.set_password(password)).map_err(|e| e.to_string())
}

#[cfg(not(test))]
pub fn delete(id: &str) {
    if let Ok(e) = keyring::Entry::new(SERVICE, id) {
        let _ = e.delete_credential();
    }
}

#[cfg(test)]
static MEM: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub fn get(id: &str) -> Result<Option<String>, String> {
    let _ = SERVICE;
    Ok(MEM.lock().unwrap().iter().find(|(k, _)| k == id).map(|(_, v)| v.clone()))
}

#[cfg(test)]
pub fn set(id: &str, password: &str) -> Result<(), String> {
    delete(id);
    MEM.lock().unwrap().push((id.to_string(), password.to_string()));
    Ok(())
}

#[cfg(test)]
pub fn delete(id: &str) {
    MEM.lock().unwrap().retain(|(k, _)| k != id);
}
