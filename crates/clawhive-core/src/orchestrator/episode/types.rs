use clawhive_memory::{EpisodeTaskStateRecord, RecentExplicitMemoryWrite, SessionMessage};

#[derive(Debug, Clone)]
pub(in crate::orchestrator) struct BoundaryFlushEpisode {
    pub(in crate::orchestrator) start_turn: u64,
    pub(in crate::orchestrator) end_turn: u64,
    pub(in crate::orchestrator) messages: Vec<SessionMessage>,
}

#[derive(Debug, Clone)]
pub(in crate::orchestrator) struct BoundaryFlushSnapshot {
    pub(in crate::orchestrator) episodes: Vec<BoundaryFlushEpisode>,
    pub(in crate::orchestrator) turn_count: u64,
    pub(in crate::orchestrator) recent_explicit_writes: Vec<RecentExplicitMemoryWrite>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::orchestrator) enum EpisodeBoundaryDecision {
    ContinueCurrent,
    CloseCurrentAndOpenNext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::orchestrator) struct EpisodeTurnDecision {
    pub(in crate::orchestrator) task_state: EpisodeTaskStateRecord,
    pub(in crate::orchestrator) boundary: EpisodeBoundaryDecision,
}

pub(in crate::orchestrator) struct EpisodeTurnInput<'a> {
    pub(in crate::orchestrator) turn_index: u64,
    pub(in crate::orchestrator) user_text: &'a str,
    pub(in crate::orchestrator) assistant_text: &'a str,
    pub(in crate::orchestrator) successful_tool_calls: usize,
    pub(in crate::orchestrator) final_stop_reason: Option<&'a str>,
}
