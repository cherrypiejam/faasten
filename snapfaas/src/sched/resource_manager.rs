//! This resource manager maintains a global resource
//! state across worker nodes.

use std::net::{IpAddr, SocketAddr};
use std::collections::HashMap;
use std::sync::mpsc::Sender;
use uuid::Uuid;

use super::rpc::ResourceInfo;
use super::Task;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct Node(IpAddr);

#[derive(Debug)]
pub struct NodeInfo {
    pub node: Node,
    total_mem: usize,
    free_mem: usize,
    dirty: bool,
}

impl NodeInfo {
    fn new(node: Node) -> Self {
        NodeInfo {
            node,
            dirty: false,
            total_mem: Default::default(),
            free_mem: Default::default(),
        }
    }

    fn dirty(&self) -> bool {
        self.dirty
    }

    fn set_dirty(&mut self, v: bool) {
        self.dirty = v;
    }
}

// type WorkerId = u64;
#[derive(Debug)]
pub struct Worker {
    // pub id: WorkerId,
    pub addr: SocketAddr,
    pub sender: Sender<Task>,
}

/// Global resource manager
#[derive(Debug, Default)]
pub struct ResourceManager {
    // TODO garbage collection
    pub info: HashMap<Node, NodeInfo>,
    // Locations of cached VMs for a function
    pub cached: HashMap<String, Vec<(Node, usize)>>,
    // If no idle workers, we simply remove the entry out of
    // the hashmap, which is why we need another struct to store info
    pub idle: HashMap<Node, Vec<Worker>>,
    // for sync invoke
    pub wait_list: HashMap<Uuid, Sender<String>>,
}

impl ResourceManager {
    pub fn new() -> Self {
        ResourceManager {
            ..Default::default()
        }
    }

    pub fn add_idle(&mut self, addr: SocketAddr, sender: Sender<Task>) {
        let node = Node(addr.ip());
        self.try_add_node(&node);
        let worker = Worker { addr, sender };
        let idle = &mut self.idle;
        if let Some(v) = idle.get_mut(&node) {
            v.push(worker);
        } else {
            idle.insert(node, vec![worker]);
        }
    }

    pub fn find_idle(&mut self, function: &String) -> Option<Worker> {
        let info = &self.info;
        let node = self.cached
                    .get_mut(function)
                    .and_then(|v| {
                        let fst = v
                            .iter_mut()
                            // Find the first safe node
                            .find(|n| {
                                let i = info.get(&n.0).unwrap();
                                !i.dirty()
                            })
                            // Update cached number for this node
                            // because we are going to use one of
                            // it's idle workers. A cached VM always
                            // implies an idle worker, but not the opposite
                            .map(|n| {
                                n.1 -= 1;
                                n.0.clone()
                            });
                        // Remove the entry if no more cached VM remains
                        v.retain(|n| n.1 != 0);
                        fst
                    });
        // Find idle worker
        // FIXME assume that all workers can handle any function
        match node {
            Some(n) => {
                let worker = self.idle
                                .get_mut(&n)
                                .and_then(|v| v.pop());
                self.idle.retain(|_, v| !v.is_empty());
                log::debug!("find cached {:?}", worker);
                worker
            }
            None => {
                log::debug!("no cached {:?}", self.cached);
                // If no cached, simply return some worker
                let worker = self.idle
                                .values_mut()
                                .next()
                                .and_then(|v| v.pop());
                // Mark the node dirty because it may or may not have
                // the same cached functions. This indicates an implicit
                // eviction on the remote worker node, thus we can't
                // make further decisions based on it unless confirmed
                if let Some(w) = worker.as_ref() {
                    let addr = w.addr.ip();
                    let node = Node(addr);
                    self.info
                        .get_mut(&node)
                        .unwrap()
                        .set_dirty(true);
                }
                // Remove the entry if no more idle remains
                self.idle.retain(|_, v| !v.is_empty());
                worker
            }
        }
    }

    pub fn reset(&mut self) {
        for (_, workers) in self.idle.iter_mut() {
            while let Some(w) = workers.pop() {
                let _ = w.sender.send(Task::Terminate);
            }
        }
        self.idle.retain(|_, v| !v.is_empty());
        // TODO Only workers get killed, meaning that
        // local resource menagers are still alive after this
        // self.cached.retain(|_, _| false);
        // (self.total_mem, self.total_num_vms) = (0, 0);
    }

    pub fn update(&mut self, addr: IpAddr, info: ResourceInfo) {
        log::debug!("update {:?}", info);
        let node = Node(addr);

        // Set node to not dirty bc we are sure of its state
        let success = self.try_add_node(&node);
        if !success {
            self.info
                .get_mut(&node)
                .unwrap()
                .set_dirty(false);
        }

        // Update mem info as well
        let nodeinfo = self.info
                            .get_mut(&node)
                            .unwrap();
        nodeinfo.total_mem = info.total_mem;
        nodeinfo.free_mem = info.free_mem;

        // Update number of cached VMs per funciton
        for (f, num_cached) in info.stats.into_iter() {
            let nodes = self.cached.get_mut(&f);
            match nodes {
                Some(nodes) => {
                    let n = nodes
                            .iter_mut()
                            .find(|&&mut n| n.0 == node);
                    if let Some(n) = n {
                        n.1 = num_cached;
                    } else {
                        nodes.push((node.clone(), num_cached));
                    }
                    nodes.retain(|n| n.1 > 0);
                }
                None => {
                    if num_cached > 0 {
                        let f = f.clone();
                        let v = vec![(node.clone(), num_cached)];
                        let _ = self.cached.insert(f, v);
                    }
                }
            }
        }
    }

    pub fn remove(&mut self, addr: IpAddr) {
        let node = Node(addr);
        // They must have no idle worker
        for (_, v) in self.cached.iter_mut() {
            if let Some(pos) = v.iter().position(|&n| n.0 == node) {
                // This doesn't preserve ordering
                v.swap_remove(pos);
            }
        }
        self.info.remove(&node);
        self.idle.remove(&node);
    }

    fn try_add_node(&mut self, node: &Node) -> bool {
        let has_node = self.info.contains_key(&node);
        if !has_node {
            self.info.insert(
                node.clone(),
                NodeInfo::new(node.clone())
            );
        }
        !has_node
    }
}