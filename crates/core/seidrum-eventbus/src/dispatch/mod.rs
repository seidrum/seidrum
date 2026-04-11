//! Dispatch layer: routes published events to subscribers.
//!
//! - [`engine`] — the [`DispatchEngine`] that runs the publish pipeline.
//! - [`subject_index`] — the trie-backed [`SubscriptionEntry`] index used
//!   to look up subscribers by subject pattern.
//! - [`filter`] — [`EventFilter`]s for narrowing subscriptions by JSON
//!   field predicates.
//! - [`interceptor`] — the [`Interceptor`] trait for sync, mutating
//!   handlers in the sync chain.

pub mod engine;
pub mod filter;
pub mod interceptor;
pub mod subject_index;

pub use engine::{DispatchEngine, SubscriptionInfo};
pub use filter::EventFilter;
pub use interceptor::{InterceptResult, Interceptor};
pub use subject_index::{SubscriptionEntry, SubscriptionMode};
