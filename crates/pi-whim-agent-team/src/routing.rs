use pi_whim_core::AgentTeamConfig;
use uuid::Uuid;

use crate::model::{AgentId, AgentNode, MessageKind, TeamState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutingError {
    TargetUnavailable,
    Forbidden,
    DepthLimit,
    LevelCapacity,
    DuplicateName,
}

pub fn resolve_visible_target(
    state: &TeamState,
    actor_id: AgentId,
    target: &str,
) -> Result<AgentId, RoutingError> {
    let actor = descriptor(state, actor_id)?;
    if target == "parent" {
        return actor.parent_id.ok_or(RoutingError::TargetUnavailable);
    }
    let candidate = Uuid::parse_str(target)
        .ok()
        .and_then(|id| {
            state.actors.get(&id).map(|_| id).or_else(|| {
                state
                    .actors
                    .values()
                    .find(|node| node.descriptor.session_id == id)
                    .map(|node| node.descriptor.id)
            })
        })
        .or_else(|| unique_visible_name(state, actor_id, target))
        .ok_or(RoutingError::TargetUnavailable)?;
    is_visible(state, actor_id, candidate)
        .then_some(candidate)
        .ok_or(RoutingError::TargetUnavailable)
}

pub fn message_kind(
    state: &TeamState,
    sender_id: AgentId,
    recipient_id: AgentId,
) -> Result<MessageKind, RoutingError> {
    let sender = descriptor(state, sender_id)?;
    let recipient = descriptor(state, recipient_id)?;
    message_kind_for_descriptors(sender, recipient)
}

pub fn message_kind_for_descriptors(
    sender: &crate::model::AgentDescriptor,
    recipient: &crate::model::AgentDescriptor,
) -> Result<MessageKind, RoutingError> {
    if sender.session_id == recipient.session_id {
        return Err(RoutingError::Forbidden);
    }
    // Level-0 sessions are user-visible peers and may coordinate by stable session ID even
    // when they belong to different runtime teams. Subagents never receive this exception.
    if sender.level == 0 && recipient.level == 0 {
        return Ok(MessageKind::PeerMessage);
    }
    if sender.team_id != recipient.team_id {
        return Err(RoutingError::Forbidden);
    }
    if sender.level == recipient.level && sender.parent_session_id == recipient.parent_session_id {
        return Ok(MessageKind::PeerMessage);
    }
    if sender.parent_session_id == Some(recipient.session_id)
        || recipient.parent_session_id == Some(sender.session_id)
    {
        return Ok(MessageKind::DirectNotification);
    }
    Err(RoutingError::Forbidden)
}

pub fn validate_child(
    state: &TeamState,
    config: &AgentTeamConfig,
    parent_id: AgentId,
    name: &str,
) -> Result<u8, RoutingError> {
    let parent = descriptor(state, parent_id)?;
    let child_level = parent.level.saturating_add(1);
    let maximum = config
        .maximum_for_level(child_level)
        .ok_or(RoutingError::DepthLimit)?;
    let active_at_level = state
        .actors
        .values()
        .filter(|node| {
            node.descriptor.team_id == parent.team_id
                && node.descriptor.level == child_level
                && node.descriptor.status.is_active()
        })
        .count();
    if active_at_level >= usize::from(maximum) {
        return Err(RoutingError::LevelCapacity);
    }
    let duplicate = state.actors.values().any(|node| {
        node.descriptor.team_id == parent.team_id
            && node.descriptor.parent_id == Some(parent_id)
            && node.descriptor.status.is_active()
            && node.descriptor.name == name
    });
    if duplicate {
        return Err(RoutingError::DuplicateName);
    }
    if parent.name == name {
        return Err(RoutingError::DuplicateName);
    }
    Ok(child_level)
}

fn unique_visible_name(state: &TeamState, actor_id: AgentId, target: &str) -> Option<AgentId> {
    let matching: Vec<_> = state
        .actors
        .values()
        .filter(|node| node.descriptor.name == target)
        .filter(|node| is_visible(state, actor_id, node.descriptor.id))
        .collect();
    let active: Vec<_> = matching
        .iter()
        .filter(|node| node.descriptor.status.is_active())
        .map(|node| node.descriptor.id)
        .collect();
    match active.as_slice() {
        [id] => Some(*id),
        [] if matching.len() == 1 => Some(matching[0].descriptor.id),
        _ => None,
    }
}

pub fn is_direct_child(state: &TeamState, parent_id: AgentId, child_id: AgentId) -> bool {
    state
        .actors
        .get(&child_id)
        .is_some_and(|node| node.descriptor.parent_id == Some(parent_id))
}

pub fn visible_agent_ids(state: &TeamState, actor_id: AgentId) -> Vec<AgentId> {
    state
        .actors
        .keys()
        .copied()
        .filter(|candidate| is_visible(state, actor_id, *candidate))
        .collect()
}

fn is_visible(state: &TeamState, actor_id: AgentId, candidate_id: AgentId) -> bool {
    if actor_id == candidate_id {
        return true;
    }
    let Ok(actor) = descriptor(state, actor_id) else {
        return false;
    };
    let Ok(candidate) = descriptor(state, candidate_id) else {
        return false;
    };
    actor.team_id == candidate.team_id
        && (message_kind(state, actor_id, candidate_id).is_ok()
            || candidate.parent_id == Some(actor_id))
}

fn descriptor(
    state: &TeamState,
    id: AgentId,
) -> Result<&crate::model::AgentDescriptor, RoutingError> {
    state
        .actors
        .get(&id)
        .map(|node: &AgentNode| &node.descriptor)
        .ok_or(RoutingError::TargetUnavailable)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};

    use crate::model::{AgentDescriptor, AgentOutcome, AgentStatus, TeamId};

    use super::*;

    fn state() -> (TeamState, [AgentId; 6]) {
        let team = TeamId::new_v4();
        let other_team = TeamId::new_v4();
        let ids = std::array::from_fn(|_| AgentId::new_v4());
        let descriptors = [
            (ids[0], team, None, 0, "root"),
            (ids[1], team, Some(ids[0]), 1, "one"),
            (ids[2], team, Some(ids[0]), 1, "two"),
            (ids[3], team, Some(ids[1]), 2, "grandchild"),
            (ids[4], team, Some(ids[2]), 2, "cousin"),
            (ids[5], other_team, None, 0, "other-root"),
        ];
        let actors = descriptors
            .into_iter()
            .map(|(id, team_id, parent_id, level, name)| {
                (
                    id,
                    AgentNode {
                        descriptor: AgentDescriptor {
                            id,
                            session_id: id,
                            team_id,
                            parent_id,
                            parent_session_id: parent_id,
                            level,
                            name: name.into(),
                            role: String::new(),
                            status: AgentStatus::Running,
                            permission_level: pi_whim_core::AgentPermissionLevel::Full,
                        },
                        capability: id.to_string(),
                        task: String::new(),
                        session_path: None,
                        transcript: VecDeque::new(),
                        outcome: AgentOutcome::default(),
                        policy: pi_whim_core::AgentPermissionPolicy {
                            level: pi_whim_core::AgentPermissionLevel::Full,
                            ..pi_whim_core::AgentPermissionPolicy::default()
                        },
                        delegated_models: Vec::new(),
                    },
                )
            })
            .collect();
        (
            TeamState {
                root_id: ids[0],
                actors,
                capabilities: HashMap::new(),
                inboxes: HashMap::<_, VecDeque<_>>::new(),
                controls: HashMap::new(),
                background_processes: HashMap::new(),
            },
            ids,
        )
    }

    #[test]
    fn routing_allows_siblings_and_direct_relations_only() {
        let (state, ids) = state();
        assert_eq!(
            message_kind(&state, ids[1], ids[2]),
            Ok(MessageKind::PeerMessage)
        );
        assert_eq!(
            message_kind(&state, ids[1], ids[0]),
            Ok(MessageKind::DirectNotification)
        );
        assert_eq!(
            message_kind(&state, ids[1], ids[3]),
            Ok(MessageKind::DirectNotification)
        );
        assert_eq!(
            message_kind(&state, ids[0], ids[3]),
            Err(RoutingError::Forbidden)
        );
        assert_eq!(
            message_kind(&state, ids[3], ids[4]),
            Err(RoutingError::Forbidden)
        );
        assert_eq!(
            message_kind(&state, ids[0], ids[5]),
            Ok(MessageKind::PeerMessage)
        );
    }

    #[test]
    fn child_validation_enforces_depth_and_global_level_capacity() {
        let (state, ids) = state();
        let config = AgentTeamConfig {
            max_depth: 2,
            max_agents_per_level: 2,
            ..Default::default()
        };
        assert_eq!(
            validate_child(&state, &config, ids[0], "three"),
            Err(RoutingError::LevelCapacity)
        );
        assert_eq!(
            validate_child(&state, &config, ids[3], "too-deep"),
            Err(RoutingError::DepthLimit)
        );
    }
}
