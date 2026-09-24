//! Plan hashing (`docs/engine/plan.md` §4).
//!
//! Each node's hash covers its operator and, through its input's hash, every
//! step before it: a Merkle chain over the DAG. It doesn't depend on how the
//! nodes are numbered, so the same computation written in a different order
//! has the same hash, and a node's hash can key caches of its result.

use crate::json::op_to_json;
use crate::plan::Plan;
use receipts_core::ContentHash;
use receipts_core::canonical_json::to_canonical_json;
use receipts_core::hash::ContentHasher;
use serde_json::json;

pub const NODE_CONTEXT: &str = "receipts plan v1 node";

/// `H(canonical JSON of the node, with "input" replaced by the input's hash
/// in hex)`, for every node in order.
pub(crate) fn node_hashes(plan: &Plan) -> Vec<ContentHash> {
    let mut hashes: Vec<ContentHash> = Vec::with_capacity(plan.nodes.len());
    for op in &plan.nodes {
        let input = op.input().map(|i| json!(hashes[i.index()].to_hex()));
        let canonical = to_canonical_json(&op_to_json(op, input))
            .expect("plan JSON has no integers beyond 2^53 and no non-finite floats");
        let mut h = ContentHasher::new(NODE_CONTEXT);
        h.bytes(canonical.as_bytes());
        hashes.push(h.finish());
    }
    hashes
}
