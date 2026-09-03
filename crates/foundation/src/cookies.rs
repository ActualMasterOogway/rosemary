//! `NSHTTPCookie` and `NSHTTPCookieStorage`, persisted across launches.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Mutex;

use corefoundation::base::CFTypeRef;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

/// One stored cookie.
#[derive(Clone, Debug, Default, PartialEq)]
struct Cookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    secure: bool,
    /// Seconds since the epoch, or `None` for a cookie that dies with the
    /// session. A session cookie is not written to the jar.
    expires: Option<i64>,
}

impl Cookie {
    /// RFC 6265's rule, which is what a caller depends on to find
    /// `.ROBLOSECURITY` for `www.roblox.com` when it was set for `.roblox.com`.
    fn matches(&self, host: &str, path: &str) -> bool {
        let domain = self.domain.trim_start_matches('.');
        if domain.is_empty() {
            return false;
        }
        let host_matches = host == domain
            || (host.len() > domain.len()
                && host.ends_with(domain)
                && host.as_bytes()[host.len() - domain.len() - 1] == b'.');
        let against = match self.path.is_empty() {
            true => "/",
            false => self.path.as_str(),
        };
        host_matches && path.starts_with(against)
    }

    /// Whether this and `other` name the same cookie, which is what makes a
    /// `-setCookie:` a replacement rather than a second entry.
    fn same_as(&self, other: &Cookie) -> bool {
        self.name == other.name && self.domain == other.domain && self.path == other.path
    }
}

static JAR: Mutex<Vec<Cookie>> = Mutex::new(Vec::new());
static LOADED: std::sync::Once = std::sync::Once::new();

fn jar() -> std::sync::MutexGuard<'static, Vec<Cookie>> {
    LOADED.call_once(|| {
        *JAR.lock().unwrap_or_else(|e| e.into_inner()) = load();
    });
    JAR.lock().unwrap_or_else(|e| e.into_inner())
}

/// Where the jar lives, under the directory macOS keeps cookies in.
fn store_path() -> PathBuf {
    PathBuf::from(crate::home_directory()).join("Library/Cookies/rosetta3-cookies.tsv")
}

/// Tab-separated, one cookie per line.
///
/// No escaping, and none is needed: a tab or a newline is not legal in any of
/// the six fields, so a line that contains one is a line this runtime did not
/// write and is skipped rather than guessed at.
fn load() -> Vec<Cookie> {
    let Ok(text) = std::fs::read_to_string(store_path()) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let mut f = line.split('\t');
            let cookie = Cookie {
                name: f.next()?.to_string(),
                value: f.next()?.to_string(),
                domain: f.next()?.to_string(),
                path: f.next()?.to_string(),
                secure: f.next()? == "1",
                expires: f.next()?.parse().ok(),
            };
            (!cookie.name.is_empty()).then_some(cookie)
        })
        .collect()
}

/// Write the jar, dropping what a real browser would drop.
///
/// Session cookies are left out on purpose - a cookie with no expiry is one the
/// server asked to last only as long as the process - and so is anything
/// already expired, because writing it back would mean reloading it next launch
/// and expiring it again forever.
fn save(cookies: &[Cookie]) {
    let now = now();
    let mut text = String::new();
    for cookie in cookies {
        let Some(expires) = cookie.expires.filter(|e| *e > now) else {
            continue;
        };
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{expires}\n",
            cookie.name,
            cookie.value,
            cookie.domain,
            cookie.path,
            u8::from(cookie.secure),
        ));
    }
    let path = store_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, text);
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// An `NSHTTPCookie`. The payload is a leaked `Box` rather than an inline
/// `Cookie`, because the object is created by the runtime's allocator and its
/// size has to be a constant.
#[repr(C)]
struct CookieObject {
    isa: *const c_void,
    cookie: *mut Cookie,
}

fn cookie_class() -> &'static objc::class::Class {
    corefoundation::base::register(
        c"NSHTTPCookie",
        None,
        size_of::<CookieObject>(),
        false,
    )
}

/// # Safety
///
/// `object` must be null or an `NSHTTPCookie` this module made.
unsafe fn cookie_of<'a>(object: Id) -> Option<&'a Cookie> {
    if object.is_null() {
        return None;
    }
    // SAFETY: the caller's obligation.
    let payload = unsafe { (*(object as *const CookieObject)).cookie };
    (!payload.is_null()).then(|| unsafe { &*payload })
}

fn make_cookie(cookie: Cookie) -> Id {
    let object = objc::object::create_instance(cookie_class(), 0);
    // SAFETY: a fresh instance of this module's own class.
    unsafe {
        (*(object as *mut CookieObject)).cookie = Box::into_raw(Box::new(cookie));
    }
    // SAFETY: a fresh object the caller does not own.
    unsafe { corefoundation::api::CFAutorelease(object as CFTypeRef) as Id }
}

/// The text of an `NSString`, or the empty string for nil.
///
/// # Safety
///
/// `object` must be null or an `NSString`.
unsafe fn text(object: Id) -> String {
    // SAFETY: the caller's obligation.
    String::from_utf8_lossy(&unsafe { crate::string::bytes(object) }).into_owned()
}

/// The host and path of an `NSURL`, from its `-absoluteString`.
///
/// Asked by message rather than read out of the object, because `NSURL` is
/// another module's class and the parse it wants - host and path, no scheme, no
/// query - is this one's business.
///
/// # Safety
///
/// `url` must be null or an object that answers `-absoluteString`.
unsafe fn host_and_path(url: Id) -> (String, String) {
    if url.is_null() {
        return (String::new(), "/".to_string());
    }
    // SAFETY: `-absoluteString` is `id (id, SEL)`.
    let string = unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc::msg::entry());
        f(url, objc::sel::get("absoluteString"))
    };
    // SAFETY: an `NSString`, or nil.
    split_url(&unsafe { text(string) })
}

fn split_url(absolute: &str) -> (String, String) {
    let rest = absolute
        .split_once("://")
        .map_or(absolute, |(_, rest)| rest)
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    // The userinfo and the port are not part of the match.
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .split(':')
        .next()
        .unwrap_or_default();
    (host.to_ascii_lowercase(), path)
}

/// `+[NSHTTPCookie cookieWithProperties:]`.
///
/// Both spellings of every key: Apple's `NSHTTPCookieName` constants and the
/// bare `Name`/`name` a caller building the dictionary by hand uses. A
/// dictionary with no name or no value is not a cookie and answers nil, which
/// is what Apple's does.
unsafe extern "C" fn cookie_with_properties(_class: Id, _sel: Sel, properties: Id) -> Id {
    // SAFETY: null or an `NSDictionary`.
    let get = |keys: &[&str]| -> String {
        for key in keys {
            let name = crate::string::from_str(key);
            // SAFETY: `properties` is null or a dictionary, `name` a live string.
            let value = unsafe {
                corefoundation::api::CFDictionaryGetValue(properties as CFTypeRef, name as CFTypeRef)
            };
            // SAFETY: a fresh string this call owns.
            unsafe { corefoundation::api::CFRelease(name as CFTypeRef) };
            if !value.is_null() {
                // SAFETY: whatever the caller put there; read as a string.
                return unsafe { text(value as Id) };
            }
        }
        String::new()
    };
    let name = get(&["NSHTTPCookieName", "Name", "name"]);
    let value = get(&["NSHTTPCookieValue", "Value", "value"]);
    if name.is_empty() {
        return std::ptr::null_mut();
    }
    let domain = get(&["NSHTTPCookieDomain", "Domain", "domain"]);
    let path = get(&["NSHTTPCookiePath", "Path", "path"]);
    let secure = !get(&["NSHTTPCookieSecure", "Secure", "secure"]).is_empty();
    make_cookie(Cookie {
        name,
        value,
        domain,
        path: match path.is_empty() {
            true => "/".to_string(),
            false => path,
        },
        secure,
        expires: None,
    })
}

/// `+[NSHTTPCookie cookiesWithResponseHeaderFields:forURL:]`.
///
/// Only `Set-Cookie`, and only the attributes that decide where a cookie goes
/// back to: `Domain`, `Path`, `Secure` and `Max-Age`. `Expires` is a date in
/// one of three formats and `Max-Age` is a number that means the same thing, so
/// the number is the one parsed and an `Expires`-only cookie is kept for the
/// session rather than given an invented lifetime.
unsafe extern "C" fn cookies_with_response_header_fields(
    _class: Id,
    _sel: Sel,
    fields: Id,
    url: Id,
) -> Id {
    // SAFETY: null or an `NSURL`.
    let (host, _) = unsafe { host_and_path(url) };
    let mut made: Vec<CFTypeRef> = Vec::new();
    for key in ["Set-Cookie", "set-cookie"] {
        let name = crate::string::from_str(key);
        // SAFETY: `fields` is null or a dictionary.
        let value = unsafe {
            corefoundation::api::CFDictionaryGetValue(fields as CFTypeRef, name as CFTypeRef)
        };
        // SAFETY: a fresh string this call owns.
        unsafe { corefoundation::api::CFRelease(name as CFTypeRef) };
        if value.is_null() {
            continue;
        }
        // SAFETY: an `NSString`.
        if let Some(cookie) = parse_set_cookie(&unsafe { text(value as Id) }, &host) {
            made.push(make_cookie(cookie) as CFTypeRef);
        }
    }
    let array = corefoundation::collections::new_array(
        &made,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: a fresh array this call owns.
    unsafe { corefoundation::api::CFAutorelease(array) as Id }
}

fn parse_set_cookie(header: &str, host: &str) -> Option<Cookie> {
    let mut parts = header.split(';');
    let (name, value) = parts.next()?.trim().split_once('=')?;
    let mut cookie = Cookie {
        name: name.trim().to_string(),
        value: value.trim().to_string(),
        domain: host.to_string(),
        path: "/".to_string(),
        secure: false,
        expires: None,
    };
    if cookie.name.is_empty() {
        return None;
    }
    for attribute in parts {
        let attribute = attribute.trim();
        let (key, value) = attribute.split_once('=').unwrap_or((attribute, ""));
        match key.to_ascii_lowercase().as_str() {
            "domain" => cookie.domain = value.trim().trim_start_matches('.').to_ascii_lowercase(),
            "path" if !value.is_empty() => cookie.path = value.trim().to_string(),
            "secure" => cookie.secure = true,
            // `Max-Age` wins over `Expires` where both are present, which is
            // RFC 6265's rule.
            "max-age" => cookie.expires = value.trim().parse::<i64>().ok().map(|s| now() + s),
            "expires" if cookie.expires.is_none() => cookie.expires = parse_expires(value.trim()),
            _ => {}
        }
    }
    Some(cookie)
}

/// `Expires=Wed, 21 Oct 2015 07:28:00 GMT` as seconds since the epoch.
///
/// This is the attribute the session cookie actually carries - `.ROBLOSECURITY`
/// arrives with an `Expires` and no `Max-Age` - so parsing only `Max-Age` meant
/// treating it as a session cookie and dropping it on exit, which is the exact
/// bug this module exists to fix.
///
/// One format, the RFC 1123 one every server sends. The other two RFC 6265
/// names are a two-digit year and an `asctime` shape, and a cookie in either is
/// kept for the session rather than given a date guessed from an ambiguous
/// year - `None` costs a re-login and a wrong century costs a cookie that never
/// expires or one that never works.
fn parse_expires(value: &str) -> Option<i64> {
    // "Wed, 21 Oct 2015 07:28:00 GMT"
    let rest = value.split_once(", ").map_or(value, |(_, rest)| rest);
    let mut parts = rest.split_whitespace();
    let day: i64 = parts.next()?.parse().ok()?;
    let month = 1 + [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ]
    .iter()
    .position(|m| parts.clone().next().is_some_and(|p| p.to_ascii_lowercase().starts_with(m)))?
        as i64;
    parts.next()?;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Howard Hinnant's `days_from_civil`: days since 1970-01-01 for a proleptic
/// Gregorian date.
///
/// Written out rather than pulled in, because the whole of what this module
/// needs from a date library is this one function, and it is exact for every
/// year - no leap-year table to get wrong at a century boundary.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

unsafe extern "C" fn cookie_name(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an `NSHTTPCookie` this module made.
    autoreleased(unsafe { cookie_of(receiver) }.map_or("", |c| c.name.as_str()))
}

unsafe extern "C" fn cookie_value(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: as above.
    autoreleased(unsafe { cookie_of(receiver) }.map_or("", |c| c.value.as_str()))
}

unsafe extern "C" fn cookie_domain(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: as above.
    autoreleased(unsafe { cookie_of(receiver) }.map_or("", |c| c.domain.as_str()))
}

unsafe extern "C" fn cookie_path(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: as above.
    autoreleased(unsafe { cookie_of(receiver) }.map_or("/", |c| c.path.as_str()))
}

unsafe extern "C" fn cookie_is_secure(receiver: Id, _sel: Sel) -> u8 {
    // SAFETY: as above.
    u8::from(unsafe { cookie_of(receiver) }.is_some_and(|c| c.secure))
}

/// `-properties`, as the dictionary `+cookieWithProperties:` would take back.
unsafe extern "C" fn cookie_properties(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an `NSHTTPCookie` this module made.
    let Some(cookie) = (unsafe { cookie_of(receiver) }) else {
        return std::ptr::null_mut();
    };
    let pairs: Vec<(CFTypeRef, CFTypeRef)> = [
        ("NSHTTPCookieName", cookie.name.as_str()),
        ("NSHTTPCookieValue", cookie.value.as_str()),
        ("NSHTTPCookieDomain", cookie.domain.as_str()),
        ("NSHTTPCookiePath", cookie.path.as_str()),
    ]
    .into_iter()
    .map(|(key, value)| {
        (
            crate::string::from_str(key) as CFTypeRef,
            crate::string::from_str(value) as CFTypeRef,
        )
    })
    .collect();
    let dictionary = corefoundation::collections::new_dictionary(
        &pairs,
        corefoundation::callbacks::Policy::from_key_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    for (key, value) in pairs {
        // SAFETY: the dictionary retained both; these are this call's own
        // references.
        unsafe {
            corefoundation::api::CFRelease(key);
            corefoundation::api::CFRelease(value);
        }
    }
    // SAFETY: a fresh dictionary this call owns.
    unsafe { corefoundation::api::CFAutorelease(dictionary) as Id }
}

/// `ROSETTA3_TRACE_COOKIES=1`: report each call and what it answered.
///
/// A cookie jar is invisible from both ends: a client that asks for a cookie it
/// has never been given and one that is never asked look the same from outside,
/// and the session state that depends on the answer is three HTTP round trips
/// away. This is what tells them apart.
fn trace(what: std::fmt::Arguments) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

    if *ON.get_or_init(|| std::env::var_os("ROSETTA3_TRACE_COOKIES").is_some()) {
        eprintln!("rosetta3: cookies: {what}");
    }
}

fn autoreleased(text: &str) -> Id {
    let string = crate::string::from_str(text);
    // SAFETY: a fresh object.
    unsafe { corefoundation::api::CFAutorelease(string as CFTypeRef) as Id }
}

/// The one storage. Retained forever, for the reason `NSLocale`'s singleton is:
/// a shared object a guest can release is one it can make dangle.
static SHARED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

unsafe extern "C" fn shared_storage(class: Id, _sel: Sel) -> Id {
    *SHARED.get_or_init(|| {
        // SAFETY: `class` is `NSHTTPCookieStorage`, which this module registered.
        let storage = unsafe {
            objc::object::create_instance(
                objc::class::from_ptr(class as *const objc::class::Class).expect("a class"),
                0,
            )
        };
        objc::arc::retain(storage);
        storage as usize
    }) as Id
}

fn cookie_array(cookies: Vec<Cookie>) -> Id {
    let objects: Vec<CFTypeRef> = cookies
        .into_iter()
        .map(|c| make_cookie(c) as CFTypeRef)
        .collect();
    let array = corefoundation::collections::new_array(
        &objects,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: a fresh array this call owns.
    unsafe { corefoundation::api::CFAutorelease(array) as Id }
}

unsafe extern "C" fn storage_cookies(_receiver: Id, _sel: Sel) -> Id {
    let all = jar().clone();
    trace(format_args!(
        "-cookies -> {} ({})",
        all.len(),
        names(&all)
    ));
    cookie_array(all)
}

/// The names in a cookie list, for [`trace`].
fn names(cookies: &[Cookie]) -> String {
    cookies
        .iter()
        .map(|c| format!("{}@{}", c.name, c.domain))
        .collect::<Vec<_>>()
        .join(", ")
}

unsafe extern "C" fn cookies_for_url(_receiver: Id, _sel: Sel, url: Id) -> Id {
    // SAFETY: null or an `NSURL`.
    let (host, path) = unsafe { host_and_path(url) };
    let now = now();
    let matching: Vec<Cookie> = jar()
        .iter()
        .filter(|c| c.matches(&host, &path) && c.expires.is_none_or(|e| e > now))
        .cloned()
        .collect();
    trace(format_args!(
        "-cookiesForURL: {host}{path} -> {} ({})",
        matching.len(),
        names(&matching)
    ));
    cookie_array(matching)
}

unsafe extern "C" fn set_cookie(_receiver: Id, _sel: Sel, cookie: Id) {
    // SAFETY: null or an `NSHTTPCookie` this module made.
    let Some(cookie) = (unsafe { cookie_of(cookie) }) else {
        return;
    };
    trace(format_args!(
        "-setCookie: {}@{}{} = {} bytes",
        cookie.name,
        cookie.domain,
        cookie.path,
        cookie.value.len()
    ));
    let mut jar = jar();
    jar.retain(|existing| !existing.same_as(cookie));
    jar.push(cookie.clone());
    save(&jar);
}

unsafe extern "C" fn delete_cookie(_receiver: Id, _sel: Sel, cookie: Id) {
    // SAFETY: as above.
    let Some(cookie) = (unsafe { cookie_of(cookie) }) else {
        return;
    };
    let mut jar = jar();
    jar.retain(|existing| !existing.same_as(cookie));
    save(&jar);
}

unsafe extern "C" fn set_cookies_for_url(
    receiver: Id,
    sel: Sel,
    cookies: Id,
    _url: Id,
    _main_document: Id,
) {
    // SAFETY: null or an `NSArray`.
    let count = unsafe { corefoundation::api::CFArrayGetCount(cookies as CFTypeRef) };
    for i in 0..count {
        // SAFETY: an index the count admits.
        let cookie = unsafe {
            corefoundation::api::CFArrayGetValueAtIndex(cookies as CFTypeRef, i) as Id
        };
        // SAFETY: `-setCookie:` takes exactly this.
        unsafe { set_cookie(receiver, sel, cookie) };
    }
}

/// `+[NSHTTPCookie requestHeaderFieldsWithCookies:]`, the inverse of
/// `+cookiesWithResponseHeaderFields:forURL:`.
unsafe extern "C" fn request_header_fields(_class: Id, _sel: Sel, cookies: Id) -> Id {
    // SAFETY: null or an `NSArray`.
    let count = unsafe { corefoundation::api::CFArrayGetCount(cookies as CFTypeRef) };
    let mut header = String::new();
    for i in 0..count {
        // SAFETY: an index the count admits.
        let object = unsafe {
            corefoundation::api::CFArrayGetValueAtIndex(cookies as CFTypeRef, i) as Id
        };
        // SAFETY: an `NSHTTPCookie` this module made.
        let Some(cookie) = (unsafe { cookie_of(object) }) else {
            continue;
        };
        if !header.is_empty() {
            header.push_str("; ");
        }
        header.push_str(&format!("{}={}", cookie.name, cookie.value));
    }
    let key = crate::string::from_str("Cookie");
    let value = crate::string::from_str(&header);
    let dictionary = corefoundation::collections::new_dictionary(
        &[(key as CFTypeRef, value as CFTypeRef)],
        corefoundation::callbacks::Policy::from_key_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: the dictionary retained both; these are this call's references.
    unsafe {
        corefoundation::api::CFRelease(key as CFTypeRef);
        corefoundation::api::CFRelease(value as CFTypeRef);
        corefoundation::api::CFAutorelease(dictionary) as Id
    }
}

/// `-setCookieAcceptPolicy:` and its getter.
///
/// Recorded and not honoured: this storage accepts every cookie it is given.
/// The policy that would change that - "only from the main document domain" -
/// needs the main document URL, which `-setCookies:forURL:mainDocumentURL:`
/// carries and nothing else does, and a program that sets the policy and then
/// reads it back is entitled to see what it set.
static ACCEPT_POLICY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

unsafe extern "C" fn set_accept_policy(_receiver: Id, _sel: Sel, policy: usize) {
    ACCEPT_POLICY.store(policy, std::sync::atomic::Ordering::Relaxed);
}

unsafe extern "C" fn accept_policy(_receiver: Id, _sel: Sel) -> usize {
    ACCEPT_POLICY.load(std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn install() {
    let cookie = cookie_class();
    class_methods!(cookie, {
        "cookieWithProperties:" => cookie_with_properties, c"@24@0:8@16";
        "cookiesWithResponseHeaderFields:forURL:" => cookies_with_response_header_fields,
            c"@32@0:8@16@24";
        "requestHeaderFieldsWithCookies:" => request_header_fields, c"@24@0:8@16";
    });
    methods!(cookie, {
        "name" => cookie_name, c"@16@0:8";
        "value" => cookie_value, c"@16@0:8";
        "domain" => cookie_domain, c"@16@0:8";
        "path" => cookie_path, c"@16@0:8";
        "isSecure" => cookie_is_secure, c"c16@0:8";
        "properties" => cookie_properties, c"@16@0:8";
    });

    let storage = corefoundation::base::register(
        c"NSHTTPCookieStorage",
        None,
        size_of::<usize>(),
        false,
    );
    class_methods!(storage, {
        "sharedHTTPCookieStorage" => shared_storage, c"@16@0:8";
    });
    methods!(storage, {
        "cookies" => storage_cookies, c"@16@0:8";
        "cookiesForURL:" => cookies_for_url, c"@24@0:8@16";
        "setCookie:" => set_cookie, c"v24@0:8@16";
        "deleteCookie:" => delete_cookie, c"v24@0:8@16";
        "setCookies:forURL:mainDocumentURL:" => set_cookies_for_url, c"v40@0:8@16@24@32";
        "setCookieAcceptPolicy:" => set_accept_policy, c"v24@0:8Q16";
        "cookieAcceptPolicy" => accept_policy, c"Q16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_splits_into_a_host_and_a_path() {
        assert_eq!(
            split_url("https://www.roblox.com/home?a=1"),
            ("www.roblox.com".to_string(), "/home".to_string())
        );
        assert_eq!(
            split_url("https://roblox.com"),
            ("roblox.com".to_string(), "/".to_string())
        );
        // The port and the userinfo are not part of the match.
        assert_eq!(
            split_url("http://user@Example.COM:8080/a/b"),
            ("example.com".to_string(), "/a/b".to_string())
        );
    }

    /// The rule that decides whether the session cookie goes back out, and the
    /// one worth a test: a cookie set for `.roblox.com` must reach
    /// `www.roblox.com` and must not reach `notroblox.com`.
    #[test]
    fn a_domain_cookie_matches_subdomains_and_nothing_else() {
        let cookie = Cookie {
            name: ".ROBLOSECURITY".into(),
            domain: ".roblox.com".into(),
            path: "/".into(),
            ..Cookie::default()
        };
        assert!(cookie.matches("roblox.com", "/"));
        assert!(cookie.matches("www.roblox.com", "/home"));
        assert!(!cookie.matches("notroblox.com", "/"));
        assert!(!cookie.matches("roblox.com.evil.test", "/"));
    }

    #[test]
    fn a_set_cookie_header_carries_its_attributes() {
        let cookie = parse_set_cookie(
            ".ROBLOSECURITY=TOKEN; Domain=.roblox.com; Path=/; Secure; Max-Age=3600",
            "www.roblox.com",
        )
        .expect("a cookie");
        assert_eq!(cookie.name, ".ROBLOSECURITY");
        assert_eq!(cookie.value, "TOKEN");
        assert_eq!(cookie.domain, "roblox.com");
        assert!(cookie.secure);
        assert!(cookie.expires.is_some());
    }

    /// The date arithmetic, against three dates whose epoch seconds are known.
    #[test]
    fn a_civil_date_converts_to_the_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(
            parse_expires("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480)
        );
    }

    /// The format `.ROBLOSECURITY` actually arrives in. Getting this wrong is
    /// the difference between staying signed in and not.
    #[test]
    fn an_expires_attribute_is_what_persists_the_session_cookie() {
        let cookie = parse_set_cookie(
            ".ROBLOSECURITY=TOKEN; Domain=.roblox.com; Path=/; Expires=Fri, 01 Jan 2100 00:00:00 GMT; Secure",
            "www.roblox.com",
        )
        .expect("a cookie");
        assert_eq!(cookie.expires, Some(4_102_444_800));
    }

    /// A format this parser does not read is a *session* cookie, not a cookie
    /// with an invented date.
    #[test]
    fn an_unreadable_date_is_refused_rather_than_guessed_at() {
        assert_eq!(parse_expires("Thu Jan  1 00:00:00 1970"), None);
        assert_eq!(parse_expires("nonsense"), None);
    }

    /// A header with no `Domain` belongs to the host that sent it.
    #[test]
    fn a_set_cookie_without_a_domain_takes_the_hosts() {
        let cookie = parse_set_cookie("a=b", "www.roblox.com").expect("a cookie");
        assert_eq!(cookie.domain, "www.roblox.com");
        assert_eq!(cookie.path, "/");
        assert_eq!(cookie.expires, None);
    }
}
