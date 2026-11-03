//! A language for programs when nobody gave one. macOS gives apps started
//! from Finder no LANG, and a shell in the "C" locale mangles anything past
//! ASCII (zsh splits 你 into loose bytes). Like Ghostty: the system's
//! language and region in UTF-8 if this machine has that locale, else
//! en_US.UTF-8. A locale already set is never touched.

use std::collections::BTreeMap;

/// The `LANG` to add when `env` sets no locale. `system`: the system's
/// `<language>_<region>`; `exists`: whether this machine has a locale.
pub fn missing_lang(
    env: &BTreeMap<String, String>,
    system: Option<String>,
    exists: impl Fn(&str) -> bool,
) -> Option<String> {
    let set = |k: &str| env.get(k).is_some_and(|v| !v.is_empty());
    if ["LANG", "LC_ALL", "LC_CTYPE"].iter().any(|k| set(k)) {
        return None;
    }
    let wanted = system.map(|s| format!("{s}.UTF-8")).filter(|l| exists(l));
    Some(wanted.unwrap_or_else(|| "en_US.UTF-8".into()))
}

/// The `LANG` this core's programs get when `env` has none: macOS only
/// (Linux logins set one; nothing to guess there).
pub fn for_programs(env: &BTreeMap<String, String>) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        static SYSTEM: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        missing_lang(env, SYSTEM.get_or_init(mac::system).clone(), mac::exists)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = env;
        None
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{CStr, CString, c_char, c_void};

    type CFTypeRef = *const c_void;
    const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFLocaleLanguageCode: CFTypeRef;
        static kCFLocaleCountryCode: CFTypeRef;
        fn CFLocaleCopyCurrent() -> CFTypeRef;
        fn CFLocaleGetValue(locale: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
        fn CFStringGetCString(s: CFTypeRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
        fn CFRelease(cf: CFTypeRef);
    }

    /// `<language>_<region>` from the system settings (what Ghostty asks Cocoa).
    pub fn system() -> Option<String> {
        unsafe {
            let locale = CFLocaleCopyCurrent();
            if locale.is_null() {
                return None;
            }
            let get = |key: CFTypeRef| {
                let s = CFLocaleGetValue(locale, key);
                let mut buf = [0 as c_char; 64];
                (!s.is_null() && CFStringGetCString(s, buf.as_mut_ptr(), buf.len() as isize, UTF8) != 0)
                    .then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
            };
            let found = match (get(kCFLocaleLanguageCode), get(kCFLocaleCountryCode)) {
                (Some(language), Some(region)) => Some(format!("{language}_{region}")),
                _ => None,
            };
            CFRelease(locale);
            found
        }
    }

    /// Whether this machine has `name` (newlocale: safe beside other threads).
    pub fn exists(name: &str) -> bool {
        let Ok(name) = CString::new(name) else { return false };
        let loc = unsafe { libc::newlocale(libc::LC_CTYPE_MASK, name.as_ptr(), std::ptr::null_mut()) };
        if !loc.is_null() {
            unsafe { libc::freelocale(loc) };
        }
        !loc.is_null()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> BTreeMap<String, String> {
        vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_missing_locale_comes_from_the_system_else_english() {
        let all = |_: &str| true;
        let none = |_: &str| false;
        assert_eq!(missing_lang(&env(&[]), Some("tr_TR".into()), all).as_deref(), Some("tr_TR.UTF-8"));
        // English with a Turkish region: no such locale on macOS.
        assert_eq!(missing_lang(&env(&[]), Some("en_TR".into()), none).as_deref(), Some("en_US.UTF-8"));
        assert_eq!(missing_lang(&env(&[]), None, all).as_deref(), Some("en_US.UTF-8"));
        assert_eq!(
            missing_lang(&env(&[("LANG", "")]), Some("de_DE".into()), all).as_deref(),
            Some("de_DE.UTF-8"),
            "empty is missing"
        );
        for set in ["LANG", "LC_ALL", "LC_CTYPE"] {
            assert_eq!(missing_lang(&env(&[(set, "C")]), Some("tr_TR".into()), all), None, "{set} chosen by the user");
        }
    }
}
