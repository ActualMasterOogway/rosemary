//! `@property` metadata.
//!
//! A property is not a method and not an ivar: it is a name and an attribute
//! string the compiler emitted beside them, and the only thing that reads it is
//! code introspecting a class it did not write. That is a small audience, and it
//! is exactly the audience this runtime has - a program asking what a class
//! declares gets an empty answer from a runtime that models none of this, and an
//! empty answer is indistinguishable from a class with no properties.
//!
//! The attribute string is the interesting half. `T@"NSString",C,N,V_name` is
//! comma-separated fields whose first character is the attribute's name and
//! whose remainder is its value, and the type field can itself contain a comma
//! inside quotes - `T@"NSDictionary<NSString*,id>"` - so splitting on commas
//! without tracking the quotes produces a type that stops halfway through.

use std::ffi::{CStr, CString, c_char};

/// One `@property`, leaked so the handle a caller holds stays valid.
pub struct Property {
    name: CString,
    attributes: CString,
}

impl Property {
    pub fn name(&self) -> *const c_char {
        self.name.as_ptr()
    }

    pub fn name_c(&self) -> &CStr {
        &self.name
    }

    pub fn attributes(&self) -> *const c_char {
        self.attributes.as_ptr()
    }

    /// The attribute string split into `(name, value)`, in the order it wrote
    /// them.
    ///
    /// A field with nothing after its first character has an empty value, which
    /// is how `C`, `N` and `R` are spelled - and is what Apple reports for them,
    /// rather than omitting the value.
    pub fn attribute_list(&self) -> Vec<(CString, CString)> {
        attributes_of(&self.attributes)
    }
}

/// An attribute string as `(name, value)` pairs.
///
/// Public because the replay pass in `rosetta3` needs it too: a guest's
/// `property_t` carries the string, and `class_addProperty` - whichever runtime
/// answers it - takes the pairs.
pub fn attributes_of(attributes: &CStr) -> Vec<(CString, CString)> {
    split_attributes(attributes.to_bytes())
        .into_iter()
        .filter_map(|field| {
            let (first, rest) = field.split_first()?;
            Some((
                CString::new(vec![*first]).ok()?,
                CString::new(rest.to_vec()).ok()?,
            ))
        })
        .collect()
}

pub fn make(name: &CStr, attributes: &CStr) -> &'static Property {
    Box::leak(Box::new(Property {
        name: CString::from(name),
        attributes: CString::from(attributes),
    }))
}

/// Split an attribute string on the commas that separate fields, ignoring the
/// ones inside a quoted type.
fn split_attributes(bytes: &[u8]) -> Vec<&[u8]> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (i, b) in bytes.iter().enumerate() {
        match b {
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                fields.push(&bytes[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < bytes.len() {
        fields.push(&bytes[start..]);
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_comma_inside_a_quoted_type_does_not_end_the_field() {
        let p = make(c"table", c"T@\"NSDictionary<NSString*,id>\",&,N,V_table");
        let attributes = p.attribute_list();
        let rendered: Vec<(String, String)> = attributes
            .iter()
            .map(|(n, v)| {
                (
                    n.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert_eq!(
            rendered,
            [
                ("T".into(), "@\"NSDictionary<NSString*,id>\"".to_string()),
                ("&".into(), String::new()),
                ("N".into(), String::new()),
                ("V".into(), "_table".into()),
            ]
        );
    }

    /// What clang emits for `@property (nonatomic, readonly) NSInteger count;`,
    /// which is the shape a program introspecting a class actually meets.
    #[test]
    fn a_plain_property_splits_into_its_three_fields() {
        let p = make(c"count", c"Tq,N,R,V_count");
        assert_eq!(p.attribute_list().len(), 4);
        assert_eq!(p.attribute_list()[0].1.to_bytes(), b"q");
    }
}
