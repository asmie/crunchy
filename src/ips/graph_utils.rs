use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
};

use spectre::{edge::Edge, graph::Graph};
use ziggurat_core_crawler::summary::NetworkType;

use crate::{
    ips::{algorithm::IpsState, statistics::median},
    Node,
};

/// Find bridges in graph.
/// Bridges are edges that if removed disconnects the graph but here we try to find something
/// similar to bridges - connections that acts like bridges between two inter-connected islands
/// but cutting them do not disconnect the graph (as there can be couple of bridges for each
/// interconnected island). That is why we use betweenness centrality to find such connections
/// instead of some popular bridge finding algorithms (like Tarjan's algorithm or chain
/// decomposition).
///
/// The idea is to find connections that have high betweenness centrality on both ends. The main
/// problem is meaning of high betweenness centrality. This approach uses median of betweenness
/// centrality of all nodes as a base point for threshold. Then, to eliminate some corner cases
/// (eg. when there are only few nodes with high betweenness centrality and most of the nodes have
/// low factor value what could result in finding too many bridges) we adjust the threshold by
/// const factor read from configuration. There could be different approaches like not using
/// the median but taking value from some percentile (eg. 90th percentile) but this could lead to
/// set threshold to find too many bridges in case of eg. balanced graph (if there are many nodes
/// with similar betweenness centrality taking top 20% would result in finding fake bridges).
pub fn find_bridges(nodes: &[Node], threshold_adjustment: f64) -> HashMap<usize, HashSet<usize>> {
    let mut bridges = HashMap::new();

    // If there are less than 2 nodes there is no point in finding bridges.
    if nodes.len() < 2 {
        return bridges;
    }

    let mut betweenness_list = nodes.iter().map(|n| n.betweenness).collect::<Vec<f64>>();

    betweenness_list.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let betweenness_median = median(&betweenness_list).unwrap(); // Safe to uwrap as we checked if there are at least 2 nodes.
    let betweenness_threshold = betweenness_median * threshold_adjustment;

    for (node_idx, node) in nodes.iter().enumerate() {
        if node.betweenness < betweenness_threshold {
            continue;
        }

        for peer_idx in &node.connections {
            if nodes[*peer_idx].betweenness <= betweenness_threshold {
                continue;
            }

            bridges
                .entry(node_idx)
                .and_modify(|peers: &mut HashSet<usize>| {
                    peers.insert(*peer_idx);
                })
                .or_default()
                .insert(*peer_idx);

            bridges
                .entry(*peer_idx)
                .and_modify(|peers: &mut HashSet<usize>| {
                    peers.insert(node_idx);
                })
                .or_default()
                .insert(node_idx);
        }
    }
    bridges
}

/// Reconstruct graph from nodes and their connection subfield. This step is used to run
/// some graph algorithms on the graph (like betweenness centrality).
pub fn construct_graph(nodes: &[Node]) -> Graph<SocketAddr> {
    let mut graph = Graph::new();

    for node in nodes {
        let node_addr = node.addr;

        // This is a hack to add nodes that are not connected to any other node. That can happen
        // when are found through different network nodes. After filtering out that nodes it could
        // be seen like some nodes are not connected to any other node.
        // This is needed to run some graph algorithms on the graph - like counting betweenness or
        // closeness centrality as well as simple getting degree.
        if node.connections.is_empty() {
            graph.insert(Edge::new(node_addr, node_addr));
            continue;
        }

        for i in &node.connections {
            if *i >= nodes.len() {
                // This should not happen as we check if node has proper connection indexes when
                // we take the snapshot of the state from external source. However, re-constructing
                // graph from the nodes list that can be modified internally (eg. by removing nodes
                // that has been found to be in different network than we would like to work over)
                // could possibly lead to this situation. In such case ignore non-existing node
                // and log the error. We need to skip this connection as it could lead to out of
                // bounds error.
                eprintln!(
                    "Node {} has connection to non-existing node {}",
                    node_addr, i
                );
                continue;
            }
            let edge = Edge::new(node_addr, nodes[*i].addr);
            graph.insert(edge);
        }
    }
    graph
}

/// Removes node from the state and updates all indices in the peerlist
pub fn remove_node(nodes: &mut Vec<Node>, node_idx: usize) {
    let node = nodes[node_idx].clone();

    for rnode in nodes.iter_mut() {
        if let Some(pos) = rnode.connections.iter().position(|x| *x == node_idx) {
            rnode.connections.remove(pos);
        }
    }

    nodes.retain(|x| x.addr != node.addr);

    // Now the tricky part - we need to update all indices in the peerlist
    // of all nodes that have higher index than the one we removed
    for node in nodes.iter_mut() {
        for peer_idx in node.connections.iter_mut() {
            if *peer_idx > node_idx {
                *peer_idx -= 1;
            }
        }
    }
}

/// Find node with lowest betweenness centrality in the provided nodes indexes.
pub fn find_lowest_betweenness(nodes_idx: &[usize], state: &IpsState) -> usize {
    let mut lowest_betweenness = f64::MAX;
    let mut lowest_betweenness_idx = 0;

    for idx in nodes_idx.iter() {
        let betweenness = state.nodes[*idx].betweenness;
        if betweenness < lowest_betweenness {
            lowest_betweenness = betweenness;
            lowest_betweenness_idx = *idx;
        }
    }

    lowest_betweenness_idx
}

/// Create new vector with nodes that have common network type.
pub fn filter_network(nodes: &[Node], network: NetworkType) -> Vec<Node> {
    let mut network_nodes = nodes.to_vec();

    // Two step here - first we collect all indices that we want to remove and then we remove them
    // First step is using `rev` so we don't mess up the order of those found indices once we start removing them
    let mut indices_to_remove = Vec::with_capacity(network_nodes.len());
    for idx in (0..network_nodes.len()).rev() {
        if network_nodes[idx].network_type != network {
            indices_to_remove.push(idx);
        }
    }

    for undesired_idx in indices_to_remove {
        remove_node(&mut network_nodes, undesired_idx);
    }

    network_nodes
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use super::*;

    #[test]
    fn construct_graph_test() {
        let nodes = vec![
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 1234),
                connections: vec![1, 2],
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 0, 0, 0)), 1234),
                connections: vec![0, 2],
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(2, 0, 0, 0)), 1234),
                connections: vec![0, 1],
                ..Default::default()
            },
        ];

        let mut graph = construct_graph(&nodes);
        let degrees = graph.degree_centrality();
        assert_eq!(
            degrees
                .get(&SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
                    1234
                ))
                .unwrap(),
            &2
        );
        assert_eq!(
            degrees
                .get(&SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(1, 0, 0, 0)),
                    1234
                ))
                .unwrap(),
            &2
        );
        assert_eq!(
            degrees
                .get(&SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(2, 0, 0, 0)),
                    1234
                ))
                .unwrap(),
            &2
        );
    }

    #[test]
    fn find_bridges_test() {
        let nodes = vec![
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 1234),
                betweenness: 1.0,
                connections: vec![1, 2],
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 1234),
                betweenness: 1.5,
                connections: vec![0, 2, 3],
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 1234),
                betweenness: 1.3,
                connections: vec![1, 3],
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 1234),
                betweenness: 3.1,
                connections: vec![1, 2, 4],
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 1234),
                betweenness: 3.2,
                connections: vec![3, 5, 7],
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 1234),
                betweenness: 1.0,
                connections: vec![4, 6],
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 1234),
                betweenness: 1.2,
                connections: vec![5, 7],
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 1234),
                betweenness: 1.4,
                connections: vec![4, 6],
                ..Default::default()
            },
        ];

        let bridges = find_bridges(&nodes, 1.25);
        assert!(bridges.contains_key(&3));
        let peers = bridges.get(&3).unwrap();
        assert_eq!(peers.len(), 1);
        assert!(peers.contains(&4));
    }

    #[test]
    fn filter_network_test() {
        let nodes = vec![
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 0, 0, 0)), 1234),
                connections: vec![1, 2],
                network_type: NetworkType::Zcash,
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(2, 0, 0, 0)), 1234),
                network_type: NetworkType::Zcash,
                connections: vec![0, 2, 3],
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(3, 0, 0, 0)), 1234),
                network_type: NetworkType::Unknown,
                connections: vec![1, 3],
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(4, 0, 0, 0)), 1234),
                network_type: NetworkType::Unknown,
                connections: vec![1, 2, 4],
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(5, 0, 0, 0)), 1234),
                network_type: NetworkType::Unknown,
                connections: vec![3, 5, 7],
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(6, 0, 0, 0)), 1234),
                network_type: NetworkType::Unknown,
                connections: vec![4, 6],
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(7, 0, 0, 0)), 1234),
                network_type: NetworkType::Zcash,
                connections: vec![5, 7],
                ..Default::default()
            },
            Node {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 0, 0, 0)), 1234),
                network_type: NetworkType::Unknown,
                connections: vec![4, 6],
                ..Default::default()
            },
        ];

        let filtered = filter_network(&nodes, NetworkType::Zcash);
        assert_eq!(filtered.len(), 3);
        for node in filtered {
            assert!(node.network_type == NetworkType::Zcash);
        }

        let filtered = filter_network(&nodes, NetworkType::Ripple);
        assert_eq!(filtered.len(), 0);

        let filtered = filter_network(&nodes, NetworkType::Unknown);
        assert_eq!(filtered.len(), 5);
        for node in filtered {
            assert!(node.network_type == NetworkType::Unknown);
        }
    }
}
