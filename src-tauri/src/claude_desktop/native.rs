//! macOS APIs used by Electron's safeStorage format. Secrets never enter command arguments.
use core_foundation::{
    base::{CFType, CFTypeRef, TCFType}, boolean::CFBoolean, data::CFData,
    dictionary::{CFDictionary, CFDictionaryRef}, string::{CFString, CFStringRef},
};
use std::ffi::{c_char, c_void};
use super::DesktopError;

#[link(name = "Security", kind = "framework")]
extern "C" {
    static kSecClass: CFStringRef;
    static kSecClassGenericPassword: CFStringRef;
    static kSecAttrService: CFStringRef;
    static kSecAttrAccount: CFStringRef;
    static kSecMatchLimit: CFStringRef;
    static kSecMatchLimitOne: CFStringRef;
    static kSecReturnData: CFStringRef;
    static kSecUseAuthenticationUI: CFStringRef;
    static kSecUseAuthenticationUIFail: CFStringRef;
    fn SecItemCopyMatching(query: CFDictionaryRef, result: *mut CFTypeRef) -> i32;
}

// CommonCrypto ships in libSystem; these signatures match the macOS SDK headers.
extern "C" {
    fn CCKeyDerivationPBKDF(algorithm: u32, password: *const c_char, password_len: usize,
        salt: *const u8, salt_len: usize, prf: u32, rounds: u32, key: *mut u8, key_len: usize) -> i32;
    fn CCCrypt(operation: u32, algorithm: u32, options: u32, key: *const c_void, key_len: usize,
        iv: *const c_void, input: *const c_void, input_len: usize, output: *mut c_void,
        output_len: usize, written: *mut usize) -> i32;
}

pub(super) fn storage_key(allow_interaction: bool) -> Result<[u8; 16], DesktopError> {
    // SAFETY: framework constants are valid CF strings. The dictionary retains all values;
    // the result is owned under Core Foundation's Create rule and type checked before use.
    let password = unsafe {
        let cf = |s| CFString::wrap_under_get_rule(s).as_CFType();
        let mut pairs = vec![
            (cf(kSecClass), cf(kSecClassGenericPassword)),
            (cf(kSecAttrService), CFString::new("Claude Safe Storage").as_CFType()),
            (cf(kSecAttrAccount), CFString::new("Claude Key").as_CFType()),
            (cf(kSecMatchLimit), cf(kSecMatchLimitOne)),
            (cf(kSecReturnData), CFBoolean::true_value().as_CFType()),
        ];
        if !allow_interaction {
            pairs.push((cf(kSecUseAuthenticationUI), cf(kSecUseAuthenticationUIFail)));
        }
        let query = CFDictionary::from_CFType_pairs(&pairs);
        let mut result = std::ptr::null();
        match SecItemCopyMatching(query.as_concrete_TypeRef(), &mut result) {
            0 if !result.is_null() => {
                let value = CFType::wrap_under_create_rule(result);
                value.downcast::<CFData>().ok_or(DesktopError::Invalid)?.bytes().to_vec()
            }
            -25308 | -25293 | -128 => return Err(DesktopError::PermissionRequired),
            -25300 => return Err(DesktopError::NotFound),
            _ => return Err(DesktopError::Keychain),
        }
    };
    derive_key(&password)
}

pub(super) fn derive_key(password: &[u8]) -> Result<[u8; 16], DesktopError> {
    if password.is_empty() { return Err(DesktopError::Invalid); }
    let mut key = [0; 16];
    // PBKDF2 / HMAC-SHA1, as used by Electron on macOS.
    let status = unsafe { CCKeyDerivationPBKDF(2, password.as_ptr().cast(), password.len(),
        b"saltysalt".as_ptr(), 9, 1, 1003, key.as_mut_ptr(), key.len()) };
    if status != 0 { return Err(DesktopError::Invalid); }
    Ok(key)
}

pub(super) fn decrypt(encrypted: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, DesktopError> {
    let payload = encrypted.strip_prefix(b"v10").filter(|p| !p.is_empty() && p.len() % 16 == 0)
        .ok_or(DesktopError::Invalid)?;
    let mut output = vec![0; payload.len() + 16];
    let mut written = 0;
    // SAFETY: all buffers remain live and sizes match their allocations. AES-128-CBC with
    // Electron's fixed IV. Validate PKCS#7 padding explicitly; CommonCrypto can accept
    // malformed padding on some macOS versions. Authentication/identity is checked separately.
    let status = unsafe { CCCrypt(1, 0, 0, key.as_ptr().cast(), key.len(), [0x20u8; 16].as_ptr().cast(),
        payload.as_ptr().cast(), payload.len(), output.as_mut_ptr().cast(), output.len(), &mut written) };
    if status != 0 { return Err(DesktopError::Invalid); }
    output.truncate(written);
    let padding = output.last().copied().ok_or(DesktopError::Invalid)? as usize;
    if !(1..=16).contains(&padding) || padding > output.len()
        || !output[output.len() - padding..].iter().all(|b| *b as usize == padding) {
        return Err(DesktopError::Invalid);
    }
    output.truncate(output.len() - padding);
    Ok(output)
}
