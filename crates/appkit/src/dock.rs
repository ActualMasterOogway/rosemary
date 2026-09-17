//! `NSDockTile`: the application's Dock icon, which this host does not have.
//!
//! A program reaches it to put a badge on the icon or draw its own content into
//! it. There is no Dock to show either in, so the tile keeps what it is given
//! and answers it back - a program that sets a badge and reads it to decide
//! whether to clear it must find its own value - and `-display` draws nothing.

use std::ffi::c_void;

use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

#[repr(C)]
pub struct DockTile {
    isa: *const c_void,
    badge_label: Id,
    content_view: Id,
    shows_application_badge: usize,
}

pub const INSTANCE_SIZE: usize = size_of::<DockTile>();

fn tile_of<'a>(receiver: Id) -> &'a mut DockTile {
    // SAFETY: a tile of this crate's.
    unsafe { &mut *(receiver as *mut DockTile) }
}

/// `-[NSApplication dockTile]`: one tile for the process, made on first use.
unsafe extern "C" fn dock_tile(_receiver: Id, _sel: Sel) -> Id {
    static TILE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *TILE.get_or_init(|| {
        let class = objc::class::lookup(c"NSDockTile").expect("declared before its methods");
        let tile = objc::object::create_instance(class, 0);
        // Apple's default: a badge set on the tile is shown.
        tile_of(tile).shows_application_badge = 1;
        tile as usize
    }) as Id
}

unsafe extern "C" fn badge_label(receiver: Id, _sel: Sel) -> Id {
    tile_of(receiver).badge_label
}

/// `-setBadgeLabel:`, which copies, as the property is declared.
unsafe extern "C" fn set_badge_label(receiver: Id, _sel: Sel, label: Id) {
    let tile = tile_of(receiver);
    let old = tile.badge_label;
    tile.badge_label = match label.is_null() {
        true => label,
        false => foundation::string::from_str(&String::from_utf8_lossy(
            // SAFETY: an `NSString`.
            &unsafe { foundation::string::bytes(label) },
        )),
    };
    objc::arc::release(old);
}

unsafe extern "C" fn content_view(receiver: Id, _sel: Sel) -> Id {
    tile_of(receiver).content_view
}

unsafe extern "C" fn set_content_view(receiver: Id, _sel: Sel, view: Id) {
    let tile = tile_of(receiver);
    let old = tile.content_view;
    tile.content_view = objc::arc::retain(view);
    objc::arc::release(old);
}

unsafe extern "C" fn shows_application_badge(receiver: Id, _sel: Sel) -> u8 {
    (tile_of(receiver).shows_application_badge != 0) as u8
}

unsafe extern "C" fn set_shows_application_badge(receiver: Id, _sel: Sel, shows: u8) {
    tile_of(receiver).shows_application_badge = shows as usize;
}

/// `-display`: there is no tile on screen to redraw.
unsafe extern "C" fn display(_receiver: Id, _sel: Sel) {}

/// `-owner`: the application, which owns the only tile made here.
unsafe extern "C" fn owner(_receiver: Id, _sel: Sel) -> Id {
    crate::application::shared()
}

pub(crate) fn install() {
    let application = objc::class::lookup(c"NSApplication").expect("declared before its methods");
    methods!(application, {
        "dockTile" => dock_tile, c"@16@0:8";
    });

    let class = objc::class::lookup(c"NSDockTile").expect("declared before its methods");
    methods!(class, {
        "badgeLabel" => badge_label, c"@16@0:8";
        "setBadgeLabel:" => set_badge_label, c"v24@0:8@16";
        "contentView" => content_view, c"@16@0:8";
        "setContentView:" => set_content_view, c"v24@0:8@16";
        "showsApplicationBadge" => shows_application_badge, c"c16@0:8";
        "setShowsApplicationBadge:" => set_shows_application_badge, c"v20@0:8c16";
        "display" => display, c"v16@0:8";
        "owner" => owner, c"@16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dock_tile_is_one_object_that_keeps_its_badge() {
        crate::bootstrap();
        // SAFETY: each is the signature it was registered with.
        unsafe {
            let first = dock_tile(std::ptr::null_mut(), objc::sel::get("dockTile"));
            let second = dock_tile(std::ptr::null_mut(), objc::sel::get("dockTile"));
            assert!(!first.is_null());
            assert_eq!(first, second);
            assert_eq!(
                shows_application_badge(first, objc::sel::get("showsApplicationBadge")),
                1
            );

            let label = foundation::string::from_str("3");
            set_badge_label(first, objc::sel::get("setBadgeLabel:"), label);
            let kept = badge_label(first, objc::sel::get("badgeLabel"));
            assert_eq!(foundation::string::bytes(kept), b"3");
            set_badge_label(
                first,
                objc::sel::get("setBadgeLabel:"),
                std::ptr::null_mut(),
            );
            assert!(badge_label(first, objc::sel::get("badgeLabel")).is_null());
        }
    }
}
