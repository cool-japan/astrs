//! Service/action pattern pairing (blueprint §9.4).
//!
//! The manifest's `pattern:` field only *labels* a node's role
//! (`service-server`, `service-client`, `action-server`, `action-client`);
//! it does not itself say which client is wired to which server — that is
//! ordinary port wiring, exactly like any other edge. This module derives
//! the request/response *correlation* the blueprint describes ("`pattern:`
//! in the manifest generates the correct port pairs and validates
//! wiring") by looking at which pattern-labeled nodes are actually
//! connected: a [`PatternPair`] is formed for every client/server pair of
//! the same kind that has at least one edge between them in either
//! direction.
//!
//! # What pairing is for
//!
//! Two things downstream of this module care about the result:
//!
//! - **Cycle legality** ([`crate::scc`]): a directed cycle formed entirely
//!   of edges that belong to some [`PatternPair`] is the expected
//!   request/response loop (client → server → client), not a deadlock
//!   risk, so [`crate::scc`] does not warn about it.
//! - **Static type checking is *not* exempted here.** Blueprint §9.2's
//!   exemption ("Correlated (service/action) ports are exempt from
//!   runtime payload checks") is about the node API skipping a runtime
//!   payload assertion — it says nothing about `astrs validate`'s static
//!   URN check, so [`crate::typecheck::check_types`] type-checks
//!   pattern-correlated edges exactly like any other edge.

use std::collections::BTreeMap;
use std::fmt;

use astrs_manifest::Pattern;
use serde::{Deserialize, Serialize};

use crate::diagnostic::{Diagnostic, DiagnosticKind, Severity};
use crate::edge::{Edge, EdgeKey};
use crate::ids::NodeId;
use crate::node::GraphNode;

/// Which family of correlated request/response exchange a [`PatternPair`]
/// represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum PatternKind {
    /// A `service-server` / `service-client` pair (`request_id` correlation).
    Service,
    /// An `action-server` / `action-client` pair (`goal_id`/`goal_status`
    /// correlation).
    Action,
}

impl fmt::Display for PatternKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Service => "service",
            Self::Action => "action",
        })
    }
}

/// Which side of a [`PatternPair`] a node plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Client,
    Server,
}

/// Split a manifest [`Pattern`] into its kind and role.
fn kind_role(pattern: Pattern) -> (PatternKind, Role) {
    match pattern {
        Pattern::ServiceServer => (PatternKind::Service, Role::Server),
        Pattern::ServiceClient => (PatternKind::Service, Role::Client),
        Pattern::ActionServer => (PatternKind::Action, Role::Server),
        Pattern::ActionClient => (PatternKind::Action, Role::Client),
    }
}

/// A correlated client/server pair, derived from wiring rather than
/// declared explicitly (see this module's top-level docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternPair {
    /// Whether this is a service or action pair.
    pub kind: PatternKind,
    /// The client-role node.
    pub client: NodeId,
    /// The server-role node.
    pub server: NodeId,
    /// Edges from `client`'s outputs into `server`'s inputs (requests /
    /// goals).
    pub client_to_server: Vec<EdgeKey>,
    /// Edges from `server`'s outputs into `client`'s inputs (responses /
    /// feedback / results).
    pub server_to_client: Vec<EdgeKey>,
}

impl PatternPair {
    /// Every edge belonging to this pair, request and response alike —
    /// the edges [`crate::Scc`] cycle analysis treats as legally cyclic.
    pub fn edges(&self) -> impl Iterator<Item = &EdgeKey> {
        self.client_to_server.iter().chain(&self.server_to_client)
    }

    /// Whether at least one request-direction edge (client → server) exists.
    #[must_use]
    pub fn has_request_edge(&self) -> bool {
        !self.client_to_server.is_empty()
    }

    /// Whether at least one response-direction edge (server → client) exists.
    #[must_use]
    pub fn has_response_edge(&self) -> bool {
        !self.server_to_client.is_empty()
    }

    /// Whether this pair is wired in both directions.
    #[must_use]
    pub fn is_fully_wired(&self) -> bool {
        self.has_request_edge() && self.has_response_edge()
    }
}

/// Derive every [`PatternPair`] in `nodes`/`edges`, plus diagnostics for
/// orphaned or partially-wired pattern-labeled nodes.
///
/// # Complexity
///
/// `O(P^2 + E)` where `P` is the number of pattern-labeled nodes and `E`
/// is the edge count: edges are indexed by producer once (`O(E)`), then
/// every same-kind client/server pair is checked against that index
/// (`O(P^2)` pairs, each an indexed lookup). Dataflow graphs are node
/// graphs, not internet-scale — this is not a hot path (it runs once at
/// graph construction), so the simple quadratic-in-pattern-nodes form is
/// preferred over a more intricate matching algorithm for the sake of
/// this being easy to verify correct.
pub(crate) fn derive_pattern_pairs(
    nodes: &BTreeMap<NodeId, GraphNode>,
    edges: &BTreeMap<EdgeKey, Edge>,
) -> (Vec<PatternPair>, Vec<Diagnostic>) {
    // Index edges by producer node so `edges_between` does not rescan the
    // whole edge set for every candidate pair.
    let mut by_producer: BTreeMap<&NodeId, Vec<&EdgeKey>> = BTreeMap::new();
    for (key, edge) in edges {
        if let Some(producer) = edge.from.producer_node() {
            by_producer.entry(producer).or_default().push(key);
        }
    }

    let edges_between = |from: &NodeId, to: &NodeId| -> Vec<EdgeKey> {
        by_producer
            .get(from)
            .into_iter()
            .flatten()
            .filter(|key| key.consumer == *to)
            .map(|key| (*key).clone())
            .collect()
    };

    let mut clients: BTreeMap<PatternKind, Vec<&NodeId>> = BTreeMap::new();
    let mut servers: BTreeMap<PatternKind, Vec<&NodeId>> = BTreeMap::new();
    for node in nodes.values() {
        let Some(pattern) = node.pattern else {
            continue;
        };
        let (kind, role) = kind_role(pattern);
        match role {
            Role::Client => clients.entry(kind).or_default().push(&node.id),
            Role::Server => servers.entry(kind).or_default().push(&node.id),
        }
    }

    let mut pairs = Vec::new();
    let mut diagnostics = Vec::new();
    let mut paired: std::collections::BTreeSet<&NodeId> = std::collections::BTreeSet::new();

    for (kind, kind_clients) in &clients {
        let Some(kind_servers) = servers.get(kind) else {
            continue;
        };
        for &client in kind_clients {
            for &server in kind_servers {
                let client_to_server = edges_between(client, server);
                let server_to_client = edges_between(server, client);
                if client_to_server.is_empty() && server_to_client.is_empty() {
                    continue;
                }
                paired.insert(client);
                paired.insert(server);

                let has_request = !client_to_server.is_empty();
                let has_response = !server_to_client.is_empty();
                if !(has_request && has_response) {
                    diagnostics.push(Diagnostic::new(
                        Severity::Warning,
                        DiagnosticKind::PartialPatternPair {
                            client: client.clone(),
                            server: server.clone(),
                            kind: *kind,
                            has_request_edge: has_request,
                            has_response_edge: has_response,
                        },
                    ));
                }

                pairs.push(PatternPair {
                    kind: *kind,
                    client: client.clone(),
                    server: server.clone(),
                    client_to_server,
                    server_to_client,
                });
            }
        }
    }

    // Any pattern-labeled node that never joined a pair is orphaned.
    for node in nodes.values() {
        let Some(pattern) = node.pattern else {
            continue;
        };
        if !paired.contains(&node.id) {
            diagnostics.push(Diagnostic::new(
                Severity::Warning,
                DiagnosticKind::UnpairedPatternNode {
                    node: node.id.clone(),
                    pattern,
                },
            ));
        }
    }

    pairs.sort_by(|a, b| (&a.client, &a.server).cmp(&(&b.client, &b.server)));
    (pairs, diagnostics)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::collections::BTreeMap;

    use super::*;
    use crate::edge::{EdgeSource, QueueConfig};
    use crate::ids::{MachineId, PortName};
    use crate::node::{InputPort, OutputPort};

    fn node(id: &str, pattern: Option<Pattern>) -> GraphNode {
        GraphNode {
            id: NodeId::new(id),
            outputs: BTreeMap::new(),
            inputs: BTreeMap::new(),
            pattern,
            machine: MachineId::CoordinatorLocal,
            spawns_process: true,
        }
    }

    fn wire_output(node: &mut GraphNode, name: &str) {
        node.outputs
            .insert(PortName::new(name), OutputPort { type_urn: None });
    }

    fn wire_input(node: &mut GraphNode, name: &str) {
        node.inputs
            .insert(PortName::new(name), InputPort { type_urn: None });
    }

    fn edge(producer: &str, output: &str) -> Edge {
        Edge {
            from: EdgeSource::NodeOutput {
                node: NodeId::new(producer),
                output: PortName::new(output),
            },
            queue: QueueConfig::default(),
        }
    }

    #[test]
    fn full_request_response_pair_has_no_diagnostics() {
        let mut client = node("caller", Some(Pattern::ServiceClient));
        wire_output(&mut client, "request");
        wire_input(&mut client, "response");
        let mut server = node("adder", Some(Pattern::ServiceServer));
        wire_input(&mut server, "request");
        wire_output(&mut server, "response");

        let mut nodes = BTreeMap::new();
        nodes.insert(client.id.clone(), client);
        nodes.insert(server.id.clone(), server);

        let mut edges = BTreeMap::new();
        edges.insert(
            EdgeKey::new(NodeId::new("adder"), PortName::new("request")),
            edge("caller", "request"),
        );
        edges.insert(
            EdgeKey::new(NodeId::new("caller"), PortName::new("response")),
            edge("adder", "response"),
        );

        let (pairs, diagnostics) = derive_pattern_pairs(&nodes, &edges);
        assert_eq!(pairs.len(), 1);
        assert!(pairs[0].is_fully_wired());
        assert_eq!(pairs[0].kind, PatternKind::Service);
        assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    }

    #[test]
    fn request_only_pair_is_flagged_partial() {
        let mut client = node("caller", Some(Pattern::ServiceClient));
        wire_output(&mut client, "request");
        let mut server = node("adder", Some(Pattern::ServiceServer));
        wire_input(&mut server, "request");

        let mut nodes = BTreeMap::new();
        nodes.insert(client.id.clone(), client);
        nodes.insert(server.id.clone(), server);

        let mut edges = BTreeMap::new();
        edges.insert(
            EdgeKey::new(NodeId::new("adder"), PortName::new("request")),
            edge("caller", "request"),
        );

        let (pairs, diagnostics) = derive_pattern_pairs(&nodes, &edges);
        assert_eq!(pairs.len(), 1);
        assert!(!pairs[0].is_fully_wired());
        assert!(pairs[0].has_request_edge());
        assert!(!pairs[0].has_response_edge());
        assert_eq!(diagnostics.len(), 1);
        assert!(matches!(
            diagnostics[0].kind,
            DiagnosticKind::PartialPatternPair { .. }
        ));
    }

    #[test]
    fn unwired_pattern_node_is_orphaned() {
        let client = node("caller", Some(Pattern::ServiceClient));
        let mut nodes = BTreeMap::new();
        nodes.insert(client.id.clone(), client);
        let edges = BTreeMap::new();

        let (pairs, diagnostics) = derive_pattern_pairs(&nodes, &edges);
        assert!(pairs.is_empty());
        assert_eq!(diagnostics.len(), 1);
        assert!(matches!(
            diagnostics[0].kind,
            DiagnosticKind::UnpairedPatternNode { .. }
        ));
    }

    #[test]
    fn nodes_without_a_pattern_are_ignored() {
        let n1 = node("a", None);
        let n2 = node("b", None);
        let mut nodes = BTreeMap::new();
        nodes.insert(n1.id.clone(), n1);
        nodes.insert(n2.id.clone(), n2);
        let mut edges = BTreeMap::new();
        edges.insert(
            EdgeKey::new(NodeId::new("b"), PortName::new("in")),
            edge("a", "out"),
        );

        let (pairs, diagnostics) = derive_pattern_pairs(&nodes, &edges);
        assert!(pairs.is_empty());
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn action_and_service_clients_do_not_cross_pair() {
        let client = node("client", Some(Pattern::ServiceClient));
        let server = node("server", Some(Pattern::ActionServer));
        let mut nodes = BTreeMap::new();
        nodes.insert(client.id.clone(), client);
        nodes.insert(server.id.clone(), server);
        let edges = BTreeMap::new();

        let (pairs, diagnostics) = derive_pattern_pairs(&nodes, &edges);
        assert!(pairs.is_empty());
        // Both are orphaned since no same-kind counterpart exists.
        assert_eq!(diagnostics.len(), 2);
    }

    #[test]
    fn pattern_pair_edges_iterator_chains_both_directions() {
        let pair = PatternPair {
            kind: PatternKind::Service,
            client: NodeId::new("c"),
            server: NodeId::new("s"),
            client_to_server: vec![EdgeKey::new(NodeId::new("s"), PortName::new("req"))],
            server_to_client: vec![EdgeKey::new(NodeId::new("c"), PortName::new("resp"))],
        };
        assert_eq!(pair.edges().count(), 2);
    }

    #[test]
    fn pattern_kind_displays_lowercase() {
        assert_eq!(PatternKind::Service.to_string(), "service");
        assert_eq!(PatternKind::Action.to_string(), "action");
    }
}
