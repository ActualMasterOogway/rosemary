//! `NSMenu` and `NSMenuItem`.
//!
//! Bookkeeping and nothing else: SDL has no menu bar, so nothing here is drawn
//! and no item is ever clicked. What they still have to do is hold what a
//! program puts in them, because a program builds its menus at launch and then
//! reads them back - `-itemWithTag:`, `-setState:` on the item it just found,
//! `[[NSApp mainMenu] itemAtIndex:0]` - long before anybody could have clicked
//! one.
//!
//! An item's target and action are kept and never fired. That is the honest
//! shape while there are no events: firing would need the responder chain, and a
//! program that sets an action expects it to happen when the user picks the item
//! rather than at the moment it is set.

use std::ffi::c_void;

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

#[repr(C)]
pub struct Menu {
    isa: *const c_void,
    title: Id,
    /// A mutable `NSArray` of items, made on first use.
    items: Id,
    /// Unretained, as every AppKit delegate is.
    delegate: Id,
    /// The item this menu is the submenu of. `-supermenu` answers that item's
    /// menu rather than the item, so it follows the item when it moves.
    supermenu: Id,
}

#[repr(C)]
pub struct MenuItem {
    isa: *const c_void,
    title: Id,
    submenu: Id,
    /// Unretained: a target owns the item, not the other way round.
    target: Id,
    key_equivalent: Id,
    menu: Id,
    action: Sel,
    tag: isize,
    state: isize,
    enabled: usize,
    hidden: usize,
    /// Set only by `+separatorItem`. A separator is an ordinary item as far as
    /// this bookkeeping goes; what makes it one is that it answers
    /// `-isSeparatorItem`, which is how a caller walking a menu tells a divider
    /// from something it should read a title off.
    separator: usize,
    /// Kept rather than ignored, because a caller that sets a key equivalent's
    /// modifiers reads them back to build its own shortcut table.
    modifier_mask: usize,
    tool_tip: Id,
    image: Id,
    /// Kept and never drawn, like everything else here.
    view: Id,
}

pub const MENU_INSTANCE_SIZE: usize = size_of::<Menu>();
pub const ITEM_INSTANCE_SIZE: usize = size_of::<MenuItem>();

fn menu_of<'a>(receiver: Id) -> &'a mut Menu {
    // SAFETY: a menu of this crate's.
    unsafe { &mut *(receiver as *mut Menu) }
}

fn item_of<'a>(receiver: Id) -> &'a mut MenuItem {
    // SAFETY: a menu item of this crate's.
    unsafe { &mut *(receiver as *mut MenuItem) }
}

/// The item list, made on first use.
fn items_of(receiver: Id) -> &'static mut corefoundation::collections::ArrayContents {
    let menu = menu_of(receiver);
    if menu.items.is_null() {
        let array = corefoundation::collections::new_array(
            &[],
            corefoundation::callbacks::Policy::from_type_callbacks(),
            true,
        );
        menu.items = array as Id;
    }
    // SAFETY: an array this function made.
    unsafe { corefoundation::collections::array(menu.items as *const c_void) }
        .expect("the array this function just made")
}

unsafe extern "C" fn init_with_title(receiver: Id, _sel: Sel, title: Id) -> Id {
    menu_of(receiver).title = objc::arc::retain(title);
    receiver
}

unsafe extern "C" fn menu_title(receiver: Id, _sel: Sel) -> Id {
    menu_of(receiver).title
}

unsafe extern "C" fn set_menu_title(receiver: Id, _sel: Sel, title: Id) {
    let menu = menu_of(receiver);
    objc::arc::release(menu.title);
    menu.title = objc::arc::retain(title);
}

unsafe extern "C" fn menu_items(receiver: Id, _sel: Sel) -> Id {
    items_of(receiver);
    menu_of(receiver).items
}

/// `-setItemArray:`, and the `NSMenuItems` key a nib carries.
unsafe extern "C" fn set_menu_items(receiver: Id, _sel: Sel, items: Id) {
    let list = items_of(receiver);
    list.values.clear();
    // SAFETY: null or an array.
    let Some(source) = (unsafe { corefoundation::collections::array(items as *const c_void) })
    else {
        return;
    };
    for value in source.values.clone() {
        list.values.push(list.policy.retain(value));
        item_of(value as Id).menu = receiver;
    }
}

unsafe extern "C" fn number_of_items(receiver: Id, _sel: Sel) -> isize {
    items_of(receiver).values.len() as isize
}

unsafe extern "C" fn item_at_index(receiver: Id, _sel: Sel, index: isize) -> Id {
    let list = items_of(receiver);
    match corefoundation::collections::at(index, list.values.len()) {
        Some(at) => list.values[at] as Id,
        None => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn add_item(receiver: Id, _sel: Sel, item: Id) {
    if item.is_null() {
        return;
    }
    let list = items_of(receiver);
    list.values.push(list.policy.retain(item as *const c_void));
    item_of(item).menu = receiver;
}

unsafe extern "C" fn insert_item(receiver: Id, _sel: Sel, item: Id, index: isize) {
    if item.is_null() {
        return;
    }
    let list = items_of(receiver);
    let at = (index.max(0) as usize).min(list.values.len());
    let retained = list.policy.retain(item as *const c_void);
    list.values.insert(at, retained);
    item_of(item).menu = receiver;
}

/// Take the item at `at` out of the menu, which then no longer owns it.
fn detach(receiver: Id, at: usize) {
    let list = items_of(receiver);
    let value = list.values.remove(at);
    let item = item_of(value as Id);
    if item.menu == receiver {
        item.menu = std::ptr::null_mut();
    }
    list.policy.release(value);
}

unsafe extern "C" fn remove_item(receiver: Id, sel: Sel, item: Id) {
    // SAFETY: `-indexOfItem:` is `NSInteger (id, SEL, id)`.
    let at = unsafe { index_of_item(receiver, sel, item) };
    if at >= 0 {
        detach(receiver, at as usize);
    }
}

unsafe extern "C" fn remove_item_at_index(receiver: Id, _sel: Sel, index: isize) {
    let list = items_of(receiver);
    if let Some(at) = corefoundation::collections::at(index, list.values.len()) {
        detach(receiver, at);
    }
}

unsafe extern "C" fn remove_all_items(receiver: Id, _sel: Sel) {
    while !items_of(receiver).values.is_empty() {
        detach(receiver, items_of(receiver).values.len() - 1);
    }
}

unsafe extern "C" fn index_of_item(receiver: Id, _sel: Sel, item: Id) -> isize {
    let list = items_of(receiver);
    list.values
        .iter()
        .position(|v| *v == item as *const c_void)
        .map_or(-1, |at| at as isize)
}

/// `-itemWithTag:`, searching this menu and then its submenus.
///
/// Recursive, as Apple's is: a program tags an item deep in the hierarchy and
/// looks for it from the main menu.
unsafe extern "C" fn item_with_tag(receiver: Id, sel: Sel, tag: isize) -> Id {
    for value in items_of(receiver).values.clone() {
        let item = value as Id;
        if item_of(item).tag == tag {
            return item;
        }
        let submenu = item_of(item).submenu;
        if !submenu.is_null() {
            // SAFETY: `-itemWithTag:` is `id (id, SEL, NSInteger)`.
            let found = unsafe { item_with_tag(submenu, sel, tag) };
            if !found.is_null() {
                return found;
            }
        }
    }
    std::ptr::null_mut()
}

/// `-itemWithTitle:`, which is **not** recursive - unlike `-itemWithTag:` above.
///
/// Apple's searches this menu's own items only, and the difference is not an
/// oversight in either place: a tag is unique across a hierarchy by convention
/// and a title is not, so a recursive search by title would answer with an item
/// from a submenu the caller was not asking about.
unsafe extern "C" fn item_with_title(receiver: Id, _sel: Sel, title: Id) -> Id {
    // SAFETY: null or an `NSString`.
    let wanted = unsafe { foundation::string::bytes(title) };
    for value in items_of(receiver).values.clone() {
        let item = value as Id;
        // SAFETY: an item of this menu, whose title is null or an `NSString`.
        if unsafe { foundation::string::bytes(item_of(item).title) } == wanted {
            return item;
        }
    }
    std::ptr::null_mut()
}

/// `-indexOfItemWithTitle:`, the same search reported as a position.
unsafe extern "C" fn index_of_item_with_title(receiver: Id, sel: Sel, title: Id) -> isize {
    // SAFETY: `-itemWithTitle:` is `id (id, SEL, id)`.
    let found = unsafe { item_with_title(receiver, sel, title) };
    match found.is_null() {
        // `NSNotFound` is `NSIntegerMax` and not -1, which is the value a
        // caller compares against.
        true => isize::MAX,
        // SAFETY: `-indexOfItem:` is `NSInteger (id, SEL, id)`, and `found` is
        // an item of this menu.
        false => unsafe { index_of_item(receiver, sel, found) },
    }
}

unsafe extern "C" fn menu_delegate(receiver: Id, _sel: Sel) -> Id {
    menu_of(receiver).delegate
}

unsafe extern "C" fn set_menu_delegate(receiver: Id, _sel: Sel, value: Id) {
    menu_of(receiver).delegate = value;
}

unsafe extern "C" fn supermenu(receiver: Id, _sel: Sel) -> Id {
    match menu_of(receiver).supermenu {
        holder if holder.is_null() => holder,
        holder => item_of(holder).menu,
    }
}

unsafe extern "C" fn init_item(
    receiver: Id,
    _sel: Sel,
    title: Id,
    action: Sel,
    key_equivalent: Id,
) -> Id {
    let item = item_of(receiver);
    item.title = objc::arc::retain(title);
    item.action = action;
    item.key_equivalent = objc::arc::retain(key_equivalent);
    item.enabled = 1;
    receiver
}

unsafe extern "C" fn item_title(receiver: Id, _sel: Sel) -> Id {
    item_of(receiver).title
}

unsafe extern "C" fn set_item_title(receiver: Id, _sel: Sel, title: Id) {
    let item = item_of(receiver);
    objc::arc::release(item.title);
    item.title = objc::arc::retain(title);
}

unsafe extern "C" fn item_action(receiver: Id, _sel: Sel) -> Sel {
    item_of(receiver).action
}

unsafe extern "C" fn set_item_action(receiver: Id, _sel: Sel, action: Sel) {
    item_of(receiver).action = action;
}

unsafe extern "C" fn item_target(receiver: Id, _sel: Sel) -> Id {
    item_of(receiver).target
}

unsafe extern "C" fn set_item_target(receiver: Id, _sel: Sel, target: Id) {
    item_of(receiver).target = target;
}

unsafe extern "C" fn item_submenu(receiver: Id, _sel: Sel) -> Id {
    item_of(receiver).submenu
}

unsafe extern "C" fn set_item_submenu(receiver: Id, _sel: Sel, submenu: Id) {
    let item = item_of(receiver);
    if !item.submenu.is_null() && menu_of(item.submenu).supermenu == receiver {
        menu_of(item.submenu).supermenu = std::ptr::null_mut();
    }
    objc::arc::release(item.submenu);
    item.submenu = objc::arc::retain(submenu);
    if !submenu.is_null() {
        menu_of(submenu).supermenu = receiver;
    }
}

unsafe extern "C" fn item_menu(receiver: Id, _sel: Sel) -> Id {
    item_of(receiver).menu
}

unsafe extern "C" fn set_item_menu(receiver: Id, _sel: Sel, menu: Id) {
    item_of(receiver).menu = menu;
}

unsafe extern "C" fn key_equivalent(receiver: Id, _sel: Sel) -> Id {
    item_of(receiver).key_equivalent
}

unsafe extern "C" fn set_key_equivalent(receiver: Id, _sel: Sel, key: Id) {
    let item = item_of(receiver);
    objc::arc::release(item.key_equivalent);
    item.key_equivalent = objc::arc::retain(key);
}

/// The plain integer accessors, each reading or writing one field.
macro_rules! integer_accessors {
    ($($get:ident / $set:ident => $field:ident),* $(,)?) => {
        $(
            unsafe extern "C" fn $get(receiver: Id, _sel: Sel) -> isize {
                item_of(receiver).$field as isize
            }
            unsafe extern "C" fn $set(receiver: Id, _sel: Sel, value: isize) {
                item_of(receiver).$field = value as _;
            }
        )*
    };
}

integer_accessors! {
    item_tag / set_item_tag => tag,
    item_state / set_item_state => state,
}

unsafe extern "C" fn is_enabled(receiver: Id, _sel: Sel) -> u8 {
    (item_of(receiver).enabled != 0) as u8
}

unsafe extern "C" fn set_enabled(receiver: Id, _sel: Sel, enabled: u8) {
    item_of(receiver).enabled = enabled as usize;
}

unsafe extern "C" fn is_hidden(receiver: Id, _sel: Sel) -> u8 {
    (item_of(receiver).hidden != 0) as u8
}

unsafe extern "C" fn set_hidden(receiver: Id, _sel: Sel, hidden: u8) {
    item_of(receiver).hidden = hidden as usize;
}

/// The requests that describe how a menu *looks*, which nothing here draws.
/// `+[NSMenuItem separatorItem]`, the divider in a menu.
///
/// A fresh item each time, as Apple's is: a separator goes into exactly one menu
/// at one index, and a shared singleton put into two menus would be at one index
/// in both.
unsafe extern "C" fn separator_item(_class: Id, _sel: Sel) -> Id {
    let class = objc::class::lookup(c"NSMenuItem").expect("declared before its methods");
    let object = objc::object::create_instance(class, 0);
    item_of(object).separator = 1;
    objc::arc::autorelease(object)
}

unsafe extern "C" fn is_separator_item(receiver: Id, _sel: Sel) -> u8 {
    item_of(receiver).separator as u8
}

/// `-hasSubmenu`, which a caller asks before descending rather than reading
/// `-submenu` and testing it for nil.
unsafe extern "C" fn has_submenu(receiver: Id, _sel: Sel) -> u8 {
    !item_of(receiver).submenu.is_null() as u8
}

unsafe extern "C" fn key_equivalent_modifier_mask(receiver: Id, _sel: Sel) -> usize {
    item_of(receiver).modifier_mask
}

unsafe extern "C" fn set_key_equivalent_modifier_mask(receiver: Id, _sel: Sel, mask: usize) {
    item_of(receiver).modifier_mask = mask;
}

unsafe extern "C" fn item_tool_tip(receiver: Id, _sel: Sel) -> Id {
    item_of(receiver).tool_tip
}

unsafe extern "C" fn set_item_tool_tip(receiver: Id, _sel: Sel, text: Id) {
    let item = item_of(receiver);
    objc::arc::release(item.tool_tip);
    item.tool_tip = objc::arc::retain(text);
}

unsafe extern "C" fn item_image(receiver: Id, _sel: Sel) -> Id {
    item_of(receiver).image
}

unsafe extern "C" fn set_item_image(receiver: Id, _sel: Sel, image: Id) {
    let item = item_of(receiver);
    objc::arc::release(item.image);
    item.image = objc::arc::retain(image);
}

unsafe extern "C" fn item_view(receiver: Id, _sel: Sel) -> Id {
    item_of(receiver).view
}

unsafe extern "C" fn set_item_view(receiver: Id, _sel: Sel, view: Id) {
    let item = item_of(receiver);
    objc::arc::release(item.view);
    item.view = objc::arc::retain(view);
}

/// `-indexOfItemWithSubmenu:`, `-indexOfItem:` asked the other way round.
///
/// How a caller finds where a submenu was attached so it can replace or remove it
/// - `-1` for a submenu this menu does not hold, which is the convention every
/// `indexOf` on `NSMenu` uses.
unsafe extern "C" fn index_of_item_with_submenu(receiver: Id, _sel: Sel, submenu: Id) -> isize {
    items_of(receiver)
        .values
        .iter()
        .position(|value| item_of(*value as Id).submenu == submenu)
        .map_or(-1, |at| at as isize)
}

unsafe extern "C" fn ignored_flag(_receiver: Id, _sel: Sel, _flag: u8) {}

unsafe extern "C" fn ignored_object(_receiver: Id, _sel: Sel, _object: Id) {}

pub(crate) fn install() {
    let menu = objc::class::lookup(c"NSMenu").expect("declared before its methods");
    methods!(menu, {
        "initWithTitle:" => init_with_title, c"@24@0:8@16";
        "title" => menu_title, c"@16@0:8";
        "setTitle:" => set_menu_title, c"v24@0:8@16";
        "itemArray" => menu_items, c"@16@0:8";
        "setItemArray:" => set_menu_items, c"v24@0:8@16";
        "numberOfItems" => number_of_items, c"q16@0:8";
        "itemAtIndex:" => item_at_index, c"@24@0:8q16";
        "addItem:" => add_item, c"v24@0:8@16";
        "insertItem:atIndex:" => insert_item, c"v32@0:8@16q24";
        "removeItem:" => remove_item, c"v24@0:8@16";
        "removeItemAtIndex:" => remove_item_at_index, c"v24@0:8q16";
        "removeAllItems" => remove_all_items, c"v16@0:8";
        "indexOfItem:" => index_of_item, c"q24@0:8@16";
        "itemWithTag:" => item_with_tag, c"@24@0:8q16";
        "itemWithTitle:" => item_with_title, c"@24@0:8@16";
        "indexOfItemWithTitle:" => index_of_item_with_title, c"q24@0:8@16";
        "delegate" => menu_delegate, c"@16@0:8";
        "setDelegate:" => set_menu_delegate, c"v24@0:8@16";
        "supermenu" => supermenu, c"@16@0:8";
        "setAutoenablesItems:" => ignored_flag, c"v20@0:8c16";
        "setSubmenu:forItem:" => ignored_object, c"v32@0:8@16@24";
        "indexOfItemWithSubmenu:" => index_of_item_with_submenu, c"q24@0:8@16";
    });

    let item = objc::class::lookup(c"NSMenuItem").expect("declared before its methods");
    class_methods!(item, {
        "separatorItem" => separator_item, c"@16@0:8";
    });
    methods!(item, {
        "initWithTitle:action:keyEquivalent:" => init_item, c"@40@0:8@16:24@32";
        "title" => item_title, c"@16@0:8";
        "setTitle:" => set_item_title, c"v24@0:8@16";
        "action" => item_action, c":16@0:8";
        "setAction:" => set_item_action, c"v24@0:8:16";
        "target" => item_target, c"@16@0:8";
        "setTarget:" => set_item_target, c"v24@0:8@16";
        "submenu" => item_submenu, c"@16@0:8";
        "setSubmenu:" => set_item_submenu, c"v24@0:8@16";
        "menu" => item_menu, c"@16@0:8";
        "setMenu:" => set_item_menu, c"v24@0:8@16";
        "keyEquivalent" => key_equivalent, c"@16@0:8";
        "setKeyEquivalent:" => set_key_equivalent, c"v24@0:8@16";
        "tag" => item_tag, c"q16@0:8";
        "setTag:" => set_item_tag, c"v24@0:8q16";
        "state" => item_state, c"q16@0:8";
        "setState:" => set_item_state, c"v24@0:8q16";
        "isEnabled" => is_enabled, c"c16@0:8";
        "setEnabled:" => set_enabled, c"v20@0:8c16";
        "isHidden" => is_hidden, c"c16@0:8";
        "setHidden:" => set_hidden, c"v20@0:8c16";
        "setKeyEquivalentModifierMask:" => set_key_equivalent_modifier_mask, c"v24@0:8Q16";
        "keyEquivalentModifierMask" => key_equivalent_modifier_mask, c"Q16@0:8";
        "isSeparatorItem" => is_separator_item, c"c16@0:8";
        "hasSubmenu" => has_submenu, c"c16@0:8";
        "toolTip" => item_tool_tip, c"@16@0:8";
        "setToolTip:" => set_item_tool_tip, c"v24@0:8@16";
        "image" => item_image, c"@16@0:8";
        "setImage:" => set_item_image, c"v24@0:8@16";
        "view" => item_view, c"@16@0:8";
        "setView:" => set_item_view, c"v24@0:8@16";
        "setOnStateImage:" => ignored_object, c"v24@0:8@16";
        "setMixedStateImage:" => ignored_object, c"v24@0:8@16";
        "setRepresentedObject:" => ignored_object, c"v24@0:8@16";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu(title: &str) -> Id {
        crate::bootstrap();
        let object =
            objc::object::create_instance(objc::class::lookup(c"NSMenu").expect("NSMenu"), 0);
        // SAFETY: `-initWithTitle:` is `id (id, SEL, id)`.
        unsafe {
            init_with_title(
                object,
                objc::sel::get("initWithTitle:"),
                foundation::string::from_str(title),
            )
        }
    }

    fn item(tag: isize) -> Id {
        crate::bootstrap();
        let object = objc::object::create_instance(
            objc::class::lookup(c"NSMenuItem").expect("NSMenuItem"),
            0,
        );
        // SAFETY: `-setTag:` is `void (id, SEL, NSInteger)`.
        unsafe { set_item_tag(object, objc::sel::get("setTag:"), tag) };
        object
    }

    /// A separator answers `-isSeparatorItem` and an ordinary item does not,
    /// which is how a caller walking a menu tells a divider from something with a
    /// title to read.
    #[test]
    fn a_separator_says_so_and_an_ordinary_item_does_not() {
        crate::bootstrap();
        let class = objc::class::ptr(objc::class::lookup(c"NSMenuItem").expect("NSMenuItem")) as Id;
        // SAFETY: `+separatorItem` is `id (Class, SEL)`.
        let separator = unsafe { separator_item(class, objc::sel::get("separatorItem")) };
        let plain = item(1);
        // SAFETY: `-isSeparatorItem` is `BOOL (id, SEL)` on items of this crate's.
        unsafe {
            let sel = objc::sel::get("isSeparatorItem");
            assert_eq!(is_separator_item(separator, sel), 1);
            assert_eq!(is_separator_item(plain, sel), 0);
        }
        // And two separators are two items, so each can sit at its own index.
        // SAFETY: as above.
        let second = unsafe { separator_item(class, objc::sel::get("separatorItem")) };
        assert_ne!(separator, second);
    }

    /// `-hasSubmenu` and `-indexOfItemWithSubmenu:` are the pair a caller uses to
    /// find where a submenu was attached.
    #[test]
    fn a_submenu_is_found_by_the_item_holding_it() {
        let parent = menu("Parent");
        let child = menu("Child");
        let holder = item(7);
        let plain = item(8);
        // SAFETY: menus and items this test made.
        unsafe {
            assert_eq!(has_submenu(holder, objc::sel::get("hasSubmenu")), 0);
            set_item_submenu(holder, objc::sel::get("setSubmenu:"), child);
            assert_eq!(has_submenu(holder, objc::sel::get("hasSubmenu")), 1);
            add_item(parent, objc::sel::get("addItem:"), plain);
            add_item(parent, objc::sel::get("addItem:"), holder);
            assert_eq!(
                index_of_item_with_submenu(
                    parent,
                    objc::sel::get("indexOfItemWithSubmenu:"),
                    child
                ),
                1
            );
            // A submenu this menu does not hold is -1, not 0.
            assert_eq!(
                index_of_item_with_submenu(
                    parent,
                    objc::sel::get("indexOfItemWithSubmenu:"),
                    menu("Elsewhere")
                ),
                -1
            );
        }
    }

    /// A tag is found through a submenu, which is how a program reaches the item
    /// it wants from the main menu.
    #[test]
    fn an_item_is_found_by_tag_through_a_submenu() {
        let root = menu("Main");
        let file = menu("File");
        let holder = item(0);
        let wanted = item(42);

        // SAFETY: each prototype is the one its selector declares.
        unsafe {
            set_item_submenu(holder, objc::sel::get("setSubmenu:"), file);
            add_item(root, objc::sel::get("addItem:"), holder);
            add_item(file, objc::sel::get("addItem:"), wanted);

            assert_eq!(
                item_with_tag(root, objc::sel::get("itemWithTag:"), 42),
                wanted
            );
            assert!(item_with_tag(root, objc::sel::get("itemWithTag:"), 7).is_null());
            // And the submenu knows the menu it hangs from, not the item.
            assert_eq!(supermenu(file, objc::sel::get("supermenu")), root);
        }
    }

    /// Adding, indexing and removing agree with each other.
    #[test]
    fn a_menus_items_are_the_ones_added_to_it() {
        let menu = menu("Edit");
        let first = item(1);
        let second = item(2);
        // SAFETY: each prototype is the one its selector declares.
        unsafe {
            add_item(menu, objc::sel::get("addItem:"), first);
            insert_item(menu, objc::sel::get("insertItem:atIndex:"), second, 0);
            assert_eq!(number_of_items(menu, objc::sel::get("numberOfItems")), 2);
            assert_eq!(
                item_at_index(menu, objc::sel::get("itemAtIndex:"), 0),
                second
            );
            assert_eq!(
                index_of_item(menu, objc::sel::get("indexOfItem:"), first),
                1
            );

            remove_item(menu, objc::sel::get("removeItem:"), second);
            assert_eq!(number_of_items(menu, objc::sel::get("numberOfItems")), 1);
            assert_eq!(
                item_at_index(menu, objc::sel::get("itemAtIndex:"), 0),
                first
            );
            // An index outside the list is nil rather than a panic.
            assert!(item_at_index(menu, objc::sel::get("itemAtIndex:"), 9).is_null());
            // A removed item no longer names the menu it left.
            assert!(item_menu(second, objc::sel::get("menu")).is_null());
            remove_item_at_index(menu, objc::sel::get("removeItemAtIndex:"), 0);
            assert_eq!(number_of_items(menu, objc::sel::get("numberOfItems")), 0);
        }
    }
}
