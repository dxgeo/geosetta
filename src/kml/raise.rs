//! Raising a raised KML 2.2 document to KML 2.3 (`plans/kml-standard.org`
//! K5, D-K7, D-K10).
//!
//! *A respelling, and no reorder.* 2.3's content models are all `<all>`
//! groups, so any order 2.2 admits, 2.3 admits too. What changes is spelling:
//!
//! - *Each `gx:` element 2.3 adopted goes into the KML namespace* (D-K10) —
//!   `gx:Track` to `Track`, `gx:altitudeMode` to `seaFloorAltitudeMode` — where
//!   2.3 admits that element in that parent ([`children_2_3`]). Where it does
//!   not, the element stays `gx:`, which 2.3's open content admits anywhere:
//!   `gx:altitudeOffset` on a Point, which the `gx:` schema allows and 2.3
//!   does not, is carried rather than respelled into something invalid.
//! - *`<Snippet>` becomes `<snippet>`*, its successor, when nothing is lost:
//!   `<snippet>` is a plain string, so a `<Snippet>` with a `maxLines` other
//!   than its default, or any other attribute, stays as it is — deprecated,
//!   still 2.3.
//! - *`<Metadata>` becomes `<ExtendedData>`* when what it holds is what
//!   `<ExtendedData>` holds: `<Data>`, `<SchemaData>` and other namespaces'
//!   elements. Anything else, and it stays `<Metadata>`, deprecated, still
//!   2.3.
//! - *The root*: OGC's namespace — Google's pre-OGC 2.2 namespace is rebound
//!   to it wherever it is declared, and a document that declared none gains
//!   it — the stamp `2.3`, and 2.3's schema where the source named 2.2's.
//!
//! Nothing is dropped: 2.3's vocabulary is a superset of 2.2's, and every
//! parent/child pair 2.2 admits, 2.3 admits too
//! (`every_2_2_placement_is_a_2_3_placement`).
//!
//! The generated region is from `ogckml23.xsd` (SHA-256
//! `5222be3703c634add22c0bfa0f90df8935cb01c4a1c60b287d33dc88fc695bcd`) by
//! `tests/fixtures/gen_kml23_children.py`.

use std::rc::Rc;

use crate::kml::lower::{is_decl, ns_decl, Changes, SCHEMA_2_2, SCHEMA_2_3, XSI_NS};
use crate::kml::model::{KmlDocument, KmlElement, Kind, Node, Spelling, GOOGLE_KML_2_2_NS, KML_NS};
use crate::xml::XmlAttr;

/// Raise `doc`, a 2.2 document, to 2.3 in place.
pub(crate) fn raise(doc: &mut KmlDocument) -> Changes {
    let mut report = Changes::default();
    let root = &mut doc.root;
    match root.attrs.iter_mut().find(|a| a.ns.is_none() && a.name == "version") {
        Some(stamp) => stamp.value = "2.3".into(),
        None => root.attrs.push(XmlAttr { prefix: None, name: "version".into(), ns: None, value: "2.3".into() }),
    }
    for a in &mut root.attrs {
        if a.ns.as_deref() == Some(XSI_NS) && a.name == "schemaLocation" {
            a.value = a.value.replace(SCHEMA_2_2, SCHEMA_2_3);
        }
    }
    if doc.root_ns.is_none() {
        root.attrs.insert(0, ns_decl(None, KML_NS));
    }
    doc.root_ns = Some(Rc::from(KML_NS));
    let kml_prefix = root.prefix.clone();
    rebind_google(root);
    raise_children(root, KmlElement::Kml, kml_prefix.as_deref(), &mut report);
    report
}

/// What [`raise`] would report for `node`, read as a child of `parent`,
/// without changing it — for a warning given before the write.
pub(crate) fn survey(node: &Node, parent: KmlElement, report: &mut Changes) {
    let Some(element) = node.element() else { return };
    let element = match raised(parent, node) {
        Some(change) => {
            change.record(element, report);
            change.element(element)
        }
        None => element,
    };
    for child in &node.children {
        survey(child, element, report);
    }
}

/// What raising does to one element.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Change {
    /// An adopted `gx:` element, into the KML namespace.
    ToKml,
    /// A deprecated element, as its successor.
    Rename(KmlElement),
}

impl Change {
    fn element(self, element: KmlElement) -> KmlElement {
        match self {
            Change::ToKml => element,
            Change::Rename(to) => to,
        }
    }

    fn record(self, element: KmlElement, report: &mut Changes) {
        match self {
            Change::ToKml => report.respell(element, Spelling::Kml),
            Change::Rename(to) => report.rename(element, to),
        }
    }
}

/// What 2.3 makes of `node` under `parent`, or `None` for no change.
fn raised(parent: KmlElement, node: &Node) -> Option<Change> {
    let Kind::Kml { element, spelling } = node.kind else { return None };
    let admits = |child: KmlElement| children_2_3(parent).contains(&child);
    match (element, spelling) {
        (_, Spelling::Gx) => admits(element).then_some(Change::ToKml),
        (KmlElement::SnippetUpper, _) if admits(KmlElement::Snippet) && snippet_says_all(node) => {
            Some(Change::Rename(KmlElement::Snippet))
        }
        (KmlElement::Metadata, _) if admits(KmlElement::ExtendedData) && is_extended_data(node) => {
            Some(Change::Rename(KmlElement::ExtendedData))
        }
        _ => None,
    }
}

/// Whether `<snippet>`, a plain string, says all a `<Snippet>` does: it has
/// no attribute but a namespace declaration and the default `maxLines`.
fn snippet_says_all(node: &Node) -> bool {
    node.attrs.iter().all(|a| is_decl(a) || (a.ns.is_none() && a.name == "maxLines" && a.value.trim() == "2"))
}

/// Whether a `<Metadata>`'s content is what `<ExtendedData>` holds: `<Data>`,
/// `<SchemaData>`, and other namespaces' elements, with no text and no
/// attribute in no namespace.
fn is_extended_data(node: &Node) -> bool {
    node.text.trim().is_empty()
        && node.attrs.iter().all(|a| a.ns.is_some())
        && node.children.iter().all(|c| match &c.kind {
            Kind::Kml { element, .. } => matches!(element, KmlElement::Data | KmlElement::SchemaData),
            Kind::Foreign(f) => !f.in_kml_namespace,
        })
}

/// Rebind every declaration of Google's pre-OGC namespace to OGC's: the
/// elements keep their prefixes and change namespace.
fn rebind_google(node: &mut Node) {
    for a in &mut node.attrs {
        if is_decl(a) && a.value == GOOGLE_KML_2_2_NS {
            a.value = KML_NS.into();
        }
    }
    for child in &mut node.children {
        rebind_google(child);
    }
}

fn raise_children(node: &mut Node, parent: KmlElement, kml_prefix: Option<&str>, report: &mut Changes) {
    for child in &mut node.children {
        let Some(element) = child.element() else { continue };
        let element = match raised(parent, child) {
            Some(change) => {
                change.record(element, report);
                let to = change.element(element);
                if change == Change::ToKml {
                    child.prefix = kml_prefix.map(str::to_string);
                }
                child.kind = Kind::Kml { element: to, spelling: Spelling::Kml };
                if to == KmlElement::Snippet {
                    child.attrs.retain(is_decl);
                }
                to
            }
            None => element,
        };
        raise_children(child, element, kml_prefix, report);
    }
}

// --- Generated from ogckml23.xsd by tests/fixtures/gen_kml23_children.py; do not edit by hand. ---

/// Every KML element 2.3 admits as a child of `parent`. Empty for an
/// element with no element content of its own.
pub(crate) fn children_2_3(parent: KmlElement) -> &'static [KmlElement] {
    use KmlElement as K;
    match parent {
        K::Alias => &[
            K::TargetHref,
            K::SourceHref,
        ],
        K::AnimatedUpdate => &[
            K::Duration,
            K::Update,
            K::DelayedStart,
        ],
        K::BalloonStyle => &[
            K::BgColor,
            K::Color,
            K::TextColor,
            K::Text,
            K::DisplayMode,
        ],
        K::Camera => &[
            K::TimeStamp,
            K::TimeSpan,
            K::Longitude,
            K::Latitude,
            K::Altitude,
            K::Heading,
            K::Tilt,
            K::Roll,
            K::AltitudeMode,
            K::SeaFloorAltitudeMode,
            K::HorizFov,
        ],
        K::Change => &[
            K::Link,
            K::Url,
            K::Document,
            K::Folder,
            K::GroundOverlay,
            K::ScreenOverlay,
            K::PhotoOverlay,
            K::Placemark,
            K::NetworkLink,
            K::Tour,
            K::LookAt,
            K::Camera,
            K::SchemaData,
            K::Data,
            K::MultiGeometry,
            K::Point,
            K::LineString,
            K::LinearRing,
            K::Polygon,
            K::Model,
            K::Track,
            K::MultiTrack,
            K::Style,
            K::StyleMap,
            K::TimeStamp,
            K::TimeSpan,
            K::Region,
            K::Lod,
            K::Icon,
            K::Location,
            K::Orientation,
            K::ScaleUpper,
            K::ResourceMap,
            K::Alias,
            K::LatLonQuad,
            K::LatLonAltBox,
            K::LatLonBox,
            K::ViewVolume,
            K::ImagePyramid,
            K::Pair,
            K::IconStyle,
            K::LabelStyle,
            K::LineStyle,
            K::PolyStyle,
            K::BalloonStyle,
            K::ListStyle,
            K::ItemIcon,
            K::AnimatedUpdate,
            K::FlyTo,
            K::SoundCue,
            K::TourControl,
            K::Wait,
            K::Playlist,
        ],
        K::Create => &[
            K::Document,
            K::Folder,
            K::MultiTrack,
            K::MultiGeometry,
        ],
        K::Data => &[
            K::DisplayName,
            K::Value,
        ],
        K::Delete => &[
            K::Document,
            K::Folder,
            K::GroundOverlay,
            K::ScreenOverlay,
            K::PhotoOverlay,
            K::Placemark,
            K::NetworkLink,
            K::Tour,
            K::MultiGeometry,
            K::Point,
            K::LineString,
            K::LinearRing,
            K::Polygon,
            K::Model,
            K::Track,
            K::MultiTrack,
        ],
        K::Document => &[
            K::Name,
            K::Visibility,
            K::BalloonVisibility,
            K::Open,
            K::Address,
            K::PhoneNumber,
            K::Snippet,
            K::SnippetUpper,
            K::Description,
            K::LookAt,
            K::Camera,
            K::TimeStamp,
            K::TimeSpan,
            K::StyleUrl,
            K::Style,
            K::StyleMap,
            K::Region,
            K::Metadata,
            K::ExtendedData,
            K::Schema,
            K::Document,
            K::Folder,
            K::GroundOverlay,
            K::ScreenOverlay,
            K::PhotoOverlay,
            K::Placemark,
            K::NetworkLink,
            K::Tour,
        ],
        K::ExtendedData => &[
            K::Data,
            K::SchemaData,
        ],
        K::FlyTo => &[
            K::Duration,
            K::FlyToMode,
            K::LookAt,
            K::Camera,
        ],
        K::Folder => &[
            K::Name,
            K::Visibility,
            K::BalloonVisibility,
            K::Open,
            K::Address,
            K::PhoneNumber,
            K::Snippet,
            K::SnippetUpper,
            K::Description,
            K::LookAt,
            K::Camera,
            K::TimeStamp,
            K::TimeSpan,
            K::StyleUrl,
            K::Style,
            K::StyleMap,
            K::Region,
            K::Metadata,
            K::ExtendedData,
            K::Document,
            K::Folder,
            K::GroundOverlay,
            K::ScreenOverlay,
            K::PhotoOverlay,
            K::Placemark,
            K::NetworkLink,
            K::Tour,
        ],
        K::GroundOverlay => &[
            K::Name,
            K::Visibility,
            K::BalloonVisibility,
            K::Open,
            K::Address,
            K::PhoneNumber,
            K::Snippet,
            K::SnippetUpper,
            K::Description,
            K::LookAt,
            K::Camera,
            K::TimeStamp,
            K::TimeSpan,
            K::StyleUrl,
            K::Style,
            K::StyleMap,
            K::Region,
            K::Metadata,
            K::ExtendedData,
            K::Color,
            K::DrawOrder,
            K::Icon,
            K::Altitude,
            K::AltitudeMode,
            K::SeaFloorAltitudeMode,
            K::LatLonQuad,
            K::LatLonAltBox,
            K::LatLonBox,
        ],
        K::Icon => &[
            K::Href,
            K::RefreshMode,
            K::RefreshInterval,
            K::ViewRefreshMode,
            K::ViewRefreshTime,
            K::ViewBoundScale,
            K::ViewFormat,
            K::HttpQuery,
        ],
        K::IconStyle => &[
            K::Color,
            K::ColorMode,
            K::Scale,
            K::Heading,
            K::Icon,
            K::HotSpot,
        ],
        K::ImagePyramid => &[
            K::TileSize,
            K::MaxWidth,
            K::MaxHeight,
            K::GridOrigin,
        ],
        K::InnerBoundaryIs => &[
            K::LinearRing,
        ],
        K::ItemIcon => &[
            K::State,
            K::Href,
        ],
        K::Kml => &[
            K::NetworkLinkControl,
            K::Document,
            K::Folder,
            K::GroundOverlay,
            K::ScreenOverlay,
            K::PhotoOverlay,
            K::Placemark,
            K::NetworkLink,
            K::Tour,
        ],
        K::LabelStyle => &[
            K::Color,
            K::ColorMode,
            K::Scale,
        ],
        K::LatLonAltBox => &[
            K::North,
            K::South,
            K::East,
            K::West,
            K::MinAltitude,
            K::MaxAltitude,
            K::AltitudeMode,
            K::SeaFloorAltitudeMode,
        ],
        K::LatLonBox => &[
            K::North,
            K::South,
            K::East,
            K::West,
            K::Rotation,
        ],
        K::LatLonQuad => &[
            K::Coordinates,
        ],
        K::LineString => &[
            K::Extrude,
            K::Tessellate,
            K::AltitudeMode,
            K::SeaFloorAltitudeMode,
            K::Coordinates,
            K::AltitudeOffset,
        ],
        K::LineStyle => &[
            K::Color,
            K::ColorMode,
            K::Width,
        ],
        K::LinearRing => &[
            K::Extrude,
            K::Tessellate,
            K::AltitudeMode,
            K::SeaFloorAltitudeMode,
            K::Coordinates,
            K::AltitudeOffset,
        ],
        K::Link => &[
            K::Href,
            K::RefreshMode,
            K::RefreshInterval,
            K::ViewRefreshMode,
            K::ViewRefreshTime,
            K::ViewBoundScale,
            K::ViewFormat,
            K::HttpQuery,
        ],
        K::ListStyle => &[
            K::ListItemType,
            K::BgColor,
            K::ItemIcon,
            K::MaxSnippetLines,
        ],
        K::Location => &[
            K::Longitude,
            K::Latitude,
            K::Altitude,
        ],
        K::Lod => &[
            K::MinLodPixels,
            K::MaxLodPixels,
            K::MinFadeExtent,
            K::MaxFadeExtent,
        ],
        K::LookAt => &[
            K::TimeStamp,
            K::TimeSpan,
            K::Longitude,
            K::Latitude,
            K::Altitude,
            K::Heading,
            K::Tilt,
            K::Range,
            K::AltitudeMode,
            K::SeaFloorAltitudeMode,
            K::HorizFov,
        ],
        K::Model => &[
            K::AltitudeMode,
            K::SeaFloorAltitudeMode,
            K::Location,
            K::Orientation,
            K::ScaleUpper,
            K::Link,
            K::ResourceMap,
        ],
        K::MultiGeometry => &[
            K::MultiGeometry,
            K::Point,
            K::LineString,
            K::LinearRing,
            K::Polygon,
            K::Model,
            K::Track,
            K::MultiTrack,
        ],
        K::MultiTrack => &[
            K::AltitudeMode,
            K::SeaFloorAltitudeMode,
            K::Interpolate,
            K::Track,
        ],
        K::NetworkLink => &[
            K::Name,
            K::Visibility,
            K::BalloonVisibility,
            K::Open,
            K::Address,
            K::PhoneNumber,
            K::Snippet,
            K::SnippetUpper,
            K::Description,
            K::LookAt,
            K::Camera,
            K::TimeStamp,
            K::TimeSpan,
            K::StyleUrl,
            K::Style,
            K::StyleMap,
            K::Region,
            K::Metadata,
            K::ExtendedData,
            K::RefreshVisibility,
            K::FlyToView,
            K::Link,
            K::Url,
        ],
        K::NetworkLinkControl => &[
            K::MinRefreshPeriod,
            K::MaxSessionLength,
            K::Cookie,
            K::Message,
            K::LinkName,
            K::LinkDescription,
            K::LinkSnippet,
            K::Expires,
            K::Update,
            K::LookAt,
            K::Camera,
        ],
        K::Orientation => &[
            K::Heading,
            K::Tilt,
            K::Roll,
        ],
        K::OuterBoundaryIs => &[
            K::LinearRing,
        ],
        K::Pair => &[
            K::Key,
            K::StyleUrl,
            K::Style,
            K::StyleMap,
        ],
        K::PhotoOverlay => &[
            K::Name,
            K::Visibility,
            K::BalloonVisibility,
            K::Open,
            K::Address,
            K::PhoneNumber,
            K::Snippet,
            K::SnippetUpper,
            K::Description,
            K::LookAt,
            K::Camera,
            K::TimeStamp,
            K::TimeSpan,
            K::StyleUrl,
            K::Style,
            K::StyleMap,
            K::Region,
            K::Metadata,
            K::ExtendedData,
            K::Color,
            K::DrawOrder,
            K::Icon,
            K::Rotation,
            K::ViewVolume,
            K::ImagePyramid,
            K::Point,
            K::Shape,
        ],
        K::Placemark => &[
            K::Name,
            K::Visibility,
            K::BalloonVisibility,
            K::Open,
            K::Address,
            K::PhoneNumber,
            K::Snippet,
            K::SnippetUpper,
            K::Description,
            K::LookAt,
            K::Camera,
            K::TimeStamp,
            K::TimeSpan,
            K::StyleUrl,
            K::Style,
            K::StyleMap,
            K::Region,
            K::Metadata,
            K::ExtendedData,
            K::MultiGeometry,
            K::Point,
            K::LineString,
            K::LinearRing,
            K::Polygon,
            K::Model,
            K::Track,
            K::MultiTrack,
        ],
        K::Playlist => &[
            K::AnimatedUpdate,
            K::FlyTo,
            K::SoundCue,
            K::TourControl,
            K::Wait,
        ],
        K::Point => &[
            K::Extrude,
            K::AltitudeMode,
            K::SeaFloorAltitudeMode,
            K::Coordinates,
        ],
        K::PolyStyle => &[
            K::Color,
            K::ColorMode,
            K::Fill,
            K::Outline,
        ],
        K::Polygon => &[
            K::Extrude,
            K::Tessellate,
            K::AltitudeMode,
            K::SeaFloorAltitudeMode,
            K::OuterBoundaryIs,
            K::InnerBoundaryIs,
        ],
        K::Region => &[
            K::LatLonQuad,
            K::LatLonAltBox,
            K::LatLonBox,
            K::Lod,
        ],
        K::ResourceMap => &[
            K::Alias,
        ],
        K::ScaleUpper => &[
            K::X,
            K::Y,
            K::Z,
        ],
        K::Schema => &[
            K::SimpleField,
            K::SimpleArrayField,
        ],
        K::SchemaData => &[
            K::SimpleData,
            K::SimpleArrayData,
        ],
        K::ScreenOverlay => &[
            K::Name,
            K::Visibility,
            K::BalloonVisibility,
            K::Open,
            K::Address,
            K::PhoneNumber,
            K::Snippet,
            K::SnippetUpper,
            K::Description,
            K::LookAt,
            K::Camera,
            K::TimeStamp,
            K::TimeSpan,
            K::StyleUrl,
            K::Style,
            K::StyleMap,
            K::Region,
            K::Metadata,
            K::ExtendedData,
            K::Color,
            K::DrawOrder,
            K::Icon,
            K::OverlayXY,
            K::ScreenXY,
            K::RotationXY,
            K::Size,
            K::Rotation,
        ],
        K::SimpleArrayData => &[
            K::Value,
        ],
        K::SimpleArrayField => &[
            K::DisplayName,
        ],
        K::SimpleField => &[
            K::DisplayName,
        ],
        K::SoundCue => &[
            K::Href,
            K::DelayedStart,
        ],
        K::Style => &[
            K::IconStyle,
            K::LabelStyle,
            K::LineStyle,
            K::PolyStyle,
            K::BalloonStyle,
            K::ListStyle,
        ],
        K::StyleMap => &[
            K::Pair,
        ],
        K::TimeSpan => &[
            K::Begin,
            K::End,
        ],
        K::TimeStamp => &[
            K::When,
        ],
        K::Tour => &[
            K::Name,
            K::Visibility,
            K::BalloonVisibility,
            K::Open,
            K::Address,
            K::PhoneNumber,
            K::Snippet,
            K::SnippetUpper,
            K::Description,
            K::LookAt,
            K::Camera,
            K::TimeStamp,
            K::TimeSpan,
            K::StyleUrl,
            K::Style,
            K::StyleMap,
            K::Region,
            K::Metadata,
            K::ExtendedData,
            K::Playlist,
        ],
        K::TourControl => &[
            K::PlayMode,
        ],
        K::Track => &[
            K::Extrude,
            K::Tessellate,
            K::AltitudeMode,
            K::SeaFloorAltitudeMode,
            K::When,
            K::Coord,
            K::Angles,
            K::Model,
            K::ExtendedData,
        ],
        K::Update => &[
            K::TargetHref,
            K::Create,
            K::Delete,
            K::Change,
        ],
        K::Url => &[
            K::Href,
            K::RefreshMode,
            K::RefreshInterval,
            K::ViewRefreshMode,
            K::ViewRefreshTime,
            K::ViewBoundScale,
            K::ViewFormat,
            K::HttpQuery,
        ],
        K::ViewVolume => &[
            K::LeftFov,
            K::RightFov,
            K::BottomFov,
            K::TopFov,
            K::Near,
        ],
        K::Wait => &[
            K::Duration,
        ],
        _ => &[],
    }
}
// --- End of generated code. ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::KmlVersion;
    use crate::kml::lower::order_2_2;
    use crate::kml::model;

    const NS: &str = r#"xmlns="http://www.opengis.net/kml/2.2" xmlns:gx="http://www.google.com/kml/ext/2.2""#;

    fn raised_doc(source: &str) -> (String, Changes) {
        let mut doc = model::parse(source).unwrap();
        let report = raise(&mut doc);
        let out = model::write(&doc);
        (out[out.find("?>\n").unwrap() + 3..].to_string(), report)
    }

    #[test]
    fn adopted_elements_go_into_the_kml_namespace() {
        let (out, report) = raised_doc(&format!(
            "<kml {NS}><Placemark><LookAt><gx:TimeStamp><when>v</when></gx:TimeStamp></LookAt>\
             <gx:Track><when>a</when><gx:coord>1 2 3</gx:coord></gx:Track></Placemark></kml>"
        ));
        assert_eq!(
            out,
            r#"<kml xmlns="http://www.opengis.net/kml/2.2" xmlns:gx="http://www.google.com/kml/ext/2.2" version="2.3"><Placemark><LookAt><TimeStamp><when>v</when></TimeStamp></LookAt><Track><when>a</when><coord>1 2 3</coord></Track></Placemark></kml>"#
        );
        assert_eq!(
            report.respelled_warning(KmlVersion::V2_2, KmlVersion::V2_3).unwrap(),
            "kml: input is KML 2.2; written as KML 2.3 (gx:TimeStamp→TimeStamp, gx:Track→Track, gx:coord→coord)"
        );
    }

    #[test]
    fn a_sea_floor_mode_is_renamed_with_its_namespace() {
        let (out, _) = raised_doc(&format!(
            "<kml {NS}><Placemark><Point><gx:altitudeMode>clampToSeaFloor</gx:altitudeMode><coordinates>1,2</coordinates></Point></Placemark></kml>"
        ));
        assert!(out.contains("<Point><seaFloorAltitudeMode>clampToSeaFloor</seaFloorAltitudeMode>"), "{out}");
    }

    #[test]
    fn an_element_2_3_does_not_admit_there_stays_gx() {
        // The gx: schema admits altitudeOffset on any geometry; 2.3 only on a
        // line string or ring.
        let (out, report) = raised_doc(&format!(
            "<kml {NS}><Placemark><Point><gx:altitudeOffset>5</gx:altitudeOffset><coordinates>1,2</coordinates></Point></Placemark>\
             <Placemark><LineString><gx:altitudeOffset>5</gx:altitudeOffset><coordinates>1,2 3,4</coordinates></LineString></Placemark></kml>"
        ));
        assert!(out.contains("<Point><gx:altitudeOffset>5</gx:altitudeOffset>"), "{out}");
        assert!(out.contains("<LineString><altitudeOffset>5</altitudeOffset>"), "{out}");
        assert_eq!(report.respelled, [(KmlElement::AltitudeOffset, Spelling::Kml, 1)]);
    }

    #[test]
    fn snippet_and_metadata_take_their_successors_only_when_nothing_is_lost() {
        let (out, report) = raised_doc(&format!(
            "<kml {NS} xmlns:x=\"urn:x\"><Placemark><Snippet maxLines=\"2\">s</Snippet><Metadata><x:a/></Metadata></Placemark>\
             <Placemark><Snippet maxLines=\"5\">t</Snippet><Metadata><name>kml inside</name></Metadata></Placemark></kml>"
        ));
        assert!(out.contains("<Placemark><snippet>s</snippet><ExtendedData><x:a/></ExtendedData></Placemark>"), "{out}");
        assert!(out.contains(r#"<Snippet maxLines="5">t</Snippet><Metadata><name>kml inside</name></Metadata>"#), "{out}");
        assert_eq!(
            report.respelled_warning(KmlVersion::V2_2, KmlVersion::V2_3).unwrap(),
            "kml: input is KML 2.2; written as KML 2.3 (Snippet→snippet, Metadata→ExtendedData)"
        );
    }

    #[test]
    fn the_root_takes_ogcs_namespace_and_the_stamp() {
        for (source, root) in [
            ("<kml><Document/></kml>", r#"<kml xmlns="http://www.opengis.net/kml/2.2" version="2.3">"#),
            (r#"<kml xmlns="http://earth.google.com/kml/2.2"><Document/></kml>"#, r#"<kml xmlns="http://www.opengis.net/kml/2.2" version="2.3">"#),
            (
                r#"<k:kml xmlns:k="http://earth.google.com/kml/2.2" version="2.2"><k:Document/></k:kml>"#,
                r#"<k:kml xmlns:k="http://www.opengis.net/kml/2.2" version="2.3">"#,
            ),
        ] {
            let (out, report) = raised_doc(source);
            assert!(out.starts_with(root), "{source}: {out}");
            assert_eq!(report, Changes::default(), "a stamp and a namespace alone are not reported");
        }
    }

    #[test]
    fn a_survey_reports_what_the_raising_does() {
        let source = format!(
            "<kml {NS}><Placemark><Snippet>s</Snippet><gx:Track><gx:coord>1</gx:coord></gx:Track>\
             <Point><gx:altitudeOffset>1</gx:altitudeOffset></Point></Placemark></kml>"
        );
        let doc = model::parse(&source).unwrap();
        let mut surveyed = Changes::default();
        for child in &doc.root.children {
            survey(child, KmlElement::Kml, &mut surveyed);
        }
        assert_eq!(raised_doc(&source).1, surveyed);
    }

    #[test]
    fn every_2_2_placement_is_a_2_3_placement() {
        // What a raise never has to drop: every child 2.2 admits in the KML
        // namespace, or as an adopted gx: element, 2.3 admits as KML — save
        // altitudeOffset, which the gx: schema puts on every geometry and
        // 2.3 on line strings and rings only, and which stays gx:.
        for parent in model::vocabulary() {
            for spelling in [Spelling::Kml, Spelling::Gx] {
                for &(child, _, _) in order_2_2(parent, spelling) {
                    if child == KmlElement::AltitudeOffset {
                        continue;
                    }
                    assert!(
                        children_2_3(parent).contains(&child),
                        "2.2 admits <{}> in <{}> and 2.3 does not",
                        child.name(),
                        parent.name()
                    );
                }
            }
        }
    }
}
