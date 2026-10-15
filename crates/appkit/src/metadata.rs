use std::ffi::CString;
use std::path::Path;
use std::sync::OnceLock;

static NAME: OnceLock<CString> = OnceLock::new();

/// The guest's name as published, which is what a window is titled until the
/// guest says otherwise. Empty before [`publish`].
pub fn name() -> &'static std::ffi::CStr {
    NAME.get().map_or(c"", CString::as_c_str)
}

/// Tell SDL who the guest is.
///
/// Called when the main bundle is set rather than when the first window is
/// made, because SDL reads this once as its drivers come up and a window is
/// well after that.
///
/// A loose binary still gets a name: its own, which is a better answer for an
/// audio mixer than SDL's "SDL Application".
pub fn publish(executable: &Path) {
    let bundle = foundation::bundle::main();
    let owned = |text: Option<&str>| text.map(str::to_string);
    // Nothing in an `Info.plist` names the developer or a product page, so
    // SDL's `creator` and `url` are left unset rather than filled in from a key
    // that means something else.
    let name = owned(bundle.and_then(|b| b.display_name()))
        .or_else(|| Some(executable.file_stem()?.to_string_lossy().into_owned()));
    if let Some(title) = name.as_deref().and_then(|n| CString::new(n).ok()) {
        let _ = NAME.set(title);
    }
    sdl3::set_app_metadata(&sdl3::AppMetadata {
        name,
        version: owned(bundle.and_then(|b| b.version())),
        identifier: owned(bundle.and_then(|b| b.identifier())),
        copyright: owned(bundle.and_then(|b| b.copyright())),
        kind: bundle
            .and_then(|b| b.category())
            .map(|category| kind(category).to_string()),
        ..Default::default()
    });
}

/// `LSApplicationCategoryType` as one of the three words SDL knows.
///
/// The suffix rather than the whole identifier, because a game declares
/// `public.app-category.action-games` as readily as the bare
/// `public.app-category.games`.
fn kind(category: &str) -> &'static str {
    if category.ends_with("games") {
        return "game";
    }
    match category {
        "public.app-category.music" | "public.app-category.video" => "mediaplayer",
        _ => "application",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three answers SDL accepts, from the identifiers a bundle actually
    /// carries.
    #[test]
    fn a_category_becomes_one_of_sdls_three_kinds() {
        assert_eq!(kind("public.app-category.games"), "game");
        assert_eq!(kind("public.app-category.action-games"), "game");
        assert_eq!(kind("public.app-category.music"), "mediaplayer");
        assert_eq!(kind("public.app-category.productivity"), "application");
    }
}
