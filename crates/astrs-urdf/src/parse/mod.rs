//! Builds a [`crate::model::Robot`] from URDF XML text, on top of
//! [`crate::xml::Reader`].
//!
//! # Shape
//!
//! One `parse_*` function per URDF element this crate models
//! (`<robot>`/`<link>`/`<joint>`/`<geometry>`/`<material>`/`<origin>`,
//! split one-per-file below), each sharing one contract: called with the
//! reader positioned just past that element's own `StartElement`, returns
//! having consumed through that element's own matching `EndElement` — the
//! crate-private `cursor` module's own docs describe this in full, and its
//! `consume_element_body` is the shared primitive every `parse_*` function
//! is built from. [`parse_str`] is the only `pub` entry point — it owns
//! the outermost XML-declaration/`<robot>`-dispatch loop and is where an
//! [`crate::xml::XmlError`] first becomes a [`crate::UrdfError::Xml`].
//!
//! # What parsing does and does not check
//!
//! This module's job stops at "the document parses into a well-formed
//! [`crate::model::Robot`] value" — every element and attribute this crate
//! models means something, and every value that has to be a number parses
//! as one. It never checks that a `<joint>`'s `parent`/`child` names a link
//! that actually exists, or that the resulting graph is a tree — that is
//! [`crate::model::Robot::validate`]'s job, deliberately kept separate
//! (blueprint §5.3 lists it as its own concern), and a caller that wants
//! the guarantee always calls it explicitly after [`parse_str`] returns
//! (this crate does not run it automatically, so a caller inspecting a
//! possibly-invalid document — a linter, an editor's live-diagnostics pass
//! — is not forced to fail fast on the first cross-reference problem
//! before ever seeing the parsed structure).

mod attrs;
mod cursor;
mod geometry;
mod joint;
mod link;
mod material;
mod origin;
mod robot;

pub use robot::parse_str;

#[cfg(test)]
mod tests;
