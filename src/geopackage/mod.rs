//! GeoPackage (GPKG) reader, built on the from-scratch `crate::sqlite` reader.
//!
//! A `.gpkg` is a SQLite database that can hold several feature tables
//! ("layers"). Reading fans out over all of them: [`read_layers`] returns each
//! layer's name and [`crate::feature::FeatureCollection`]. Geometry is stored as
//! GeoPackage Binary — a small header wrapping standard WKB, which we already
//! decode. See `plans/geopackage.org`.

pub mod carry;
mod container;
mod reader;
mod reconcile;
mod rtree;
mod writer;

pub use container::container_drop_warnings;
/// The container carrying channel — independent content read out of a source
/// GeoPackage so a `gpkg -> gpkg` write can put it back. Crate-visible: this is
/// the write path's own plumbing, not part of the format-agnostic surface.
pub use carry::{Carried, read as read_container};
pub use reader::read_layers;
/// Merging a destination container's carried content with a source's, for the
/// append path — `plans/container-reconciliation.org` § 3.
pub use reconcile::reconcile as reconcile_containers;
pub use writer::{metadata_drop_warnings, write_layers};
/// The sentinel `gpkg_contents.last_change` written where a layer carried none.
/// Crate-visible so the reader can recognise its own output and drop it rather
/// than carrying it back as if it were content — see [`writer::LAST_CHANGE`].
pub(crate) use writer::LAST_CHANGE;
