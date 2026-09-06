//! Cross-file type-reference resolution: turning a field's `pkg/Type` or
//! bare `Type` reference into the [`TypeName`] it names, and checking that
//! no message type embeds itself.
//!
//! Every type [`crate::codegen`] will emit for one build (a package, or the
//! whole `common_interfaces` set) is registered in a [`TypeUniverse`]
//! *before* any code is generated, so a field's reference always resolves
//! against the complete picture rather than only the files parsed so far.

use std::collections::{HashMap, HashSet};

use crate::ast::{MessageSection, NamedTypeRef, ScalarType};
use crate::error::IdlError;
use crate::naming::{PackageName, TypeName};
use crate::span::Span;

/// Every message-shaped type available for a field's [`NamedTypeRef`] to
/// resolve against, across every file being generated together.
///
/// "Message-shaped" includes a `.msg` file's own type and each section of a
/// `.srv`/`.action` file (request/response, goal/result/feedback) — anything
/// with fields a *different* file's field could legally embed. The five
/// wire types `.action` synthesis adds on top (`_SendGoal_Request` and
/// friends, blueprint §10.3) are deliberately not registered here: nothing
/// outside their own action ever needs to name them, so `crate::codegen`'s
/// private `action` submodule wires them up directly rather than through
/// this general lookup.
#[derive(Debug, Default)]
pub struct TypeUniverse {
    entries: HashMap<(String, String), TypeName>,
}

impl TypeUniverse {
    /// An empty universe.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one message-shaped type as resolvable by name.
    pub fn register(&mut self, type_name: TypeName) {
        let key = (
            type_name.package.as_str().to_owned(),
            type_name.name.clone(),
        );
        self.entries.insert(key, type_name);
    }

    /// The type a package/name pair names, if any is registered.
    #[must_use]
    pub fn get(&self, package: &str, name: &str) -> Option<&TypeName> {
        self.entries.get(&(package.to_owned(), name.to_owned()))
    }

    /// Resolves a field's type reference: a namespaced `pkg/Type` resolves
    /// against `pkg`, a bare `Type` against `home_package`.
    ///
    /// # Errors
    ///
    /// [`IdlError::UnknownType`] when no such type is registered.
    pub fn resolve(
        &self,
        reference: &NamedTypeRef,
        home_package: &PackageName,
    ) -> Result<&TypeName, IdlError> {
        let package = reference
            .package
            .as_deref()
            .unwrap_or_else(|| home_package.as_str());
        self.get(package, &reference.name)
            .ok_or_else(|| IdlError::UnknownType {
                reference: display_reference(reference),
                span: reference.span,
            })
    }
}

fn display_reference(reference: &NamedTypeRef) -> String {
    match &reference.package {
        Some(package) => format!("{package}/{}", reference.name),
        None => reference.name.clone(),
    }
}

/// One message-shaped section, ready for the cycle check: the type it
/// defines, the package its bare references resolve against, and its
/// fields.
pub struct SectionRef<'a> {
    /// The type this section defines.
    pub type_name: TypeName,
    /// Package bare (unqualified) field references resolve against.
    pub home_package: PackageName,
    /// The section's fields.
    pub section: &'a MessageSection,
}

/// Checks that no message type in `sections` embeds itself, directly or
/// transitively, as a field — which would give it infinite size.
///
/// # Errors
///
/// [`IdlError::UnknownType`] for a reference `universe` cannot resolve
/// (surfaced here rather than left for codegen, so a cycle-adjacent typo is
/// reported before any code is emitted), or
/// [`IdlError::CircularTypeReference`] for a cycle.
pub fn check_no_cycles(
    sections: &[SectionRef<'_>],
    universe: &TypeUniverse,
) -> Result<(), IdlError> {
    let mut graph: HashMap<TypeName, Vec<(TypeName, Span)>> = HashMap::new();
    for section_ref in sections {
        let mut edges = Vec::new();
        for field in section_ref.section.fields() {
            if let ScalarType::Named(named) = &field.type_.scalar {
                let target = universe.resolve(named, &section_ref.home_package)?;
                edges.push((target.clone(), field.span));
            }
        }
        graph.insert(section_ref.type_name.clone(), edges);
    }

    let mut visited: HashSet<TypeName> = HashSet::new();
    let nodes: Vec<TypeName> = graph.keys().cloned().collect();
    for start in nodes {
        if visited.contains(&start) {
            continue;
        }
        let mut on_path: Vec<TypeName> = Vec::new();
        visit(&start, &graph, &mut visited, &mut on_path)?;
    }
    Ok(())
}

fn visit(
    node: &TypeName,
    graph: &HashMap<TypeName, Vec<(TypeName, Span)>>,
    visited: &mut HashSet<TypeName>,
    on_path: &mut Vec<TypeName>,
) -> Result<(), IdlError> {
    on_path.push(node.clone());
    if let Some(edges) = graph.get(node) {
        for (target, span) in edges {
            if let Some(start_index) = on_path.iter().position(|n| n == target) {
                let mut names: Vec<String> = on_path[start_index..]
                    .iter()
                    .map(TypeName::relative)
                    .collect();
                names.push(target.relative());
                return Err(IdlError::CircularTypeReference {
                    cycle: names.join(" -> "),
                    span: *span,
                });
            }
            if !visited.contains(target) {
                visit(target, graph, visited, on_path)?;
            }
        }
    }
    on_path.pop();
    visited.insert(node.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::naming::InterfaceKind;
    use crate::parser::parse_message;
    use crate::span::Position;

    fn pkg(name: &str) -> PackageName {
        PackageName::new(name, Span::empty(Position::new(1, 1))).unwrap()
    }

    fn type_name(package: &str, name: &str) -> TypeName {
        TypeName::new(
            pkg(package),
            InterfaceKind::Msg,
            name,
            Span::empty(Position::new(1, 1)),
        )
        .unwrap()
    }

    #[test]
    fn resolve_finds_a_bare_reference_in_the_home_package() {
        let mut universe = TypeUniverse::new();
        universe.register(type_name("geometry_msgs", "Point"));
        let file = parse_message("Point origin\n").unwrap();
        let field = file.section.fields().next().unwrap();
        let ScalarType::Named(named) = &field.type_.scalar else {
            panic!("expected a named type");
        };
        let resolved = universe.resolve(named, &pkg("geometry_msgs")).unwrap();
        assert_eq!(resolved.full(), "geometry_msgs/msg/Point");
    }

    #[test]
    fn resolve_finds_a_namespaced_reference_in_another_package() {
        let mut universe = TypeUniverse::new();
        universe.register(type_name("geometry_msgs", "Point"));
        let file = parse_message("geometry_msgs/Point position\n").unwrap();
        let field = file.section.fields().next().unwrap();
        let ScalarType::Named(named) = &field.type_.scalar else {
            panic!("expected a named type");
        };
        let resolved = universe.resolve(named, &pkg("sensor_msgs")).unwrap();
        assert_eq!(resolved.full(), "geometry_msgs/msg/Point");
    }

    #[test]
    fn resolve_rejects_an_unregistered_reference() {
        let universe = TypeUniverse::new();
        let file = parse_message("nope/Missing thing\n").unwrap();
        let field = file.section.fields().next().unwrap();
        let ScalarType::Named(named) = &field.type_.scalar else {
            panic!("expected a named type");
        };
        let err = universe.resolve(named, &pkg("geometry_msgs")).unwrap_err();
        assert_eq!(
            err,
            IdlError::UnknownType {
                reference: "nope/Missing".to_owned(),
                span: named.span,
            }
        );
    }

    #[test]
    fn acyclic_graphs_are_accepted() {
        let mut universe = TypeUniverse::new();
        universe.register(type_name("geometry_msgs", "Point"));
        universe.register(type_name("geometry_msgs", "Polygon"));
        let point = parse_message("float64 x\n").unwrap();
        let polygon = parse_message("geometry_msgs/Point[] points\n").unwrap();
        let sections = vec![
            SectionRef {
                type_name: type_name("geometry_msgs", "Point"),
                home_package: pkg("geometry_msgs"),
                section: &point.section,
            },
            SectionRef {
                type_name: type_name("geometry_msgs", "Polygon"),
                home_package: pkg("geometry_msgs"),
                section: &polygon.section,
            },
        ];
        assert_eq!(check_no_cycles(&sections, &universe), Ok(()));
    }

    #[test]
    fn a_direct_self_reference_is_a_cycle() {
        let mut universe = TypeUniverse::new();
        universe.register(type_name("pkg", "Node"));
        let node = parse_message("pkg/Node child\n").unwrap();
        let sections = vec![SectionRef {
            type_name: type_name("pkg", "Node"),
            home_package: pkg("pkg"),
            section: &node.section,
        }];
        let err = check_no_cycles(&sections, &universe).unwrap_err();
        assert!(matches!(err, IdlError::CircularTypeReference { .. }));
    }

    #[test]
    fn an_indirect_cycle_through_two_types_is_detected() {
        let mut universe = TypeUniverse::new();
        universe.register(type_name("pkg", "A"));
        universe.register(type_name("pkg", "B"));
        let a = parse_message("pkg/B b\n").unwrap();
        let b = parse_message("pkg/A a\n").unwrap();
        let sections = vec![
            SectionRef {
                type_name: type_name("pkg", "A"),
                home_package: pkg("pkg"),
                section: &a.section,
            },
            SectionRef {
                type_name: type_name("pkg", "B"),
                home_package: pkg("pkg"),
                section: &b.section,
            },
        ];
        let err = check_no_cycles(&sections, &universe).unwrap_err();
        assert!(matches!(err, IdlError::CircularTypeReference { .. }));
    }
}
