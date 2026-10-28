//! The raised document divided between the collection and its features
//! (`plans/kml-standard.org` D-K4, K4).
//!
//! *Per-feature content travels with its feature.* Each projected feature
//! element leaves the document tree and becomes its feature's native, holding
//! its own node and its [`Address`]: the container it sat in and its place
//! among that container's other children. The rest of the document — every
//! container, style, schema, network link, the root and its declarations —
//! stays behind as the collection's [`Skeleton`]. So `--sort-hilbert`, an
//! edit, a removal or an addition moves features without detaching any of
//! them from its folder, which a skeleton holding numbered placeholders could
//! not survive (D-K4's rejected design).
//!
//! [`split`] divides; [`assemble`] puts features back, in the order the IR
//! holds them within each place. Comments that sat immediately before a
//! feature travel with it, so they are written back before it.

use crate::kml::model::{KmlDocument, KmlElement, Node};
use crate::xml::XmlMisc;

/// The document with its projected features taken out.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Skeleton {
    pub doc: KmlDocument,
}

/// Where a feature sat: the path of child indices from the root to its
/// container, through the skeleton, and how many of that container's
/// remaining children preceded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Address {
    pub container: Vec<usize>,
    pub slot: usize,
}

/// A projected feature's own part of the document.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FeatureModel {
    pub node: Node,
    pub address: Address,
    /// Comments and processing instructions immediately before it.
    pub leading: Vec<XmlMisc>,
}

/// One of a layered `.kmz`'s other documents (`plans/same-format-fidelity.org`
/// M3c): which archive entry it is, the version it was read as, and its
/// skeleton. The archive's first document is the collection's own
/// (`KmlNative`'s `version` and `document`); these are the rest, in archive
/// order, numbered from 1 in each feature's `document`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Layer {
    pub entry: usize,
    pub version: crate::format::KmlVersion,
    pub skeleton: std::rc::Rc<Skeleton>,
}

/// A feature to put back: where it was read from (`None` for one added since),
/// the comments before it, and its element.
pub(crate) type Placed = (Option<Address>, Vec<XmlMisc>, Node);

/// A feature bound for one container: its slot there, the comments before it,
/// and its element.
type Slotted = (usize, Vec<XmlMisc>, Node);

/// Whether a child of a container is a feature the IR projects: a Placemark,
/// or an overlay with geography (D-K14).
pub(crate) fn is_projected(node: &Node) -> bool {
    node.element().is_some_and(crate::kml::project::is_projected)
}

/// Take the projected features out of `doc`, in document order. Containers are
/// the root `<kml>`, `<Document>` and `<Folder>`; a feature anywhere else —
/// inside a `<NetworkLinkControl>`'s `<Update>`, say, where it is an
/// instruction to another document rather than a feature of this one — stays
/// in the skeleton.
pub(crate) fn split(mut doc: KmlDocument) -> (Skeleton, Vec<FeatureModel>) {
    let mut features = Vec::new();
    split_container(&mut doc.root, &mut Vec::new(), &mut features);
    (Skeleton { doc }, features)
}

fn split_container(container: &mut Node, path: &mut Vec<usize>, out: &mut Vec<FeatureModel>) {
    let children = std::mem::take(&mut container.children);
    let mut misc = std::mem::take(&mut container.misc).into_iter().peekable();
    let len = children.len();
    let mut kept = Vec::with_capacity(len);
    let mut kept_misc = Vec::new();
    let mut children = children.into_iter();
    for i in 0..=len {
        let mut before = Vec::new();
        while let Some((_, m)) = misc.next_if(|(at, _)| *at <= i) {
            before.push(m);
        }
        let Some(mut child) = children.next() else {
            kept_misc.extend(before.into_iter().map(|m| (kept.len(), m)));
            break;
        };
        if is_projected(&child) {
            out.push(FeatureModel {
                node: child,
                address: Address { container: path.clone(), slot: kept.len() },
                leading: before,
            });
            continue;
        }
        kept_misc.extend(before.into_iter().map(|m| (kept.len(), m)));
        if child.element().is_some_and(KmlElement::is_container) {
            path.push(kept.len());
            split_container(&mut child, path, out);
            path.pop();
        }
        kept.push(child);
    }
    // Sized for every child the container had; most of them were features.
    kept.shrink_to_fit();
    container.children = kept;
    container.misc = kept_misc;
}

/// The skeleton with `features` put back. Each feature goes to its address
/// when the skeleton still has that container, and to the end of the
/// document's first container otherwise — which is also where a feature with
/// no address (one added since the read) goes. Features sharing a place are
/// written in the order given.
pub(crate) fn assemble(skeleton: &Skeleton, features: Vec<Placed>) -> KmlDocument {
    let mut doc = skeleton.doc.clone();
    let default = default_container(&doc.root);
    // Grouped per container, keeping the order given within each.
    let mut placed: Vec<(Vec<usize>, Vec<Slotted>)> = Vec::new();
    for (address, leading, node) in features {
        let (path, slot) = match address.filter(|a| container_at(&doc.root, &a.container).is_some()) {
            Some(a) => (a.container, a.slot),
            None => (default.clone(), usize::MAX),
        };
        match placed.iter_mut().find(|(p, _)| *p == path) {
            Some((_, list)) => list.push((slot, leading, node)),
            None => placed.push((path, vec![(slot, leading, node)])),
        }
    }
    // Deepest first: a path indexes the skeleton, and putting features into a
    // container shifts its children's indices, so every container below it
    // must be filled while its path is still true.
    placed.sort_by_key(|(path, _)| std::cmp::Reverse(path.len()));
    for (path, list) in placed {
        let container = container_at_mut(&mut doc.root, &path).expect("checked above");
        insert(container, list);
    }
    doc
}

/// Where a feature with nowhere else to go is written: the root's first
/// container child (its `<Document>` or `<Folder>`), or the root itself.
fn default_container(root: &Node) -> Vec<usize> {
    match root.children.iter().position(|c| c.element().is_some_and(KmlElement::is_container)) {
        Some(i) => vec![i],
        None => Vec::new(),
    }
}

fn container_at<'a>(root: &'a Node, path: &[usize]) -> Option<&'a Node> {
    let mut node = root;
    for &i in path {
        node = node.children.get(i)?;
    }
    (path.is_empty() || node.element().is_some_and(KmlElement::is_container)).then_some(node)
}

fn container_at_mut<'a>(root: &'a mut Node, path: &[usize]) -> Option<&'a mut Node> {
    let mut node = root;
    for &i in path {
        node = node.children.get_mut(i)?;
    }
    Some(node)
}

/// Interleave `features` into `container`'s children: each before the kept
/// child its slot names, those past the end after the last.
fn insert(container: &mut Node, mut features: Vec<Slotted>) {
    // A stable sort keeps the given order among features sharing a slot.
    features.sort_by_key(|(slot, _, _)| *slot);
    let kept = std::mem::take(&mut container.children);
    let kept_misc = std::mem::take(&mut container.misc);
    let mut features = features.into_iter().peekable();
    let mut misc = kept_misc.into_iter().peekable();
    let len = kept.len();
    let mut kept = kept.into_iter();
    for k in 0..=len {
        // Features first: a comment just before a feature travels in its
        // `leading`, so the container's own comments at this slot came after
        // any feature that sat here.
        while let Some((_, leading, node)) = features.next_if(|(slot, _, _)| *slot <= k || k == len) {
            container.misc.extend(leading.into_iter().map(|m| (container.children.len(), m)));
            container.children.push(node);
        }
        while let Some((_, m)) = misc.next_if(|(at, _)| *at <= k) {
            container.misc.push((container.children.len(), m));
        }
        if let Some(child) = kept.next() {
            container.children.push(child);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kml::model;
    use crate::xml;

    fn split_source(source: &str) -> (Skeleton, Vec<FeatureModel>) {
        split(model::parse(source).unwrap())
    }

    fn names(features: &[FeatureModel]) -> Vec<&str> {
        features.iter().map(|f| f.node.child(KmlElement::Name).unwrap().text_trimmed()).collect()
    }

    fn put_back(skeleton: &Skeleton, features: Vec<FeatureModel>) -> String {
        model::write(&assemble(skeleton, features.into_iter().map(|f| (Some(f.address), f.leading, f.node)).collect()))
    }

    const NESTED: &str = "<kml><Document><name>d</name><Style id='s'/><!--before a-->\
        <Placemark><name>a</name></Placemark><Folder><name>f</name><Placemark><name>b</name></Placemark>\
        <NetworkLink/><Placemark><name>c</name></Placemark></Folder><Placemark><name>e</name></Placemark>\
        <!--end--></Document></kml>";

    #[test]
    fn features_leave_the_tree_with_their_addresses() {
        let (skeleton, features) = split_source(NESTED);
        assert_eq!(names(&features), ["a", "b", "c", "e"]);
        let addresses: Vec<_> = features.iter().map(|f| (f.address.container.clone(), f.address.slot)).collect();
        // Document is root child 0; its kept children are name, Style, Folder.
        assert_eq!(addresses, [(vec![0], 2), (vec![0, 2], 1), (vec![0, 2], 2), (vec![0], 3)]);
        assert_eq!(features[0].leading, vec![XmlMisc::Comment("before a".into())]);
        let document = &skeleton.doc.root.children[0];
        assert!(document.children.iter().all(|c| !is_projected(c)));
    }

    #[test]
    fn putting_every_feature_back_gives_back_the_document() {
        let (skeleton, features) = split_source(NESTED);
        let written = put_back(&skeleton, features);
        assert_eq!(xml::parse_document(&written).unwrap(), xml::parse_document(NESTED).unwrap(), "{written}");
    }

    #[test]
    fn reordered_features_stay_in_their_folders_and_reorder_only_among_neighbours() {
        let source = "<kml><Document><Placemark><name>a</name></Placemark><Placemark><name>b</name></Placemark>\
            <Folder><Placemark><name>c</name></Placemark><NetworkLink/><Placemark><name>d</name></Placemark>\
            </Folder></Document></kml>";
        let (skeleton, mut features) = split_source(source);
        features.reverse();
        let (_, again) = split_source(&put_back(&skeleton, features));
        // a and b share a place, so the new order holds between them. c and d
        // are either side of a network link: reordering the IR does not carry
        // a feature past content the IR does not hold. Nothing changed folder.
        assert_eq!(names(&again), ["b", "a", "c", "d"]);
        assert_eq!(again[2].address.container, vec![0, 0]);
    }

    #[test]
    fn a_feature_with_no_address_goes_to_the_end_of_the_first_container() {
        let (skeleton, features) = split_source(NESTED);
        let mut list: Vec<_> = features.into_iter().map(|f| (Some(f.address), f.leading, f.node)).collect();
        let (_, extra) = split_source("<kml><Placemark><name>new</name></Placemark></kml>");
        list.push((None, Vec::new(), extra.into_iter().next().unwrap().node));
        let (_, again) = split_source(&model::write(&assemble(&skeleton, list)));
        assert_eq!(names(&again), ["a", "b", "c", "e", "new"]);
        assert_eq!(again[4].address.container, vec![0]);
    }

    #[test]
    fn a_placemark_inside_an_update_is_not_a_feature_of_this_document() {
        let (_, features) = split_source(
            "<kml><NetworkLinkControl><Update><Create><Document targetId='x'>\
             <Placemark><name>instruction</name></Placemark></Document></Create></Update></NetworkLinkControl></kml>",
        );
        assert!(features.is_empty());
    }

    #[test]
    fn a_placemark_directly_under_the_root_round_trips() {
        let source = "<kml><Placemark><name>only</name></Placemark></kml>";
        let (skeleton, features) = split_source(source);
        assert_eq!(features[0].address, Address { container: vec![], slot: 0 });
        assert_eq!(xml::parse_document(&put_back(&skeleton, features)).unwrap(), xml::parse_document(source).unwrap());
    }
}
