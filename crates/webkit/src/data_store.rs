use std::ffi::c_int;

use corefoundation::base::CFTypeRef;
use objc::object::Id;
use objc::sel::Sel;

use crate::engine::{self, Block, Engine};
use crate::gtk::{self, GList, Ptr};
use crate::send;

/// `+defaultDataStore`: one per process, and never freed.
pub fn default_store() -> Id {
    static DEFAULT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *DEFAULT.get_or_init(|| {
        let class =
            objc::class::lookup(c"WKWebsiteDataStore").expect("registered during bootstrap");
        objc::object::create_instance(class, 0) as usize
    }) as Id
}

unsafe extern "C" fn default_data_store(_class: Id, _sel: Sel) -> Id {
    default_store()
}

/// There is one network session, so a non-persistent store is the default one.
/// Said once, because a guest that asked for one expects nothing to be kept.
unsafe extern "C" fn non_persistent_data_store(_class: Id, _sel: Sel) -> Id {
    engine::warn_once("a non-persistent data store shares the default store's session");
    default_store()
}

fn cookie_store() -> Id {
    static STORE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *STORE.get_or_init(|| {
        let class = objc::class::lookup(c"WKHTTPCookieStore").expect("registered during bootstrap");
        objc::object::create_instance(class, 0) as usize
    }) as Id
}

unsafe extern "C" fn http_cookie_store(_receiver: Id, _sel: Sel) -> Id {
    cookie_store()
}

fn empty_array() -> Id {
    let array = corefoundation::collections::new_array(
        &[],
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    objc::arc::autorelease(array as Id)
}

/// `-fetchDataRecordsOfTypes:completionHandler:`.
///
/// Answers no records. WebKitGTK groups its data by origin under types that do
/// not map one-to-one onto WebKit's, and the caller this exists for waits for
/// the handler without reading what it is given.
unsafe extern "C" fn fetch_data_records(_receiver: Id, _sel: Sel, _types: Id, completion: Id) {
    let Some(completion) = Block::copy(completion) else {
        return;
    };
    engine::defer(move || completion.call1(empty_array()));
}

unsafe extern "C" fn remove_data_for_records(
    _receiver: Id,
    _sel: Sel,
    _types: Id,
    _records: Id,
    completion: Id,
) {
    engine::warn_once("website data removal is not implemented; nothing was removed");
    let Some(completion) = Block::copy(completion) else {
        return;
    };
    engine::defer(move || completion.call0());
}

unsafe extern "C" fn remove_data_modified_since(
    _receiver: Id,
    _sel: Sel,
    _types: Id,
    _date: Id,
    completion: Id,
) {
    // SAFETY: the same handler shape as the records form.
    unsafe { remove_data_for_records(_receiver, _sel, _types, std::ptr::null_mut(), completion) };
}

fn cookie_manager(engine: &Engine) -> Ptr {
    // SAFETY: the engine's own session.
    unsafe { (engine.api.webkit_network_session_get_cookie_manager)(engine.session as Ptr) }
}

/// The `NSHTTPCookie` for a libsoup cookie.
///
/// # Safety
///
/// `cookie` must be a live `SoupCookie`.
unsafe fn cookie_object(api: &gtk::Api, cookie: Ptr) -> Id {
    // SAFETY: the caller's obligation; each getter returns a string the cookie owns.
    let (name, value, domain, path, secure, expires) = unsafe {
        (
            gtk::text((api.soup_cookie_get_name)(cookie)),
            gtk::text((api.soup_cookie_get_value)(cookie)),
            gtk::text((api.soup_cookie_get_domain)(cookie)),
            gtk::text((api.soup_cookie_get_path)(cookie)),
            (api.soup_cookie_get_secure)(cookie) != 0,
            (api.soup_cookie_get_expires)(cookie),
        )
    };
    let mut pairs: Vec<(Id, Id)> = [
        ("NSHTTPCookieName", name.as_str()),
        ("NSHTTPCookieValue", value.as_str()),
        ("NSHTTPCookieDomain", domain.as_str()),
        ("NSHTTPCookiePath", path.as_str()),
    ]
    .into_iter()
    .chain(secure.then_some(("NSHTTPCookieSecure", "TRUE")))
    .map(|(key, value)| (engine::string(key), engine::string(value)))
    .collect();
    if !expires.is_null() {
        // SAFETY: a `GDateTime` the cookie owns.
        let seconds = unsafe { (api.g_date_time_to_unix)(expires) };
        pairs.push((
            engine::string("NSHTTPCookieExpires"),
            foundation::timer::date_since_1970(seconds as f64),
        ));
    }
    let pairs: Vec<(CFTypeRef, CFTypeRef)> = pairs
        .into_iter()
        .map(|(key, value)| (key as CFTypeRef, value as CFTypeRef))
        .collect();
    let properties = corefoundation::collections::new_dictionary(
        &pairs,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    let class = objc::class::lookup(c"NSHTTPCookie").expect("registered by Foundation");
    // SAFETY: `+cookieWithProperties:` is `id (Class, SEL, NSDictionary *)`.
    let cookie = unsafe {
        send!(objc::class::ptr(class) as Id, "cookieWithProperties:", properties as Id => Id; Id)
    };
    objc::arc::release(properties as Id);
    cookie
}

/// A libsoup cookie for an `NSHTTPCookie`, owned by the caller.
///
/// Foundation keeps a cookie's domain without its leading dot, which to
/// libsoup is a host-only cookie. The dot is put back, so a cookie Studio
/// stored for `example.com` is sent to `www.example.com` as the domain cookie it
/// was.
///
/// # Safety
///
/// `cookie` must be an `NSHTTPCookie`.
unsafe fn soup_cookie(api: &gtk::Api, cookie: Id) -> Ptr {
    // SAFETY: the caller's obligation; each accessor is `id (id, SEL)`.
    let (name, value, domain, path, secure, expires) = unsafe {
        (
            engine::text_of(send!(cookie, "name"; Id)),
            engine::text_of(send!(cookie, "value"; Id)),
            engine::text_of(send!(cookie, "domain"; Id)),
            engine::text_of(send!(cookie, "path"; Id)),
            send!(cookie, "isSecure"; u8) != 0,
            send!(cookie, "expiresDate"; Id),
        )
    };
    let is_address = domain.parse::<std::net::IpAddr>().is_ok();
    let domain = match domain.starts_with('.') || is_address || domain.is_empty() {
        true => domain,
        false => format!(".{domain}"),
    };
    let path = match path.is_empty() {
        true => "/".to_string(),
        false => path,
    };
    let (name, value, domain, path) = (
        gtk::c_string(&name),
        gtk::c_string(&value),
        gtk::c_string(&domain),
        gtk::c_string(&path),
    );
    // SAFETY: four NUL-terminated strings, which the cookie copies; -1 is a
    // session cookie until an expiry is set below.
    let soup = unsafe {
        (api.soup_cookie_new)(
            name.as_ptr(),
            value.as_ptr(),
            domain.as_ptr(),
            path.as_ptr(),
            -1,
        )
    };
    // SAFETY: a fresh cookie; `-timeIntervalSince1970` is `double (id, SEL)`.
    unsafe {
        (api.soup_cookie_set_secure)(soup, c_int::from(secure));
        if !expires.is_null() {
            let seconds = send!(expires, "timeIntervalSince1970"; f64);
            let date = (api.g_date_time_new_from_unix_utc)(seconds as i64);
            if !date.is_null() {
                (api.soup_cookie_set_expires)(soup, date);
                (api.g_date_time_unref)(date);
            }
        }
    }
    soup
}

unsafe extern "C" fn all_cookies_ready(source: Ptr, result: Ptr, data: Ptr) {
    // SAFETY: the box `get_all_cookies` leaked for this call.
    let completion = unsafe { Box::from_raw(data as *mut Block) };
    let Some(engine) = engine::get() else {
        return;
    };
    let api = engine.api;
    let pool = objc::arc::pool_push();
    let mut error = std::ptr::null_mut();
    // SAFETY: the manager and result this callback was invoked with.
    let list =
        unsafe { (api.webkit_cookie_manager_get_all_cookies_finish)(source, result, &mut error) };
    if let Some((_, message)) = unsafe { gtk::take_error(api, error) } {
        engine::warn_once(&format!("reading cookies failed: {message}"));
    }
    let mut objects = Vec::new();
    let mut node = list;
    while !node.is_null() {
        // SAFETY: a node of a list of `SoupCookie`s this call now owns.
        unsafe {
            let cookie = (*node).data;
            let object = cookie_object(api, cookie);
            if !object.is_null() {
                objects.push(object as CFTypeRef);
            }
            (api.soup_cookie_free)(cookie);
            node = (*(node as *const GList)).next;
        }
    }
    // SAFETY: the list's nodes; their data was freed above.
    unsafe { (api.g_list_free)(list) };
    if engine::tracing() {
        eprintln!(
            "rosemary: webkit: getAllCookies: {} cookie(s)",
            objects.len()
        );
    }
    let array = corefoundation::collections::new_array(
        &objects,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    completion.call1(objc::arc::autorelease(array as Id));
    objc::arc::pool_pop(pool);
}

unsafe extern "C" fn get_all_cookies(_receiver: Id, _sel: Sel, completion: Id) {
    let Some(completion) = Block::copy(completion) else {
        return;
    };
    let Some(engine) = engine::get() else {
        engine::defer(move || completion.call1(empty_array()));
        return;
    };
    let data = Box::into_raw(Box::new(completion)) as Ptr;
    // SAFETY: the engine's cookie manager; the callback takes the box back.
    unsafe {
        (engine.api.webkit_cookie_manager_get_all_cookies)(
            cookie_manager(engine),
            std::ptr::null_mut(),
            Some(all_cookies_ready),
            data,
        )
    };
}

/// A pending add or delete: the cookie to free and the handler to call.
struct Change {
    soup: usize,
    completion: Option<Block>,
    adding: bool,
}

unsafe extern "C" fn change_ready(source: Ptr, result: Ptr, data: Ptr) {
    // SAFETY: the box `change_cookie` leaked for this call.
    let change = unsafe { Box::from_raw(data as *mut Change) };
    let Some(engine) = engine::get() else {
        return;
    };
    let api = engine.api;
    let mut error = std::ptr::null_mut();
    // SAFETY: the manager and result this callback was invoked with.
    unsafe {
        match change.adding {
            true => (api.webkit_cookie_manager_add_cookie_finish)(source, result, &mut error),
            false => (api.webkit_cookie_manager_delete_cookie_finish)(source, result, &mut error),
        };
        if let Some((_, message)) = gtk::take_error(api, error) {
            engine::warn_once(&format!("changing a cookie failed: {message}"));
        }
        (api.soup_cookie_free)(change.soup as Ptr);
    }
    if let Some(completion) = change.completion {
        let pool = objc::arc::pool_push();
        completion.call0();
        objc::arc::pool_pop(pool);
    }
}

fn change_cookie(cookie: Id, completion: Id, adding: bool) {
    let completion = Block::copy(completion);
    let Some(engine) = engine::get().filter(|_| !cookie.is_null()) else {
        if let Some(completion) = completion {
            engine::defer(move || completion.call0());
        }
        return;
    };
    let api = engine.api;
    // SAFETY: an `NSHTTPCookie`, checked non-nil above.
    let soup = unsafe { soup_cookie(api, cookie) };
    if engine::tracing() {
        eprintln!(
            "rosemary: webkit: {} cookie {}",
            if adding { "setting" } else { "deleting" },
            // SAFETY: a live cookie.
            unsafe { gtk::text((api.soup_cookie_get_name)(soup)) }
        );
    }
    let change = Box::into_raw(Box::new(Change {
        soup: soup as usize,
        completion,
        adding,
    })) as Ptr;
    let manager = cookie_manager(engine);
    // SAFETY: the engine's manager and a cookie it copies; the callback takes
    // the box back and frees the cookie.
    unsafe {
        match adding {
            true => (api.webkit_cookie_manager_add_cookie)(
                manager,
                soup,
                std::ptr::null_mut(),
                Some(change_ready),
                change,
            ),
            false => (api.webkit_cookie_manager_delete_cookie)(
                manager,
                soup,
                std::ptr::null_mut(),
                Some(change_ready),
                change,
            ),
        }
    }
}

unsafe extern "C" fn set_cookie(_receiver: Id, _sel: Sel, cookie: Id, completion: Id) {
    change_cookie(cookie, completion, true);
}

unsafe extern "C" fn delete_cookie(_receiver: Id, _sel: Sel, cookie: Id, completion: Id) {
    change_cookie(cookie, completion, false);
}

pub fn install() {
    let store = corefoundation::base::abstract_class(c"WKWebsiteDataStore", None);
    crate::class_methods!(store, {
        "defaultDataStore" => default_data_store, c"@16@0:8";
        "nonPersistentDataStore" => non_persistent_data_store, c"@16@0:8";
    });
    crate::methods!(store, {
        "httpCookieStore" => http_cookie_store, c"@16@0:8";
        "fetchDataRecordsOfTypes:completionHandler:" => fetch_data_records, c"v32@0:8@16@?24";
        "removeDataOfTypes:forDataRecords:completionHandler:" => remove_data_for_records, c"v40@0:8@16@24@?32";
        "removeDataOfTypes:modifiedSince:completionHandler:" => remove_data_modified_since, c"v40@0:8@16@24@?32";
    });

    let cookies = corefoundation::base::abstract_class(c"WKHTTPCookieStore", None);
    crate::methods!(cookies, {
        "getAllCookies:" => get_all_cookies, c"v24@0:8@?16";
        "setCookie:completionHandler:" => set_cookie, c"v32@0:8@16@?24";
        "deleteCookie:completionHandler:" => delete_cookie, c"v32@0:8@16@?24";
    });
}
