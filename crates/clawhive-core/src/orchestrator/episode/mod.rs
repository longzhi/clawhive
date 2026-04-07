mod boundary;
mod flush;
mod tracking;
mod types;

pub(crate) use boundary::find_boundary_flush_conflict;
pub(super) use types::EpisodeTurnInput;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{TimeZone, Utc};
    use clawhive_memory::{
        EpisodeStateRecord, EpisodeStatusRecord, EpisodeTaskStateRecord, MemoryStore,
        RecentExplicitMemoryWrite, SessionEntry, SessionMemoryStateRecord, SessionMessage,
        SessionRecord, SessionWriter,
    };
    use clawhive_schema::*;

    use crate::orchestrator::summary::fact_token_overlap_ratio;
    use crate::orchestrator::test_helpers::{
        make_file_backed_test_orchestrator, test_full_agent, FailingEmbeddingProvider,
    };
    use crate::orchestrator::Orchestrator;
    use crate::session::Session;

    use super::boundary::{
        boundary_flush_conflict_passes_two_step, boundary_flush_topic_tokens_from_text,
        collect_unflushed_boundary_episodes, collect_unflushed_boundary_turns, decide_episode_turn,
        infer_episode_task_state, MAX_OPEN_EPISODE_TURNS,
    };
    use super::types::{EpisodeBoundaryDecision, EpisodeTurnInput};
    use super::*;

    #[test]
    fn collect_unflushed_boundary_episodes_only_returns_turns_after_checkpoint() {
        let entries = vec![
            SessionEntry::Message {
                id: "m1".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 0, 0).unwrap(),
                message: SessionMessage {
                    role: "user".to_string(),
                    content: "first user".to_string(),
                    timestamp: None,
                },
            },
            SessionEntry::Message {
                id: "m2".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 0, 1).unwrap(),
                message: SessionMessage {
                    role: "assistant".to_string(),
                    content: "first reply".to_string(),
                    timestamp: None,
                },
            },
            SessionEntry::Message {
                id: "m3".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 1, 0).unwrap(),
                message: SessionMessage {
                    role: "user".to_string(),
                    content: "second user".to_string(),
                    timestamp: None,
                },
            },
            SessionEntry::Message {
                id: "m4".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 1, 1).unwrap(),
                message: SessionMessage {
                    role: "assistant".to_string(),
                    content: "second reply".to_string(),
                    timestamp: None,
                },
            },
        ];

        let (episodes, turn_count) =
            collect_unflushed_boundary_episodes(entries, 1).expect("snapshot");

        assert_eq!(turn_count, 2);
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].start_turn, 2);
        assert_eq!(episodes[0].end_turn, 2);
        assert_eq!(episodes[0].messages.len(), 2);
        assert_eq!(episodes[0].messages[0].content, "second user");
        assert_eq!(episodes[0].messages[1].content, "second reply");
    }

    #[test]
    fn collect_unflushed_boundary_episodes_groups_related_turns() {
        let entries = vec![
            SessionEntry::Message {
                id: "m1".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 0, 0).unwrap(),
                message: SessionMessage {
                    role: "user".to_string(),
                    content: "How do I use Rust Vec push?".to_string(),
                    timestamp: None,
                },
            },
            SessionEntry::Message {
                id: "m2".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 0, 1).unwrap(),
                message: SessionMessage {
                    role: "assistant".to_string(),
                    content: "Use Vec::push to append items.".to_string(),
                    timestamp: None,
                },
            },
            SessionEntry::Message {
                id: "m3".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 1, 0).unwrap(),
                message: SessionMessage {
                    role: "user".to_string(),
                    content: "What about Rust Vec insert?".to_string(),
                    timestamp: None,
                },
            },
            SessionEntry::Message {
                id: "m4".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 1, 1).unwrap(),
                message: SessionMessage {
                    role: "assistant".to_string(),
                    content: "Use Vec::insert for indexed insertion.".to_string(),
                    timestamp: None,
                },
            },
        ];

        let (episodes, turn_count) =
            collect_unflushed_boundary_episodes(entries, 0).expect("snapshot");

        assert_eq!(turn_count, 2);
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].start_turn, 1);
        assert_eq!(episodes[0].end_turn, 2);
        assert_eq!(episodes[0].messages.len(), 4);
    }

    #[test]
    fn collect_unflushed_boundary_turns_does_not_truncate_long_unflushed_history() {
        let mut entries = Vec::new();
        for turn in 1..=60 {
            entries.push(SessionEntry::Message {
                id: format!("u-{turn}"),
                timestamp: Utc::now(),
                message: SessionMessage {
                    role: "user".to_string(),
                    content: format!("user turn {turn}"),
                    timestamp: None,
                },
            });
            entries.push(SessionEntry::Message {
                id: format!("a-{turn}"),
                timestamp: Utc::now(),
                message: SessionMessage {
                    role: "assistant".to_string(),
                    content: format!("assistant turn {turn}"),
                    timestamp: None,
                },
            });
        }

        let (turns, turn_count) = collect_unflushed_boundary_turns(entries, 0).expect("snapshot");
        assert_eq!(turn_count, 60);
        assert_eq!(turns.len(), 60);
        assert_eq!(turns.first().map(|turn| turn.start_turn), Some(1));
        assert_eq!(turns.last().map(|turn| turn.end_turn), Some(60));
    }

    #[tokio::test]
    async fn record_session_turn_episode_merges_related_turns_into_same_open_episode() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-1";
        let session_key = "telegram:tg:chat:episode-1";
        let agent_id = "agent-a";

        let (orchestrator, memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 0,
        };

        orchestrator
            .record_session_turn_episode(
                agent_id,
                &session,
                EpisodeTurnInput {
                    turn_index: 1,
                    user_text: "How do I use Rust Vec push?",
                    assistant_text: "Use Vec::push to append items.",
                    successful_tool_calls: 0,
                    final_stop_reason: Some("end_turn"),
                },
            )
            .await;
        orchestrator
            .record_session_turn_episode(
                agent_id,
                &session,
                EpisodeTurnInput {
                    turn_index: 2,
                    user_text: "What about Rust Vec insert?",
                    assistant_text: "Use Vec::insert for indexed insertion.",
                    successful_tool_calls: 0,
                    final_stop_reason: Some("end_turn"),
                },
            )
            .await;

        let state = memory
            .get_session_memory_state(agent_id, session_id)
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.open_episodes.len(), 1);
        let episode = &state.open_episodes[0];
        assert_eq!(episode.start_turn, 1);
        assert_eq!(episode.end_turn, 2);
        assert_eq!(episode.status, EpisodeStatusRecord::Open);
        assert_eq!(episode.task_state, EpisodeTaskStateRecord::Delivered);
    }

    #[tokio::test]
    async fn record_session_turn_episode_closes_previous_episode_on_topic_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-closure";
        let session_key = "telegram:tg:chat:episode-closure";
        let agent_id = "agent-a";

        let (orchestrator, memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 0,
        };

        orchestrator
            .record_session_turn_episode(
                agent_id,
                &session,
                EpisodeTurnInput {
                    turn_index: 1,
                    user_text: "How do I use Rust Vec push?",
                    assistant_text: "Use Vec::push to append items.",
                    successful_tool_calls: 0,
                    final_stop_reason: Some("end_turn"),
                },
            )
            .await;
        let closed = orchestrator
            .record_session_turn_episode(
                agent_id,
                &session,
                EpisodeTurnInput {
                    turn_index: 2,
                    user_text: "How do I inspect RunPod GPU usage?",
                    assistant_text: "Use nvidia-smi on the pod.",
                    successful_tool_calls: 0,
                    final_stop_reason: Some("end_turn"),
                },
            )
            .await
            .expect("closed episode");

        assert_eq!(closed.start_turn, 1);
        assert_eq!(closed.end_turn, 1);
        assert_eq!(closed.status, EpisodeStatusRecord::Closed);

        let state = memory
            .get_session_memory_state(agent_id, session_id)
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.open_episodes.len(), 2);
        assert_eq!(state.open_episodes[0].status, EpisodeStatusRecord::Closed);
        assert_eq!(state.open_episodes[1].status, EpisodeStatusRecord::Open);
        assert_eq!(state.open_episodes[1].start_turn, 2);
    }

    #[test]
    fn infer_episode_task_state_distinguishes_structural_delivery_states() {
        assert_eq!(
            infer_episode_task_state("好，让我把所有内容整合起来：", 0, Some("end_turn")),
            EpisodeTaskStateRecord::Executing
        );
        assert_eq!(
            infer_episode_task_state("我现在就整理给你", 0, Some("end_turn")),
            EpisodeTaskStateRecord::Exploring
        );
        assert_eq!(
            infer_episode_task_state("整理好了，答案如下。", 0, Some("end_turn")),
            EpisodeTaskStateRecord::Delivered
        );
        assert_eq!(
            infer_episode_task_state("我现在就整理给你", 1, Some("end_turn")),
            EpisodeTaskStateRecord::Delivered
        );
        assert_eq!(
            infer_episode_task_state("整理到一半", 0, Some("length")),
            EpisodeTaskStateRecord::Executing
        );
    }

    #[test]
    fn decide_episode_turn_keeps_related_topics_in_same_episode() {
        let current = boundary_flush_topic_tokens_from_text("rust vec push");
        let next = boundary_flush_topic_tokens_from_text("rust vec capacity");

        let decision = decide_episode_turn(
            &current,
            &next,
            "整理好了，答案如下。",
            0,
            Some("end_turn"),
            EpisodeTaskStateRecord::Delivered,
            1,
        );

        assert_eq!(decision.boundary, EpisodeBoundaryDecision::ContinueCurrent);
        assert_eq!(decision.task_state, EpisodeTaskStateRecord::Delivered);
    }

    #[test]
    fn decide_episode_turn_splits_unrelated_topics_and_tracks_runtime_state() {
        let current = boundary_flush_topic_tokens_from_text("rust vec push");
        let next = boundary_flush_topic_tokens_from_text("runpod gpu inspect");

        let decision = decide_episode_turn(
            &current,
            &next,
            "我现在就整理给你",
            1,
            Some("end_turn"),
            EpisodeTaskStateRecord::Delivered,
            1,
        );

        assert_eq!(
            decision.boundary,
            EpisodeBoundaryDecision::CloseCurrentAndOpenNext
        );
        assert_eq!(decision.task_state, EpisodeTaskStateRecord::Delivered);
    }

    #[test]
    fn decide_episode_turn_splits_when_current_episode_reaches_turn_cap() {
        let current = boundary_flush_topic_tokens_from_text("rust vec push");
        let next = boundary_flush_topic_tokens_from_text("rust vec capacity");

        let decision = decide_episode_turn(
            &current,
            &next,
            "继续补充说明。",
            0,
            Some("end_turn"),
            EpisodeTaskStateRecord::Delivered,
            MAX_OPEN_EPISODE_TURNS,
        );

        assert_eq!(
            decision.boundary,
            EpisodeBoundaryDecision::CloseCurrentAndOpenNext
        );
    }

    #[test]
    fn fact_token_overlap_requires_high_similarity() {
        let overlap = fact_token_overlap_ratio(
            "User prefers Rust for backend services",
            "User prefers Rust for backend systems",
        );
        assert!(overlap > 0.6);

        let low_overlap = fact_token_overlap_ratio(
            "User prefers Rust for backend services",
            "User moved to Tokyo last month",
        );
        assert!(low_overlap < 0.6);
    }

    #[test]
    fn boundary_flush_conflict_requires_same_type_and_embedding_signal() {
        let old_fact = clawhive_memory::fact_store::Fact {
            id: "old".to_string(),
            agent_id: "agent-1".to_string(),
            content: "User prefers Rust for backend services".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.7,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: Utc::now().to_rfc3339(),
            source_type: "boundary_flush".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        };
        let different_type = clawhive_memory::fact_store::Fact {
            fact_type: "decision".to_string(),
            ..old_fact.clone()
        };

        assert!(boundary_flush_conflict_passes_two_step(
            "User prefers Rust for backend systems",
            "preference",
            &old_fact,
            Some(0.9)
        ));
        assert!(!boundary_flush_conflict_passes_two_step(
            "User prefers Rust for backend systems",
            "preference",
            &different_type,
            Some(0.9)
        ));
        assert!(!boundary_flush_conflict_passes_two_step(
            "User prefers Rust for backend systems",
            "preference",
            &old_fact,
            None
        ));
        assert!(boundary_flush_conflict_passes_two_step(
            "User no longer uses Python for backend systems",
            "preference",
            &old_fact,
            Some(0.9)
        ));
    }

    #[tokio::test]
    async fn boundary_flush_conflict_check_fallbacks_to_insert_on_embedding_failure() {
        let old_fact = clawhive_memory::fact_store::Fact {
            id: "old".to_string(),
            agent_id: "agent-1".to_string(),
            content: "User prefers Rust for backend services".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.7,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: Utc::now().to_rfc3339(),
            source_type: "boundary_flush".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        };

        let provider: Arc<dyn clawhive_memory::embedding::EmbeddingProvider> =
            Arc::new(FailingEmbeddingProvider);
        let conflict = find_boundary_flush_conflict(
            &provider,
            "User prefers Rust for backend systems",
            "preference",
            &[old_fact],
        )
        .await
        .unwrap_or_default();

        assert!(conflict.is_none());
    }

    #[tokio::test]
    async fn record_session_turn_episode_marks_open_episode_executing_for_structural_promise() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-task-state";
        let session_key = "telegram:tg:chat:episode-task-state";
        let agent_id = "agent-a";

        let (orchestrator, memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 0,
        };

        orchestrator
            .record_session_turn_episode(
                agent_id,
                &session,
                EpisodeTurnInput {
                    turn_index: 1,
                    user_text: "请整理 memory 重构方案",
                    assistant_text: "好，让我把所有内容整合起来：",
                    successful_tool_calls: 0,
                    final_stop_reason: Some("end_turn"),
                },
            )
            .await;

        let state = memory
            .get_session_memory_state(agent_id, session_id)
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.open_episodes.len(), 1);
        assert_eq!(
            state.open_episodes[0].task_state,
            EpisodeTaskStateRecord::Executing
        );
    }

    #[tokio::test]
    async fn record_session_turn_episode_marks_inconclusive_reply_delivered_after_tool_execution() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-task-state-tools";
        let session_key = "telegram:tg:chat:episode-task-state-tools";
        let agent_id = "agent-a";

        let (orchestrator, memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 0,
        };

        orchestrator
            .record_session_turn_episode(
                agent_id,
                &session,
                EpisodeTurnInput {
                    turn_index: 1,
                    user_text: "请帮我检查 GPU 状态",
                    assistant_text: "我现在就整理给你",
                    successful_tool_calls: 1,
                    final_stop_reason: Some("end_turn"),
                },
            )
            .await;

        let state = memory
            .get_session_memory_state(agent_id, session_id)
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.open_episodes.len(), 1);
        assert_eq!(
            state.open_episodes[0].task_state,
            EpisodeTaskStateRecord::Delivered
        );
    }

    #[tokio::test]
    async fn boundary_flush_snapshot_prefers_persisted_open_episode_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-2";
        let session_key = "telegram:tg:chat:episode-2";
        let agent_id = "agent-a";

        {
            let store = MemoryStore::open(db_path.to_str().expect("db path")).unwrap();
            store
                .upsert_session(SessionRecord {
                    session_key: session_key.to_string(),
                    session_id: session_id.to_string(),
                    agent_id: agent_id.to_string(),
                    created_at: Utc::now(),
                    last_active: Utc::now(),
                    ttl_seconds: 1800,
                    interaction_count: 2,
                })
                .await
                .unwrap();
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    session_key: session_key.to_string(),
                    last_flushed_turn: 0,
                    last_boundary_flush_at: None,
                    pending_flush: false,
                    flush_phase: "idle".to_string(),
                    flush_phase_updated_at: None,
                    flush_summary_cache: None,
                    recent_explicit_writes: Vec::new(),
                    open_episodes: vec![EpisodeStateRecord {
                        episode_id: format!("{session_id}:1"),
                        start_turn: 1,
                        end_turn: 2,
                        status: EpisodeStatusRecord::Open,
                        task_state: EpisodeTaskStateRecord::Delivered,
                        topic_sketch: "rust vec".to_string(),
                        last_activity_at: Utc::now(),
                    }],
                })
                .await
                .unwrap();
        }

        let writer = SessionWriter::new(tmp.path());
        writer
            .append_message(session_id, "user", "How do I use Rust Vec push?")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "Use Vec::push to append items.")
            .await
            .unwrap();
        writer
            .append_message(session_id, "user", "Completely different new task")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "Handled the unrelated task.")
            .await
            .unwrap();

        let (orchestrator, _memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 2,
        };

        let snapshot = orchestrator
            .capture_boundary_flush_snapshot(agent_id, &session, &test_full_agent(agent_id))
            .await
            .expect("snapshot");

        assert_eq!(snapshot.episodes.len(), 1);
        assert_eq!(snapshot.episodes[0].start_turn, 1);
        assert_eq!(snapshot.episodes[0].end_turn, 2);
        assert_eq!(snapshot.episodes[0].messages.len(), 4);
    }

    #[tokio::test]
    async fn boundary_flush_snapshot_ignores_already_flushed_episode_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-3";
        let session_key = "telegram:tg:chat:episode-3";
        let agent_id = "agent-a";

        {
            let store = MemoryStore::open(db_path.to_str().expect("db path")).unwrap();
            store
                .upsert_session(SessionRecord {
                    session_key: session_key.to_string(),
                    session_id: session_id.to_string(),
                    agent_id: agent_id.to_string(),
                    created_at: Utc::now(),
                    last_active: Utc::now(),
                    ttl_seconds: 1800,
                    interaction_count: 2,
                })
                .await
                .unwrap();
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    session_key: session_key.to_string(),
                    last_flushed_turn: 1,
                    last_boundary_flush_at: Some(Utc::now()),
                    pending_flush: false,
                    flush_phase: "idle".to_string(),
                    flush_phase_updated_at: None,
                    flush_summary_cache: None,
                    recent_explicit_writes: Vec::new(),
                    open_episodes: vec![
                        EpisodeStateRecord {
                            episode_id: format!("{session_id}:1"),
                            start_turn: 1,
                            end_turn: 1,
                            status: EpisodeStatusRecord::Flushed,
                            task_state: EpisodeTaskStateRecord::Delivered,
                            topic_sketch: "rust vec".to_string(),
                            last_activity_at: Utc::now(),
                        },
                        EpisodeStateRecord {
                            episode_id: format!("{session_id}:2"),
                            start_turn: 2,
                            end_turn: 2,
                            status: EpisodeStatusRecord::Open,
                            task_state: EpisodeTaskStateRecord::Delivered,
                            topic_sketch: "runpod gpu".to_string(),
                            last_activity_at: Utc::now(),
                        },
                    ],
                })
                .await
                .unwrap();
        }

        let writer = SessionWriter::new(tmp.path());
        writer
            .append_message(session_id, "user", "How do I use Rust Vec push?")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "Use Vec::push to append items.")
            .await
            .unwrap();
        writer
            .append_message(session_id, "user", "How do I inspect RunPod GPU usage?")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "Use nvidia-smi on the pod.")
            .await
            .unwrap();

        let (orchestrator, _memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 2,
        };

        let snapshot = orchestrator
            .capture_boundary_flush_snapshot(agent_id, &session, &test_full_agent(agent_id))
            .await
            .expect("snapshot");

        assert_eq!(snapshot.episodes.len(), 1);
        assert_eq!(snapshot.episodes[0].start_turn, 2);
        assert_eq!(snapshot.episodes[0].end_turn, 2);
        assert_eq!(snapshot.turn_count, 2);
    }

    #[tokio::test]
    async fn boundary_flush_snapshot_skips_recent_flush_pending_episode_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-pending";
        let session_key = "telegram:tg:chat:episode-pending";
        let agent_id = "agent-a";

        {
            let store = MemoryStore::open(db_path.to_str().expect("db path")).unwrap();
            store
                .upsert_session(SessionRecord {
                    session_key: session_key.to_string(),
                    session_id: session_id.to_string(),
                    agent_id: agent_id.to_string(),
                    created_at: Utc::now(),
                    last_active: Utc::now(),
                    ttl_seconds: 1800,
                    interaction_count: 2,
                })
                .await
                .unwrap();
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    session_key: session_key.to_string(),
                    last_flushed_turn: 0,
                    last_boundary_flush_at: None,
                    pending_flush: false,
                    flush_phase: "idle".to_string(),
                    flush_phase_updated_at: None,
                    flush_summary_cache: None,
                    recent_explicit_writes: Vec::new(),
                    open_episodes: vec![
                        EpisodeStateRecord {
                            episode_id: format!("{session_id}:1"),
                            start_turn: 1,
                            end_turn: 1,
                            status: EpisodeStatusRecord::FlushPending,
                            task_state: EpisodeTaskStateRecord::Delivered,
                            topic_sketch: "rust vec".to_string(),
                            last_activity_at: Utc::now(),
                        },
                        EpisodeStateRecord {
                            episode_id: format!("{session_id}:2"),
                            start_turn: 2,
                            end_turn: 2,
                            status: EpisodeStatusRecord::Closed,
                            task_state: EpisodeTaskStateRecord::Delivered,
                            topic_sketch: "runpod gpu".to_string(),
                            last_activity_at: Utc::now(),
                        },
                    ],
                })
                .await
                .unwrap();
        }

        let writer = SessionWriter::new(tmp.path());
        writer
            .append_message(session_id, "user", "How do I use Rust Vec push?")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "Use Vec::push to append items.")
            .await
            .unwrap();
        writer
            .append_message(session_id, "user", "How do I inspect RunPod GPU usage?")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "Use nvidia-smi on the pod.")
            .await
            .unwrap();

        let (orchestrator, _memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 2,
        };

        let snapshot = orchestrator
            .capture_boundary_flush_snapshot(agent_id, &session, &test_full_agent(agent_id))
            .await
            .expect("snapshot");

        assert_eq!(snapshot.episodes.len(), 1);
        assert_eq!(snapshot.episodes[0].start_turn, 2);
        assert_eq!(snapshot.episodes[0].end_turn, 2);
        assert_eq!(snapshot.turn_count, 2);
    }

    #[tokio::test]
    async fn session_end_schedule_closes_open_episodes_before_flush() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-session-end";
        let session_key = "telegram:tg:chat:episode-session-end";
        let agent_id = "agent-a";

        {
            let store = MemoryStore::open(db_path.to_str().expect("db path")).unwrap();
            store
                .upsert_session(SessionRecord {
                    session_key: session_key.to_string(),
                    session_id: session_id.to_string(),
                    agent_id: agent_id.to_string(),
                    created_at: Utc::now(),
                    last_active: Utc::now(),
                    ttl_seconds: 1800,
                    interaction_count: 2,
                })
                .await
                .unwrap();
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    session_key: session_key.to_string(),
                    last_flushed_turn: 0,
                    last_boundary_flush_at: None,
                    pending_flush: false,
                    flush_phase: "idle".to_string(),
                    flush_phase_updated_at: None,
                    flush_summary_cache: None,
                    recent_explicit_writes: Vec::new(),
                    open_episodes: vec![
                        EpisodeStateRecord {
                            episode_id: format!("{session_id}:1"),
                            start_turn: 1,
                            end_turn: 1,
                            status: EpisodeStatusRecord::Closed,
                            task_state: EpisodeTaskStateRecord::Delivered,
                            topic_sketch: "rust vec".to_string(),
                            last_activity_at: Utc::now(),
                        },
                        EpisodeStateRecord {
                            episode_id: format!("{session_id}:2"),
                            start_turn: 2,
                            end_turn: 2,
                            status: EpisodeStatusRecord::Open,
                            task_state: EpisodeTaskStateRecord::Exploring,
                            topic_sketch: "runpod gpu".to_string(),
                            last_activity_at: Utc::now(),
                        },
                    ],
                })
                .await
                .unwrap();
        }

        let writer = SessionWriter::new(tmp.path());
        writer
            .append_message(session_id, "user", "How do I use Rust Vec push?")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "Use Vec::push to append items.")
            .await
            .unwrap();
        writer
            .append_message(session_id, "user", "How do I inspect RunPod GPU usage?")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "I need to check more details.")
            .await
            .unwrap();

        let (orchestrator, memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 2,
        };
        let agent = test_full_agent(agent_id);
        let view = orchestrator.config_view();

        orchestrator
            .schedule_session_end_flush(view.as_ref(), agent_id, &session, &agent)
            .await;

        let state = memory
            .get_session_memory_state(agent_id, session_id)
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.open_episodes.len(), 2);
        assert!(
            state
                .open_episodes
                .iter()
                .all(|episode| episode.status != EpisodeStatusRecord::Open),
            "session-end scheduling should close all open episodes before flush"
        );
    }

    #[tokio::test]
    async fn close_open_episodes_for_session_end_marks_remaining_open_episodes_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-4";
        let session_key = "telegram:tg:chat:episode-4";
        let agent_id = "agent-a";

        let memory = Arc::new(MemoryStore::open(db_path.to_str().expect("db path")).unwrap());
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 2,
        };

        memory
            .upsert_session_memory_state(SessionMemoryStateRecord {
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                session_key: session_key.to_string(),
                last_flushed_turn: 0,
                last_boundary_flush_at: None,
                pending_flush: false,
                flush_phase: "idle".to_string(),
                flush_phase_updated_at: None,
                flush_summary_cache: None,
                recent_explicit_writes: Vec::new(),
                open_episodes: vec![
                    EpisodeStateRecord {
                        episode_id: format!("{session_id}:1"),
                        start_turn: 1,
                        end_turn: 1,
                        status: EpisodeStatusRecord::Open,
                        task_state: EpisodeTaskStateRecord::Executing,
                        topic_sketch: "memory".to_string(),
                        last_activity_at: Utc::now(),
                    },
                    EpisodeStateRecord {
                        episode_id: format!("{session_id}:2"),
                        start_turn: 2,
                        end_turn: 2,
                        status: EpisodeStatusRecord::Closed,
                        task_state: EpisodeTaskStateRecord::Delivered,
                        topic_sketch: "runpod".to_string(),
                        last_activity_at: Utc::now(),
                    },
                    EpisodeStateRecord {
                        episode_id: format!("{session_id}:3"),
                        start_turn: 3,
                        end_turn: 3,
                        status: EpisodeStatusRecord::Flushed,
                        task_state: EpisodeTaskStateRecord::Delivered,
                        topic_sketch: "obsidian".to_string(),
                        last_activity_at: Utc::now(),
                    },
                ],
            })
            .await
            .unwrap();

        Orchestrator::close_open_episodes_for_session_end(&memory, agent_id, &session).await;

        let state = memory
            .get_session_memory_state(agent_id, session_id)
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.open_episodes.len(), 3);
        assert_eq!(state.open_episodes[0].status, EpisodeStatusRecord::Closed);
        assert_eq!(state.open_episodes[1].status, EpisodeStatusRecord::Closed);
        assert_eq!(state.open_episodes[2].status, EpisodeStatusRecord::Flushed);
    }

    #[tokio::test]
    async fn update_closed_episode_flush_state_reverts_pending_episode_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-failure";
        let session_key = "telegram:tg:chat:episode-failure";
        let agent_id = "agent-a";

        let memory = Arc::new(MemoryStore::open(db_path.to_str().expect("db path")).unwrap());
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 1,
        };

        memory
            .upsert_session_memory_state(SessionMemoryStateRecord {
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                session_key: session_key.to_string(),
                last_flushed_turn: 0,
                last_boundary_flush_at: None,
                pending_flush: false,
                flush_phase: "idle".to_string(),
                flush_phase_updated_at: None,
                flush_summary_cache: None,
                recent_explicit_writes: Vec::new(),
                open_episodes: vec![EpisodeStateRecord {
                    episode_id: format!("{session_id}:1"),
                    start_turn: 1,
                    end_turn: 1,
                    status: EpisodeStatusRecord::FlushPending,
                    task_state: EpisodeTaskStateRecord::Delivered,
                    topic_sketch: "memory".to_string(),
                    last_activity_at: Utc::now(),
                }],
            })
            .await
            .unwrap();

        Orchestrator::update_closed_episode_flush_state(
            &memory,
            agent_id,
            &session,
            &format!("{session_id}:1"),
            false,
        )
        .await;

        let state = memory
            .get_session_memory_state(agent_id, session_id)
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.open_episodes.len(), 1);
        assert_eq!(state.open_episodes[0].status, EpisodeStatusRecord::Closed);
    }

    #[tokio::test]
    async fn boundary_flush_snapshot_resumes_from_persisted_checkpoint_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-1";
        let session_key = "telegram:tg:chat:1";
        let agent_id = "agent-a";

        {
            let store = MemoryStore::open(db_path.to_str().expect("db path")).unwrap();
            store
                .upsert_session(SessionRecord {
                    session_key: session_key.to_string(),
                    session_id: session_id.to_string(),
                    agent_id: agent_id.to_string(),
                    created_at: Utc::now(),
                    last_active: Utc::now(),
                    ttl_seconds: 1800,
                    interaction_count: 2,
                })
                .await
                .unwrap();
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    session_key: session_key.to_string(),
                    last_flushed_turn: 1,
                    last_boundary_flush_at: Some(Utc::now()),
                    pending_flush: false,
                    flush_phase: "idle".to_string(),
                    flush_phase_updated_at: None,
                    flush_summary_cache: None,
                    recent_explicit_writes: Vec::new(),
                    open_episodes: Vec::new(),
                })
                .await
                .unwrap();
            drop(store);
        }

        let writer = SessionWriter::new(tmp.path());
        writer
            .append_message(session_id, "user", "first user")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "first reply")
            .await
            .unwrap();
        writer
            .append_message(session_id, "user", "second user")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "second reply")
            .await
            .unwrap();

        let (orchestrator, _memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 2,
        };

        let snapshot = orchestrator
            .capture_boundary_flush_snapshot(agent_id, &session, &test_full_agent(agent_id))
            .await
            .expect("snapshot");

        assert_eq!(snapshot.turn_count, 2);
        assert_eq!(snapshot.episodes.len(), 1);
        assert_eq!(snapshot.episodes[0].start_turn, 2);
        assert_eq!(snapshot.episodes[0].end_turn, 2);
        assert_eq!(snapshot.episodes[0].messages.len(), 2);
        assert_eq!(snapshot.episodes[0].messages[0].content, "second user");
        assert_eq!(snapshot.episodes[0].messages[1].content, "second reply");
    }

    #[tokio::test]
    async fn explicit_memory_marker_survives_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-1";
        let session_key = "telegram:tg:chat:1";
        let agent_id = "agent-a";
        let recorded_at = Utc::now();

        {
            let store = MemoryStore::open(db_path.to_str().expect("db path")).unwrap();
            store
                .upsert_session(SessionRecord {
                    session_key: session_key.to_string(),
                    session_id: session_id.to_string(),
                    agent_id: agent_id.to_string(),
                    created_at: Utc::now(),
                    last_active: Utc::now(),
                    ttl_seconds: 1800,
                    interaction_count: 2,
                })
                .await
                .unwrap();
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    session_key: session_key.to_string(),
                    last_flushed_turn: 0,
                    last_boundary_flush_at: None,
                    pending_flush: false,
                    flush_phase: "idle".to_string(),
                    flush_phase_updated_at: None,
                    flush_summary_cache: None,
                    recent_explicit_writes: vec![RecentExplicitMemoryWrite {
                        turn_index: 1,
                        memory_ref: "fact-1".to_string(),
                        canonical_id: Some("canon-1".to_string()),
                        summary: "User prefers concise replies".to_string(),
                        recorded_at,
                    }],
                    open_episodes: Vec::new(),
                })
                .await
                .unwrap();
            drop(store);
        }

        let writer = SessionWriter::new(tmp.path());
        writer
            .append_message(session_id, "user", "first user")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "first reply")
            .await
            .unwrap();

        let (orchestrator, _memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 1,
        };

        let snapshot = orchestrator
            .capture_boundary_flush_snapshot(agent_id, &session, &test_full_agent(agent_id))
            .await
            .expect("snapshot");

        assert_eq!(snapshot.recent_explicit_writes.len(), 1);
        let marker = &snapshot.recent_explicit_writes[0];
        assert_eq!(marker.memory_ref, "fact-1");
        assert_eq!(marker.canonical_id.as_deref(), Some("canon-1"));
        assert_eq!(marker.summary, "User prefers concise replies");
        assert_eq!(marker.recorded_at, recorded_at);
    }
}
