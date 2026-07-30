//! Planner, optimizer, and vectorized executor for Pintail.

mod logical;

pub use logical::{LogicalPlan, LogicalPlanner, Scan};
