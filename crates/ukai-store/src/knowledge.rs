//! The notes a gateway's agents and their people write about the code they
//! work on (see "ナレッジ" in `docs/design.md`): [`store`] keeps them in
//! SurrealDB, and [`service`] answers `ukai.knowledge.v1.KnowledgeStore`.

pub mod service;
pub mod store;
