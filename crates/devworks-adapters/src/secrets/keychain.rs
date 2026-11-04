//! The macOS Keychain: one generic-password item per handle (service `DevWorks`, account
//! `<namespace>/<handle>`) through the SecItem API. The item's access control list is the
//! default for the creating application, so another application reading it prompts the user.
//! A locked keychain that cannot prompt (`errSecInteractionNotAllowed`), a cancelled unlock
//! prompt and a missing, read-only or unusable keychain are reported as unavailable, a failed
//! authentication as denied ([`super::faults::keychain`]).
use super::{SERVICE, handle_of};
use core_foundation::base::{CFType, CFTypeRef, OSStatus, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::data::{CFData, CFDataRef};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};
use devworks_core::ports::secrets::{Secret, SecretError, SecretKey, SecretStore};
use devworks_core::protocol::Id;
use std::ptr;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecClass: CFStringRef;
    static kSecClassGenericPassword: CFStringRef;
    static kSecAttrService: CFStringRef;
    static kSecAttrAccount: CFStringRef;
    static kSecValueData: CFStringRef;
    static kSecReturnData: CFStringRef;
    static kSecReturnAttributes: CFStringRef;
    static kSecMatchLimit: CFStringRef;
    static kSecMatchLimitOne: CFStringRef;
    static kSecMatchLimitAll: CFStringRef;
    fn SecItemAdd(attributes: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
    fn SecItemCopyMatching(query: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
    fn SecItemUpdate(query: CFDictionaryRef, attributes_to_update: CFDictionaryRef) -> OSStatus;
    fn SecItemDelete(query: CFDictionaryRef) -> OSStatus;
}

const ERR_SEC_SUCCESS: OSStatus = 0;
const ERR_SEC_DUPLICATE_ITEM: OSStatus = -25299;
const ERR_SEC_ITEM_NOT_FOUND: OSStatus = -25300;

pub struct Keychain;

/// A Security framework constant as a CFString (the constants are static, never released).
fn constant(c: CFStringRef) -> CFType {
    // SAFETY: the Security framework exports these as valid, immortal CFStringRefs.
    unsafe { CFString::wrap_under_get_rule(c) }.as_CFType()
}
fn cfstr(s: &str) -> CFType {
    CFString::new(s).as_CFType()
}
fn error(status: OSStatus) -> SecretError {
    super::faults::keychain(status)
}
/// The query naming one item (or, with `account` `None`, every DevWorks item).
fn query(account: Option<&str>, extra: &[(CFType, CFType)]) -> CFDictionary<CFType, CFType> {
    // SAFETY: reading immortal framework constants.
    let mut pairs = unsafe {
        vec![
            (constant(kSecClass), constant(kSecClassGenericPassword)),
            (constant(kSecAttrService), cfstr(SERVICE)),
        ]
    };
    if let Some(a) = account {
        // SAFETY: as above.
        pairs.push((unsafe { constant(kSecAttrAccount) }, cfstr(a)));
    }
    pairs.extend(extra.iter().cloned());
    CFDictionary::from_CFType_pairs(&pairs)
}

impl SecretStore for Keychain {
    fn name(&self) -> &'static str {
        "macOS Keychain"
    }

    fn put(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        let account = key.account();
        let data = CFData::from_buffer(secret.expose().as_bytes()).as_CFType();
        // SAFETY: reading an immortal framework constant.
        let value_key = unsafe { constant(kSecValueData) };
        let add = query(Some(&account), &[(value_key.clone(), data.clone())]);
        // SAFETY: `add` is a valid dictionary for the call's duration; no result is requested.
        let status = unsafe { SecItemAdd(add.as_concrete_TypeRef(), ptr::null_mut()) };
        match status {
            ERR_SEC_SUCCESS => Ok(()),
            ERR_SEC_DUPLICATE_ITEM => {
                let find = query(Some(&account), &[]);
                let update = CFDictionary::from_CFType_pairs(&[(value_key, data)]);
                // SAFETY: both dictionaries are valid for the call's duration.
                let status = unsafe {
                    SecItemUpdate(find.as_concrete_TypeRef(), update.as_concrete_TypeRef())
                };
                if status == ERR_SEC_SUCCESS {
                    Ok(())
                } else {
                    Err(error(status))
                }
            }
            s => Err(error(s)),
        }
    }

    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        // SAFETY: reading immortal framework constants.
        let q = unsafe {
            query(
                Some(&key.account()),
                &[
                    (
                        constant(kSecReturnData),
                        CFBoolean::true_value().as_CFType(),
                    ),
                    (constant(kSecMatchLimit), constant(kSecMatchLimitOne)),
                ],
            )
        };
        let mut result: CFTypeRef = ptr::null();
        // SAFETY: `q` is valid; on success `result` receives a +1 CFData we take ownership of.
        let status = unsafe { SecItemCopyMatching(q.as_concrete_TypeRef(), &mut result) };
        match status {
            ERR_SEC_SUCCESS if !result.is_null() => {
                // SAFETY: kSecReturnData with a single match returns a CFData (create rule).
                let data = unsafe { CFData::wrap_under_create_rule(result as CFDataRef) };
                let text = String::from_utf8(data.bytes().to_vec())
                    .map_err(|_| SecretError::Failed("the item is not UTF-8".into()))?;
                Ok(Some(Secret::new(text)))
            }
            ERR_SEC_SUCCESS | ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            s => Err(error(s)),
        }
    }

    fn delete(&self, key: &SecretKey) -> Result<bool, SecretError> {
        let q = query(Some(&key.account()), &[]);
        // SAFETY: `q` is valid for the call's duration.
        match unsafe { SecItemDelete(q.as_concrete_TypeRef()) } {
            ERR_SEC_SUCCESS => Ok(true),
            ERR_SEC_ITEM_NOT_FOUND => Ok(false),
            s => Err(error(s)),
        }
    }

    fn list(&self, namespace: &str) -> Result<Vec<Id>, SecretError> {
        // SAFETY: reading immortal framework constants.
        let q = unsafe {
            query(
                None,
                &[
                    (
                        constant(kSecReturnAttributes),
                        CFBoolean::true_value().as_CFType(),
                    ),
                    (constant(kSecMatchLimit), constant(kSecMatchLimitAll)),
                ],
            )
        };
        let mut result: CFTypeRef = ptr::null();
        // SAFETY: `q` is valid; on success `result` receives a +1 CFArray of dictionaries.
        let status = unsafe { SecItemCopyMatching(q.as_concrete_TypeRef(), &mut result) };
        match status {
            ERR_SEC_ITEM_NOT_FOUND => return Ok(Vec::new()),
            ERR_SEC_SUCCESS if !result.is_null() => {}
            ERR_SEC_SUCCESS => return Ok(Vec::new()),
            s => return Err(error(s)),
        }
        // SAFETY: kSecReturnAttributes with kSecMatchLimitAll returns a CFArray (create rule).
        let array: core_foundation::array::CFArray<CFType> =
            unsafe { core_foundation::array::CFArray::wrap_under_create_rule(result as _) };
        // SAFETY: reading an immortal framework constant.
        let account_key = unsafe { kSecAttrAccount };
        let mut out = Vec::new();
        for item in array.iter() {
            let Some(dict) = item.downcast::<CFDictionary>() else {
                continue;
            };
            let Some(value) = dict.find(account_key as *const std::ffi::c_void) else {
                continue;
            };
            // SAFETY: kSecAttrAccount values are CFStrings owned by the dictionary.
            let account = unsafe { CFString::wrap_under_get_rule(*value as CFStringRef) };
            if let Some(h) = handle_of(&account.to_string(), namespace) {
                out.push(h);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    /// The real login keychain. Opt-in (`--ignored`); it may prompt on first access.
    #[test]
    #[ignore = "touches the real macOS Keychain; run locally with --ignored"]
    fn keychain_round_trip() {
        super::super::round_trip(&super::Keychain);
    }
}
