// Intelligent Peer Sharing (IPS) module
// Selection is based on the "beauty" contest of the nodes - each node is evaluated based on its
// degree, betweenness, closeness and eigenvector centrality. Then, if requested ranking is
// updated with location factor. Each factor has its own weight that is used to determine
// factor's importance to the calculation of the final ranking. That gives possibility
// to test different approaches to the selection of the peers, without re-compiling the code.
// Weights are defined in the configuration file.
// When the ranking is calculated, the peers are selected based on the ranking. The number of
// peers can be changed and it is defined in the configuration file. Algorithm is constructed
// as step by step process, so it is easy to add new steps or change the order of the steps.
// Especially, there could be a need to add some modifiers to the ranking.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::IpAddr,
    str::FromStr,
};

use serde::{Deserialize, Serialize};
use spectre::{
    edge::Edge,
    graph::{AGraph, Graph},
};

use crate::{
    config::{GeoLocationMode, IPSConfiguration},
    CrunchyState, Node,
};

/// Intelligent Peer Sharing (IPS) module structure
#[derive(Default, Clone)]
pub struct Ips {
    config: IPSConfiguration,
    degree_factors: NormalizationFactors,
    betweenness_factors: NormalizationFactors,
    closeness_factors: NormalizationFactors,
    eigenvector_factors: NormalizationFactors,
}

/// Peer list structure containing peer list for each node
#[derive(Clone, Serialize, Deserialize)]
pub struct Peer {
    /// IP address of the node
    pub ip: IpAddr,
    /// List of peers for the node
    pub list: Vec<IpAddr>,
}

/// Internal structure for storing peer information
#[derive(Copy, Clone)]
struct PeerEntry {
    /// IP address of the peer
    pub ip: IpAddr,
    /// Index of the peer in the state.nodes
    pub index: usize,
    /// Ranking of the peer
    pub rating: f64,
}

const NORMALIZE_TO_VALUE: f64 = 100.0;
const NORMALIZE_HALF: f64 = NORMALIZE_TO_VALUE / 2.0;
const NORMALIZE_2_3: f64 = NORMALIZE_TO_VALUE * 2.0 / 3.0;
const NORMALIZE_1_3: f64 = NORMALIZE_TO_VALUE * 1.0 / 3.0;

const ERR_PARSE_IP: &str = "failed to parse IP address";
const ERR_GET_DEGREE: &str = "failed to get degree";
const ERR_GET_EIGENVECTOR: &str = "failed to get eigenvector";

#[derive(Default, Clone)]
struct NormalizationFactors {
    min: f64,
    max: f64,
}

impl Ips {
    pub fn new(config: IPSConfiguration) -> Ips {
        Ips {
            config,
            ..Default::default()
        }
    }

    /// Generate peer list - main function with The Algorithm
    /// It needs state and agraph to be passed as parameters which need to be correlated with
    /// the crawler's state and agraph (and with each other), so the indexes saved in the
    /// agraph are the same as the positions of the nodes in the state.nodes.
    pub async fn generate(&mut self, state: &CrunchyState, agraph: &AGraph) -> Vec<Peer> {
        let mut peer_list = Vec::new();

        // Reconstruct graph from the agraph - we need to do this because we need all the
        // measures provided by spectre's graph implementation.
        // Using agraph gives us certainity that we are using the same graph as the crawler and
        // there are only good nodes there (this is critical assumption!). Second assumption is
        // that agraph node indexes are the same as in the state.nodes vector.
        let mut graph = self.construct_graph(&state.nodes, agraph);

        // 0 - Detect islands
        // To reconsider if islands should be merged prior to any other computations or not.
        // IMHO, if there are islands they can influence on the results of the computations.
        // TODO(asmie): Merging islands is not implemented yet.
        let _islands = self.detect_islands(agraph);

        // Now take the current params
        let degrees = graph.degree_centrality();
        let degree_avg = self.degree_centrality_avg(&degrees);
        let eigenvalues = graph.eigenvalue_centrality();

        // Determine factors used for normalization.
        // Normalization step is needed to make sure that all the factors are in the same range and
        // weights can be applied to them.
        self.degree_factors =
            NormalizationFactors::determine(&degrees.values().cloned().collect::<Vec<u32>>());

        self.eigenvector_factors =
            NormalizationFactors::determine(&eigenvalues.values().cloned().collect::<Vec<f64>>());

        let betweenness = &state
            .nodes
            .iter()
            .map(|n| n.betweenness)
            .collect::<Vec<f64>>();
        self.betweenness_factors = NormalizationFactors::determine(betweenness);

        let closeness = &state
            .nodes
            .iter()
            .map(|n| n.closeness)
            .collect::<Vec<f64>>();
        self.closeness_factors = NormalizationFactors::determine(closeness);

        // Node rating can be split into two parts: constant and variable depending on the node's
        // location. Now we can compute each node's constant rating based on some graph params.
        // Vector contains IpAddr, node index (from the state.nodes) and rating. We need index just
        // to be able to easily get the node from nodes vector after sorting.
        let mut const_factors = Vec::with_capacity(state.nodes.len());
        for (idx, node) in state.nodes.iter().enumerate() {
            let ip = IpAddr::from_str(node.ip.as_str()).expect(ERR_PARSE_IP);
            const_factors.push(PeerEntry {
                ip,
                index: idx,
                rating: self.rate_node(
                    node,
                    *degrees.get(&ip).expect(ERR_GET_DEGREE), // should be safe to unwrap here as degree hashmap is constructed from the same nodes as the state.nodes
                    *eigenvalues.get(&ip).expect(ERR_GET_EIGENVECTOR), // should be safe to unwrap here as eigenvector hashmap is constructed from the same nodes as the state.nodes
                ),
            });
        }

        // Iterate over nodes to generate peerlist entry for each node
        for (node_idx, node) in state.nodes.iter().enumerate() {
            let node_ip = IpAddr::from_str(node.ip.as_str()).expect(ERR_PARSE_IP);

            // Clone const factors for each node to be able to modify them
            let mut peer_ratings = const_factors.clone();

            let mut curr_peer_ratings: Vec<PeerEntry> = Vec::new();

            let mut peer_list_entry = Peer {
                ip: node_ip,
                list: Vec::new(),
            };

            // 1 - update ranks by location for specified node
            // This need to be done every time as location ranking will change for differently
            // located nodes.
            if self.config.geolocation != GeoLocationMode::Off {
                self.update_rating_by_location(node, &state.nodes, &mut peer_ratings);
            }

            // Load peerlist with current connections (we don't want to change everything)
            for peer in &agraph[node_idx] {
                peer_list_entry
                    .list
                    .push(IpAddr::from_str(state.nodes[*peer].ip.as_str()).expect(ERR_PARSE_IP));

                // Remember current peer ratings
                curr_peer_ratings.push(peer_ratings[*peer]);
            }

            // Get current node's degree for further computations
            let degree = *degrees.get(&node_ip).expect(ERR_GET_DEGREE);

            // 2 - Calculate desired vertex degree
            // In the first iteration we will use degree average so all nodes should pursue to
            // that level. That could be bad if graph's vertexes have very high (or low) degrees
            // and therefore, delta is very high (or low) too. But until we have some better idea
            // this one is the best we can do to keep up with the graph.
            let desired_degree = ((degree_avg + degree as f64) / 2.0).round() as u32;

            // 3 - Calculate how many peers to add or delete from peerlist
            let mut peers_to_delete_count = if desired_degree < degree {
                degree.saturating_sub(desired_degree)
            } else {
                // Check if config forces to change peerlist even if we have good degree.
                // This should be always set to at least one to allow for some changes in graph -
                // searching for better potential peers.
                self.config.change_at_least
            };

            // Calculating how many peers should be added. If we have more peers than desired degree
            // we will add at least config.change_at_least peers.
            let mut peers_to_add_count = if desired_degree > degree {
                desired_degree
                    .saturating_sub(degree)
                    .saturating_add(peers_to_delete_count)
            } else {
                self.config.change_at_least
            };

            // Limit number of changes to config value
            if peers_to_add_count > self.config.change_no_more {
                peers_to_add_count = self.config.change_no_more;
            }

            // Remove node itself to ensure we don't add it to peerlist
            peer_ratings.retain(|x| x.index != node_idx);

            // Sort peers by rating (highest first)
            curr_peer_ratings.sort_by(|a, b| b.rating.partial_cmp(&a.rating).unwrap());

            // 4 - Choose peers to delete from peerlist (based on ranking)
            while peers_to_delete_count > 0 && curr_peer_ratings.pop().is_some() {
                peers_to_delete_count -= 1;
            }

            // 5 - Find peers to add from selected peers (based on rating)
            if peers_to_add_count > 0 {
                // Sort peers by rating
                peer_ratings.sort_by(|a, b| b.rating.partial_cmp(&a.rating).unwrap());

                // Remove peers that are already in peerlist
                peer_ratings.retain(|x| !peer_list_entry.list.contains(&x.ip));

                let mut candidates = peer_ratings
                    .iter()
                    .take((peers_to_add_count * 2) as usize) // Take twice as many candidates
                    .copied()
                    .collect::<Vec<_>>();

                // Here we have 2*peers_to_add_count candidates to add sorted by ranking.
                // We need to choose best ones from them - let's choose those with lowest
                // betweenness factor - just to avoid creating "hot" nodes that have very high
                // importance to the network which can be risky if such node goes down.
                candidates.sort_by(|a, b| {
                    state.nodes[a.index]
                        .betweenness
                        .partial_cmp(&state.nodes[b.index].betweenness)
                        .unwrap()
                });

                for peer in candidates.iter().take(peers_to_add_count as usize) {
                    peer_list_entry.list.push(peer.ip);
                }
            }

            // Do not compute factors one more time after every single peerlist addition. At least
            // for now, when computing factors is very expensive (especially betweenness and closeness).
            // Re-calculating it after each node for whole graph would take too long.
            // TODO(asmie): recalculate some factors (like islands) after each node to check if graph is still connected

            peer_list.push(peer_list_entry);
        }
        peer_list
    }

    // Helper functions

    /// Update nodes rating based on location
    fn update_rating_by_location(
        &self,
        selected_node: &Node,
        nodes: &[Node],
        ratings: &mut [PeerEntry],
    ) {
        if selected_node.geolocation.is_none() {
            return;
        }

        let selected_location =
            if let Some(location) = selected_node.geolocation.as_ref().unwrap().location {
                location
            } else {
                return;
            };

        for (node_idx, node) in nodes.iter().enumerate() {
            if node.geolocation.is_none() {
                continue;
            }

            let geo_info = node.geolocation.as_ref().unwrap();
            if geo_info.location.is_none() {
                continue;
            }

            let distance = selected_location.distance_to(geo_info.location.unwrap());
            let minmax_distance_m = self.config.geolocation_minmax_distance_km as f64 * 1000.0;

            // Map distance to some levels of rating - now they are taken arbitrarily but
            // they should be somehow related to the distance.
            let rating = if self.config.geolocation == GeoLocationMode::PreferCloser {
                match distance {
                    _ if distance < minmax_distance_m => NORMALIZE_TO_VALUE,
                    _ if distance < 2.0 * minmax_distance_m => NORMALIZE_2_3,
                    _ if distance < 3.0 * minmax_distance_m => NORMALIZE_1_3,
                    _ => 0.0,
                }
            } else {
                match distance {
                    _ if distance < 0.5 * minmax_distance_m => 0.0,
                    _ if distance < minmax_distance_m => NORMALIZE_HALF,
                    _ => NORMALIZE_TO_VALUE,
                }
            };
            ratings[node_idx].rating += rating * self.config.mcda_weights.location;
        }
    }

    fn construct_graph(&self, nodes: &[Node], agraph: &AGraph) -> Graph<IpAddr> {
        let mut graph = Graph::new();

        for i in 0..agraph.len() {
            for j in 0..agraph[i].len() {
                let edge = Edge::new(
                    IpAddr::from_str(nodes[i].ip.as_str()).expect(ERR_PARSE_IP),
                    IpAddr::from_str(nodes[j].ip.as_str()).expect(ERR_PARSE_IP),
                );
                graph.insert(edge);
            }
        }
        graph
    }

    fn degree_centrality_avg(&self, degrees: &HashMap<IpAddr, u32>) -> f64 {
        (degrees.iter().fold(0, |acc, (_, &degree)| acc + degree) as f64) / degrees.len() as f64
    }

    fn rate_node(&self, node: &Node, degree: u32, eigenvalue: f64) -> f64 {
        // Calculate rating for node (if min == max for normalization factors then rating is
        // not increased for that factor as lerp() returns 0.0).
        // Rating is a combination of the following factors:
        let mut rating = 0.0;

        // 1. Degree
        rating += self.degree_factors.scale(degree as f64)
            * NORMALIZE_TO_VALUE
            * self.config.mcda_weights.degree;

        // 2. Betweenness
        rating += self.betweenness_factors.scale(node.betweenness)
            * NORMALIZE_TO_VALUE
            * self.config.mcda_weights.betweenness;

        // 3. Closeness
        rating += self.closeness_factors.scale(node.closeness)
            * NORMALIZE_TO_VALUE
            * self.config.mcda_weights.closeness;

        // 4. Eigenvector
        rating += self.eigenvector_factors.scale(eigenvalue)
            * NORMALIZE_TO_VALUE
            * self.config.mcda_weights.eigenvector;

        rating
    }

    // Very simple algorithm to detect islands.
    // Take first vertex and do BFS to find all connected vertices. If there are any unvisited vertices
    // create new island and do BFS one more time. Repeat until all vertices are visited.
    fn detect_islands(&self, agraph: &AGraph) -> Vec<HashSet<usize>> {
        let mut islands = Vec::new();
        let mut visited = vec![false; agraph.len()];

        for i in 0..agraph.len() {
            if visited[i] {
                continue;
            }

            let mut island = HashSet::new();
            let mut queue = VecDeque::new();
            queue.push_back(i);

            while let Some(node_idx) = queue.pop_front() {
                if visited[node_idx] {
                    continue;
                }

                island.insert(node_idx);

                visited[node_idx] = true;

                for j in 0..agraph[node_idx].len() {
                    if !visited[agraph[node_idx][j]] {
                        queue.push_back(agraph[node_idx][j]);
                    }
                }
            }
            islands.push(island);
        }
        islands
    }
}

impl NormalizationFactors {
    /// Determine min and max values for normalization.
    fn determine<T>(list: &[T]) -> NormalizationFactors
    where
        T: PartialOrd + Into<f64> + Copy,
    {
        let min = list
            .iter()
            .min_by(|a, b| a.partial_cmp(b).unwrap())
            .unwrap();
        let max = list
            .iter()
            .max_by(|a, b| a.partial_cmp(b).unwrap())
            .unwrap();

        NormalizationFactors {
            min: (*min).into(),
            max: (*max).into(),
        }
    }

    /// Scale value to [0.0, 1.0] range.
    fn scale(&self, value: f64) -> f64 {
        if self.min == self.max {
            return 0.0;
        }

        (value - self.min) / (self.max - self.min)
    }
}

#[cfg(test)]
mod tests {
    use spectre::edge::Edge;

    use super::*;

    #[test]
    fn normalization_factors_determine_test() {
        let list = vec![1, 2, 3, 4, 5];
        let factors = NormalizationFactors::determine(&list);

        assert_eq!(factors.min, 1.0);
        assert_eq!(factors.max, 5.0);
    }

    #[test]
    fn normalization_factors_lerp_test() {
        let factors = NormalizationFactors { min: 1.0, max: 5.0 };
        let value = 3.0;

        assert_eq!(factors.scale(value), 0.5);
    }

    #[test]
    fn normalization_factors_lerp_divide_zero_test() {
        let factors = NormalizationFactors { min: 2.0, max: 2.0 };
        let value = 3.0;

        assert_eq!(factors.scale(value), 0.0);
    }

    #[tokio::test]
    async fn detect_islands_test_no_islands() {
        let mut graph = Graph::new();
        let mut nodes = Vec::new();
        let mut ipaddrs = Vec::new();
        let ips_config = IPSConfiguration::default();
        let ips = Ips::new(ips_config);

        for i in 0..10 {
            let ip = format!("192.168.0.{i}");

            ipaddrs.push(IpAddr::from_str(ip.as_str()).expect(ERR_PARSE_IP));

            let node = Node {
                ip: ip.clone(),
                ..Default::default()
            };
            nodes.push(node);
        }

        // Case where each node is connected to all other nodes
        for i in 0..10 {
            for j in 0..10 {
                if i == j {
                    continue;
                }
                graph.insert(Edge::new(
                    IpAddr::from_str(nodes[i].ip.as_str()).expect(ERR_PARSE_IP),
                    IpAddr::from_str(nodes[j].ip.as_str()).expect(ERR_PARSE_IP),
                ));
            }
        }

        let agraph = graph.create_agraph(&ipaddrs);
        let islands = ips.detect_islands(&agraph);

        assert_eq!(islands.len(), 1);
    }

    #[tokio::test]
    async fn detect_islands_test() {
        let mut graph = Graph::new();
        let mut nodes = Vec::new();
        let mut ipaddrs = Vec::new();
        let ips_config = IPSConfiguration::default();
        let ips = Ips::new(ips_config);

        for i in 0..10 {
            let ip = format!("192.169.0.{i}");

            ipaddrs.push(IpAddr::from_str(ip.as_str()).expect(ERR_PARSE_IP));

            let node = Node {
                ip: ip.clone(),
                ..Default::default()
            };
            nodes.push(node);
        }

        // Each node is connected only to itself - each node is an island
        for i in 0..10 {
            for j in 0..10 {
                if i != j {
                    continue;
                }
                graph.insert(Edge::new(
                    IpAddr::from_str(nodes[i].ip.as_str()).expect(ERR_PARSE_IP),
                    IpAddr::from_str(nodes[j].ip.as_str()).expect(ERR_PARSE_IP),
                ));
            }
        }

        let agraph = graph.create_agraph(&ipaddrs);
        let islands = ips.detect_islands(&agraph);

        assert_eq!(islands.len(), nodes.len());
    }
}
