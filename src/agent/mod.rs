//! Agent layer: loop, session store, compaction, prompts. (Grown across
//! phases 3-5.)

pub mod compaction;
pub mod prompts;
pub mod session;
pub mod loop_;

pub use loop_::{AgentLoop, TurnResult};
