use std::collections::BTreeMap;

use crate::LogicalPath;

/// Detects cycles in a graph of *resolved, local* dependency edges (see
/// `crates/logical/README.md`, "A: cycles are allowed in a draft, caught
/// at validation" — scope is local edges only; a `RemoteReferenceV1`
/// or an unresolved edge is never part of this graph, so it can neither
/// hide nor fabricate a cycle here).
///
/// Standard three-color DFS. Reports one cycle per back-edge encountered;
/// if several cycles share nodes, each back-edge still gets its own
/// report (documented scope, not a bug — see the README's cascading-
/// errors discussion for why this is an accepted trade-off rather than
/// something `logical` tries to de-duplicate further).
pub(crate) fn find_cycles(
    edges: &BTreeMap<LogicalPath, Vec<LogicalPath>>,
) -> Vec<Vec<LogicalPath>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Color {
        White,
        Gray,
        Black,
    }

    let mut color: BTreeMap<&LogicalPath, Color> =
        edges.keys().map(|node| (node, Color::White)).collect();
    let mut stack: Vec<&LogicalPath> = Vec::new();
    let mut cycles = Vec::new();

    // Iterative DFS (avoids fighting the borrow checker over a recursive
    // closure capturing `color`/`stack`/`cycles` mutably).
    let nodes: Vec<&LogicalPath> = edges.keys().collect();
    for start in nodes {
        if color[start] != Color::White {
            continue;
        }
        let mut work: Vec<(&LogicalPath, usize)> = vec![(start, 0)];
        color.insert(start, Color::Gray);
        stack.push(start);

        while let Some((node, next_edge)) = work.pop() {
            let neighbors = edges.get(node).map(Vec::as_slice).unwrap_or(&[]);
            if next_edge < neighbors.len() {
                work.push((node, next_edge + 1));
                let neighbor = &neighbors[next_edge];
                match color.get(neighbor).copied().unwrap_or(Color::Black) {
                    Color::White => {
                        color.insert(neighbor, Color::Gray);
                        stack.push(neighbor);
                        work.push((neighbor, 0));
                    }
                    Color::Gray => {
                        let start_index = stack
                            .iter()
                            .position(|n| *n == neighbor)
                            .expect("a Gray node is always on the current DFS stack");
                        let mut cycle: Vec<LogicalPath> =
                            stack[start_index..].iter().map(|n| (*n).clone()).collect();
                        cycle.push(neighbor.clone());
                        cycles.push(cycle);
                    }
                    Color::Black => {}
                }
            } else {
                color.insert(node, Color::Black);
                stack.pop();
            }
        }
    }

    cycles
}

#[cfg(test)]
mod test {
    use super::*;
    use disk::EntryName;

    fn path(name: &str) -> LogicalPath {
        LogicalPath::root(EntryName(name.to_string()))
    }

    #[test]
    fn reports_no_cycle_for_an_acyclic_graph() {
        let mut edges = BTreeMap::new();
        edges.insert(path("a"), vec![path("b")]);
        edges.insert(path("b"), vec![path("c")]);
        edges.insert(path("c"), vec![]);

        assert!(find_cycles(&edges).is_empty());
    }

    #[test]
    fn reports_a_direct_self_cycle() {
        let mut edges = BTreeMap::new();
        edges.insert(path("a"), vec![path("a")]);

        let cycles = find_cycles(&edges);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0], vec![path("a"), path("a")]);
    }

    #[test]
    fn reports_a_three_node_cycle() {
        let mut edges = BTreeMap::new();
        edges.insert(path("a"), vec![path("b")]);
        edges.insert(path("b"), vec![path("c")]);
        edges.insert(path("c"), vec![path("a")]);

        let cycles = find_cycles(&edges);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].first(), cycles[0].last());
        assert_eq!(cycles[0].len(), 4);
    }

    #[test]
    fn ignores_a_missing_edge_target_not_in_the_node_set() {
        // Simulates an edge whose target never resolved and so was never
        // added as its own node — must not panic.
        let mut edges = BTreeMap::new();
        edges.insert(path("a"), vec![path("nonexistent")]);

        assert!(find_cycles(&edges).is_empty());
    }

    #[test]
    fn a_diamond_with_no_cycle_reports_nothing() {
        let mut edges = BTreeMap::new();
        edges.insert(path("a"), vec![path("b"), path("c")]);
        edges.insert(path("b"), vec![path("d")]);
        edges.insert(path("c"), vec![path("d")]);
        edges.insert(path("d"), vec![]);

        assert!(find_cycles(&edges).is_empty());
    }
}
