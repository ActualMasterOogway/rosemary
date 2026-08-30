//! What the desktop is told this program is.
//!
//! None of it is guessable from the process: the executable is `rosetta3`, and
//! the only place the guest's own identity is written down is its `Info.plist`.
//! SDL passes what it is given here to the audio server - so a volume mixer
//! names the game rather than the runner - and to the compositor, which matches
//! the identifier against a desktop entry for window grouping and for the icon
//! shown outside the window.

use std::path::Path;

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
    sdl3::set_app_metadata(&sdl3::AppMetadata {
        name: owned(bundle.and_then(|b| b.display_name()))
            .or_else(|| Some(executable.file_stem()?.to_string_lossy().into_owned())),
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
