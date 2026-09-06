//! [`ExpandError`]: everything [`super::expand`] can fail with.

use std::path::PathBuf;

use crate::ManifestError;

/// An error raised while flattening a manifest's `module:`-sourced nodes
/// (blueprint §8.5).
///
/// Unlike [`crate::ValidationErrors`] (which collects every structural
/// problem in one pass), [`super::expand`] fails on the *first* problem it
/// finds — expansion is a structural transformation with a well-defined
/// next step at each recursion point, not an independent-checks pass, so
/// there is no meaningful "keep going and collect more" here (a
/// [`ExpandError::Cycle`] or [`ExpandError::DepthExceeded`] would corrupt
/// any further recursion anyway). This mirrors [`ManifestError`]'s own
/// fail-fast shape, not [`crate::ValidationErrors`]'s collect-everything
/// one.
///
/// Most variants report the *including* node's id (`host_node`) rather
/// than a document path, since expansion may be several `module:` levels
/// deep by the time it fails — a `nodes[3]`-style path from the
/// originally-parsed root manifest would not even name the right file. The
/// two variants that can fire with no host node at all (a stray
/// `_mod/<port>` or a plain duplicate id in the root manifest itself) use
/// a pre-formatted `location` string instead.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExpandError {
    /// Two nodes would share the same id after flattening — either two
    /// nodes in the same not-yet-expanded manifest already collide (this
    /// manifest was never [validated](crate::Manifest::validate)), or
    /// prefixing produced a collision between an ordinary node id and a
    /// module-expansion-generated one (see [`super::expand`]'s module
    /// docs for exactly when this can happen with dotted ids).
    #[error("duplicate node id `{id}` in {location}")]
    DuplicateNodeId {
        /// Where the collision was found: `"the root manifest"` or a
        /// string naming the offending module manifest's resolved path.
        location: String,
        /// The repeated id.
        id: String,
    },

    /// A `module:`-referenced manifest file could not be read.
    #[error("failed to read module manifest `{}`: {source}", path.display())]
    Io {
        /// The resolved path that failed to load.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A `module:`-referenced manifest file was read but failed to parse.
    #[error("failed to parse module manifest `{}`: {source}", path.display())]
    Parse {
        /// The resolved path that failed to parse.
        path: PathBuf,
        /// The underlying parse error.
        #[source]
        source: ManifestError,
    },

    /// A `module:`-referenced manifest file parsed fine but carries no
    /// [`crate::ModuleHeader`] (`module:` root field) — it cannot be
    /// included as a module.
    #[error(
        "module manifest `{}` has no `module:` header, so it cannot be included as a module",
        path.display()
    )]
    NotAModule {
        /// The manifest file that is missing its `module:` header.
        path: PathBuf,
    },

    /// Expanding `module:` references would revisit a manifest file
    /// already being expanded — a direct or indirect include cycle.
    #[error(
        "module inclusion cycle detected: {}",
        chain.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(" -> ")
    )]
    Cycle {
        /// The full ancestor chain, oldest first, ending with the path
        /// that would re-enter the cycle.
        chain: Vec<PathBuf>,
    },

    /// Module inclusion nested deeper than [`super::ExpandOptions::max_depth`]
    /// — a guard against runaway (though not necessarily cyclic) nesting
    /// that [`ExpandError::Cycle`] does not catch (e.g. many distinct
    /// modules chained end to end).
    #[error(
        "module inclusion depth exceeded the limit of {max_depth} (pass a larger \
         `ExpandOptions::max_depth` if this nesting is intentional)"
    )]
    DepthExceeded {
        /// The configured limit that was hit.
        max_depth: usize,
    },

    /// `host_node.inputs` supplies a value for a port the included
    /// module's header does not declare — most often a typo.
    #[error(
        "node `{host_node}` supplies an input for `{port}`, but the module it includes \
         declares no such boundary input"
    )]
    UnknownModuleInput {
        /// The id of the node whose `module:` field triggered this include.
        host_node: String,
        /// The offending `inputs` map key.
        port: String,
    },

    /// `host_node.outputs` claims to expose a port the included module's
    /// header does not declare.
    #[error(
        "node `{host_node}` declares output `{port}`, but the module it includes declares no \
         such boundary output"
    )]
    UnknownModuleOutput {
        /// The id of the node whose `module:` field triggered this include.
        host_node: String,
        /// The offending `outputs` entry.
        port: String,
    },

    /// An internal node references `_mod/<port>`, `<port>` is a boundary
    /// input the module legitimately declares, but nothing in the
    /// including scope ever supplies a value for it.
    ///
    /// Uses `location` rather than a `host_node` id because this can also
    /// fire for the root manifest itself (`_mod/<port>` used outside any
    /// module at all has, by definition, no including host node) — see
    /// [`ExpandError::DuplicateNodeId`] for the same reasoning.
    #[error(
        "{location} uses its own module boundary input `{port}` internally (via \
         `_mod/{port}`), but nothing supplies a value for it"
    )]
    UnsuppliedModuleInput {
        /// Where the unsupplied reference was found: `"the root
        /// manifest"` or a string naming the offending module manifest's
        /// resolved path.
        location: String,
        /// The unsupplied boundary input port name.
        port: String,
    },

    /// `host_node.outputs` exposes a declared module output, but no
    /// internal node produces it (see [`super::expand`]'s module docs for
    /// the `<port>` / `_mod/<port>` producer-naming convention).
    #[error(
        "node `{host_node}` exposes module output `{port}`, but no internal node produces an \
         output named `{port}` or `_mod/{port}`"
    )]
    MissingModuleOutputProducer {
        /// The id of the node whose `module:` field triggered this include.
        host_node: String,
        /// The output port with no producer.
        port: String,
    },

    /// `host_node.outputs` exposes a declared module output, and more
    /// than one internal node produces it — ambiguous.
    #[error(
        "node `{host_node}` exposes module output `{port}`, but {} internal nodes produce it: \
         {producers:?}",
        producers.len()
    )]
    AmbiguousModuleOutput {
        /// The id of the node whose `module:` field triggered this include.
        host_node: String,
        /// The output port with more than one producer.
        port: String,
        /// The (module-local, pre-prefix) ids of every producer found.
        producers: Vec<String>,
    },
}
