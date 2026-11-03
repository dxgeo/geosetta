//! Where each of a feature's properties goes when it is written as KML: into
//! one of the feature element's own members, or into `<ExtendedData>` — the
//! inverse of [`crate::kml::project`], shared by the writer that builds a
//! document from the IR and the one that reconciles an edit into a document
//! it read.
//!
//! *A key is never lost, only moved.* Each of
//! [`project::element_keys`] is written as its element when its value is one
//! the element admits — `timestamp` a KML time, `altitudeMode` one of its
//! three modes, `tessellate` on a geometry that has one — and in
//! `<ExtendedData>` otherwise, which reads back under the same key. Only a
//! key's first occurrence is a member; a second one, as a document whose
//! `<ExtendedData>` repeats `name` gives, stays in `<ExtendedData>` where it
//! came from.


use crate::feature::FeatureRef;
use crate::kml::model::KmlElement;
use crate::kml::project::{self, is_kml_time, ALTITUDE_MEMBERS, BEGIN, DESCRIPTION, END, ICON, NAME, TIMESTAMP};
use crate::kml::writer::property_text;
use crate::table::Value;

/// A feature's time, as its element will hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Time {
    None,
    Stamp(String),
    Span(Option<String>, Option<String>),
}

impl Time {
    /// The time a stored element projects, in the same shape.
    pub fn of(values: &[(&str, &str)]) -> Time {
        let get = |key: &str| values.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string());
        match get(TIMESTAMP) {
            Some(when) => Time::Stamp(when),
            None => match (get(BEGIN), get(END)) {
                (None, None) => Time::None,
                (begin, end) => Time::Span(begin, end),
            },
        }
    }
}

/// Every property of a feature, sorted into where it is written.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Placement<'f> {
    pub name: Option<String>,
    pub description: Option<String>,
    pub time: Time,
    pub icon: Option<String>,
    /// Each altitude member to write, as its element and text, in
    /// [`ALTITUDE_MEMBERS`] order.
    pub altitude: Vec<(KmlElement, String)>,
    /// Everything else, in the IR's order.
    pub extended: Vec<(&'f str, Value<'f>)>,
}

/// Sort `f`'s properties for a feature element of kind `element`, whose
/// geometry can hold an altitude member when `carries` says so.
pub(crate) fn place<'f>(f: FeatureRef<'f>, element: KmlElement, carries: impl Fn(KmlElement) -> bool) -> Placement<'f> {
    // What is not placed in an element stays, in order, as `extended`.
    let mut properties: Vec<(&'f str, Value<'f>)> = f.properties().map(|(k, v)| (&**k, v)).collect();
    // Which property a member would come from: its key's first occurrence.
    let first = |key: &str| {
        project::element_keys(element).any(|k| k == key).then(|| properties.iter().position(|(k, _)| *k == key)).flatten()
    };
    // The positions placed, which most features have none of.
    let mut used: Vec<usize> = Vec::new();
    let mut take = |key: &str, ok: &dyn Fn(&str) -> bool| -> Option<String> {
        let i = first(key)?;
        let text = property_text(properties[i].1);
        ok(&text).then(|| {
            used.push(i);
            text
        })
    };
    let name = take(NAME, &|_| true);
    let description = take(DESCRIPTION, &|_| true);
    // One time primitive per feature: a stamp, else a span. A begin or end
    // beside a stamp has no element left to go in.
    let time = match take(TIMESTAMP, &is_kml_time) {
        Some(when) => Time::Stamp(when),
        None => match (take(BEGIN, &is_kml_time), take(END, &is_kml_time)) {
            (None, None) => Time::None,
            (begin, end) => Time::Span(begin, end),
        },
    };
    let icon = take(ICON, &|v| !v.is_empty());
    let mut altitude: Vec<(KmlElement, String)> = Vec::new();
    for (key, member) in ALTITUDE_MEMBERS {
        // The two modes are one choice in the schema (`altitudeModeGroup`),
        // so a sea-floor mode beside a ground one is not a member.
        let exclusive =
            member == KmlElement::SeaFloorAltitudeMode && altitude.iter().any(|(m, _)| *m == KmlElement::AltitudeMode);
        if let Some(v) = take(key, &|v| carries(member) && !exclusive && admits(member, v)) {
            altitude.push((member, v));
        }
    }
    if !used.is_empty() {
        let mut i = 0;
        properties.retain(|_| {
            i += 1;
            !used.contains(&(i - 1))
        });
    }
    Placement { name, description, time, icon, altitude, extended: properties }
}

/// Whether `text` is a value the altitude member `member` admits.
fn admits(member: KmlElement, text: &str) -> bool {
    match member {
        KmlElement::AltitudeMode => matches!(text, "clampToGround" | "relativeToGround" | "absolute"),
        KmlElement::SeaFloorAltitudeMode => matches!(text, "clampToSeaFloor" | "relativeToSeaFloor"),
        KmlElement::Extrude | KmlElement::Tessellate => matches!(text, "0" | "1" | "true" | "false"),
        _ => false,
    }
}
