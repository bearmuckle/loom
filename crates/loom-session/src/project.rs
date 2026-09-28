use std::collections::BTreeMap;

use loom_core::{AgentSessionId, ErrorCode, LoomError, ProjectId, Result, Timestamp};
use serde::{Deserialize, Serialize};

/// Root depth is one; nested agent sessions may be at most three levels deep.
pub const MAX_AGENT_DEPTH: u8 = loom_core::MAX_PROJECT_AGENT_DEPTH;
/// Default limit for simultaneously active child agents in one project.
pub const DEFAULT_MAX_CONCURRENT_AGENTS: usize = 4;

/// Membership of one agent session in a project hierarchy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentMembership {
    pub project_id: ProjectId,
    pub session_id: AgentSessionId,
    pub parent_session_id: Option<AgentSessionId>,
    pub depth: u8,
    pub created_at: Timestamp,
}

/// A project is rooted at its original session ID. The ID is intentionally the
/// same type and value as the root session ID for the initial migration model.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectRecord {
    pub id: ProjectId,
    pub root_session_id: AgentSessionId,
    pub members: BTreeMap<AgentSessionId, AgentMembership>,
    pub created_at: Timestamp,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectManagerState {
    pub projects: BTreeMap<ProjectId, ProjectRecord>,
    /// Required positive cap on simultaneously active child agents per project.
    pub max_concurrent_agents: usize,
}

#[derive(Debug)]
pub struct ProjectManager {
    projects: BTreeMap<ProjectId, ProjectRecord>,
    max_concurrent_agents: usize,
}

impl Default for ProjectManager {
    fn default() -> Self {
        Self::with_max_concurrent_agents(DEFAULT_MAX_CONCURRENT_AGENTS)
    }
}

impl ProjectManager {
    pub fn with_max_concurrent_agents(limit: usize) -> Self {
        Self {
            projects: BTreeMap::new(),
            max_concurrent_agents: limit,
        }
    }

    pub fn from_state(state: ProjectManagerState) -> Result<Self> {
        let mut all_members = std::collections::BTreeSet::new();
        for (id, project) in &state.projects {
            if *id != project.id
                || project.id != ProjectId::from_uuid(*project.root_session_id.as_uuid())
            {
                return Err(malformed("project key, ID, and root session ID must match"));
            }
            let root = project.members.get(&project.root_session_id);
            if root.is_none_or(|m| {
                m.project_id != project.id
                    || m.session_id != project.root_session_id
                    || m.parent_session_id.is_some()
                    || m.depth != 1
            }) {
                return Err(malformed("project root membership is inconsistent"));
            }
            for (member_id, member) in &project.members {
                if *member_id != member.session_id {
                    return Err(malformed(
                        "project member key does not match its session ID",
                    ));
                }
                if !all_members.insert(member.session_id) {
                    return Err(malformed("a session belongs to multiple projects"));
                }
                if member.project_id != project.id
                    || member.depth == 0
                    || member.depth > MAX_AGENT_DEPTH
                {
                    return Err(malformed("project membership has invalid project or depth"));
                }
                if member.session_id == project.root_session_id {
                    continue;
                }
                let parent_id = member
                    .parent_session_id
                    .ok_or_else(|| malformed("non-root member has no parent"))?;
                let parent = project
                    .members
                    .get(&parent_id)
                    .ok_or_else(|| malformed("member parent is not in the same project"))?;
                if parent.depth.checked_add(1) != Some(member.depth) {
                    return Err(malformed("member depth does not match its parent"));
                }
            }
            // Parent depth strictly decreases while walking upward, so the checks above
            // also rule out cycles and disconnected membership chains.
        }
        Ok(Self {
            projects: state.projects,
            max_concurrent_agents: state.max_concurrent_agents,
        })
    }

    pub fn export_state(&self) -> ProjectManagerState {
        ProjectManagerState {
            projects: self.projects.clone(),
            max_concurrent_agents: self.max_concurrent_agents,
        }
    }

    /// Registers an existing root session as a project and its depth-one member.
    pub fn create_project(&mut self, root_session_id: AgentSessionId) -> Result<ProjectRecord> {
        let project_id = ProjectId::from_uuid(*root_session_id.as_uuid());
        if self.projects.contains_key(&project_id)
            || self
                .projects
                .values()
                .any(|p| p.members.contains_key(&root_session_id))
        {
            return Err(LoomError::conflict(format!(
                "session {root_session_id} already belongs to a project"
            )));
        }
        let now = Timestamp::now();
        let membership = AgentMembership {
            project_id,
            session_id: root_session_id,
            parent_session_id: None,
            depth: 1,
            created_at: now,
        };
        let project = ProjectRecord {
            id: project_id,
            root_session_id,
            members: BTreeMap::from([(root_session_id, membership)]),
            created_at: now,
        };
        self.projects.insert(project_id, project.clone());
        Ok(project)
    }

    /// Adds a member only through an existing project and an existing parent.
    pub fn add_agent(
        &mut self,
        project_id: ProjectId,
        session_id: AgentSessionId,
        parent_session_id: AgentSessionId,
    ) -> Result<AgentMembership> {
        if self
            .projects
            .values()
            .any(|p| p.members.contains_key(&session_id))
        {
            return Err(LoomError::conflict(format!(
                "session {session_id} already belongs to a project"
            )));
        }
        let project = self
            .projects
            .get_mut(&project_id)
            .ok_or_else(|| LoomError::not_found("project", project_id))?;
        let parent = project.members.get(&parent_session_id).ok_or_else(|| {
            LoomError::invalid_request("parent session is not a member of this project")
        })?;
        let depth = parent
            .depth
            .checked_add(1)
            .ok_or_else(|| LoomError::invalid_request("agent hierarchy depth exceeded"))?;
        if depth > MAX_AGENT_DEPTH {
            return Err(LoomError::invalid_request("agent hierarchy depth exceeded"));
        }
        let membership = AgentMembership {
            project_id,
            session_id,
            parent_session_id: Some(parent_session_id),
            depth,
            created_at: Timestamp::now(),
        };
        project.members.insert(session_id, membership.clone());
        Ok(membership)
    }

    /// Reparents an existing member, validating same-project ancestry and depth.
    pub fn set_parent(
        &mut self,
        project_id: ProjectId,
        session_id: AgentSessionId,
        parent_session_id: AgentSessionId,
    ) -> Result<AgentMembership> {
        let project = self
            .projects
            .get_mut(&project_id)
            .ok_or_else(|| LoomError::not_found("project", project_id))?;
        if session_id == project.root_session_id {
            return Err(LoomError::invalid_request(
                "project root cannot be reparented",
            ));
        }
        let parent = project.members.get(&parent_session_id).ok_or_else(|| {
            LoomError::invalid_request("parent session is not a member of this project")
        })?;
        if parent_session_id == session_id || is_descendant(project, parent_session_id, session_id)
        {
            return Err(LoomError::invalid_request(
                "agent hierarchy cannot contain a cycle",
            ));
        }
        let subtree_height = subtree_height(project, session_id);
        let new_depth = parent
            .depth
            .checked_add(1)
            .ok_or_else(|| LoomError::invalid_request("agent hierarchy depth exceeded"))?;
        if new_depth.saturating_add(subtree_height) > MAX_AGENT_DEPTH {
            return Err(LoomError::invalid_request("agent hierarchy depth exceeded"));
        }
        let old_depth = project
            .members
            .get(&session_id)
            .ok_or_else(|| LoomError::not_found("project member", session_id))?
            .depth;
        let delta = i16::from(new_depth) - i16::from(old_depth);
        let descendants = descendant_ids(project, session_id);
        for id in descendants {
            let member = project.members.get_mut(&id).expect("collected from map");
            member.depth = (i16::from(member.depth) + delta) as u8;
        }
        let member = project
            .members
            .get_mut(&session_id)
            .expect("validated above");
        member.parent_session_id = Some(parent_session_id);
        member.depth = new_depth;
        Ok(member.clone())
    }

    pub fn get(&self, project_id: ProjectId) -> Result<ProjectRecord> {
        self.projects
            .get(&project_id)
            .cloned()
            .ok_or_else(|| LoomError::not_found("project", project_id))
    }

    pub fn membership(
        &self,
        project_id: ProjectId,
        session_id: AgentSessionId,
    ) -> Result<AgentMembership> {
        self.projects
            .get(&project_id)
            .and_then(|p| p.members.get(&session_id))
            .cloned()
            .ok_or_else(|| LoomError::not_found("project member", session_id))
    }

    /// Checks whether the server may launch one more child agent.
    pub fn validate_concurrency(
        &self,
        project_id: ProjectId,
        active_agent_count: usize,
    ) -> Result<()> {
        self.projects
            .get(&project_id)
            .ok_or_else(|| LoomError::not_found("project", project_id))?;
        if active_agent_count >= self.max_concurrent_agents {
            return Err(LoomError::conflict("project concurrency limit reached"));
        }
        Ok(())
    }
}

fn descendant_ids(project: &ProjectRecord, ancestor: AgentSessionId) -> Vec<AgentSessionId> {
    let mut result = vec![ancestor];
    let mut index = 0;
    while index < result.len() {
        let parent = result[index];
        result.extend(
            project
                .members
                .values()
                .filter(|m| m.parent_session_id == Some(parent))
                .map(|m| m.session_id),
        );
        index += 1;
    }
    result
}

fn subtree_height(project: &ProjectRecord, root: AgentSessionId) -> u8 {
    descendant_ids(project, root)
        .into_iter()
        .filter_map(|id| project.members.get(&id).map(|m| m.depth))
        .max()
        .unwrap_or(0)
        .saturating_sub(project.members.get(&root).map_or(0, |m| m.depth))
}

fn is_descendant(
    project: &ProjectRecord,
    candidate: AgentSessionId,
    ancestor: AgentSessionId,
) -> bool {
    let mut current = candidate;
    while let Some(member) = project.members.get(&current) {
        let Some(parent) = member.parent_session_id else {
            return false;
        };
        if parent == ancestor {
            return true;
        }
        current = parent;
    }
    false
}

fn malformed(message: &'static str) -> LoomError {
    LoomError::new(ErrorCode::MalformedPayload, message, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_project_root_and_nested_members_and_round_trips() {
        let mut manager = ProjectManager::default();
        let root = AgentSessionId::new();
        let project = manager.create_project(root).unwrap();
        let project_id = ProjectId::from_uuid(*root.as_uuid());
        assert_eq!(project.id, project_id);
        assert_eq!(project.members[&root].depth, 1);
        let child = AgentSessionId::new();
        assert_eq!(manager.add_agent(project_id, child, root).unwrap().depth, 2);
        let grandchild = AgentSessionId::new();
        assert_eq!(
            manager
                .add_agent(project_id, grandchild, child)
                .unwrap()
                .depth,
            3
        );
        let restored = ProjectManager::from_state(manager.export_state()).unwrap();
        assert_eq!(
            restored
                .membership(project_id, grandchild)
                .unwrap()
                .parent_session_id,
            Some(child)
        );
    }

    #[test]
    fn rejects_standalone_members_cross_project_parents_and_excess_depth() {
        let mut manager = ProjectManager::default();
        let root = AgentSessionId::new();
        manager.create_project(root).unwrap();
        let project_id = ProjectId::from_uuid(*root.as_uuid());
        let foreign_root = AgentSessionId::new();
        manager.create_project(foreign_root).unwrap();
        let standalone = AgentSessionId::new();
        assert!(
            manager
                .add_agent(project_id, standalone, standalone)
                .is_err()
        );
        assert!(
            manager
                .add_agent(project_id, AgentSessionId::new(), foreign_root)
                .is_err()
        );
        let child = AgentSessionId::new();
        manager.add_agent(project_id, child, root).unwrap();
        let grandchild = AgentSessionId::new();
        manager.add_agent(project_id, grandchild, child).unwrap();
        assert!(
            manager
                .add_agent(project_id, AgentSessionId::new(), grandchild)
                .is_err()
        );
    }

    #[test]
    fn enforces_capacity_and_prevents_reparent_cycles_or_invalid_depth() {
        let root = AgentSessionId::new();
        let mut manager = ProjectManager::with_max_concurrent_agents(2);
        manager.create_project(root).unwrap();
        let project_id = ProjectId::from_uuid(*root.as_uuid());
        let child = AgentSessionId::new();
        manager.add_agent(project_id, child, root).unwrap();
        assert!(manager.validate_concurrency(project_id, 1).is_ok());
        assert!(manager.validate_concurrency(project_id, 2).is_err());
        assert!(manager.set_parent(project_id, child, child).is_err());
        let grandchild = AgentSessionId::new();
        manager.add_agent(project_id, grandchild, child).unwrap();
        assert!(manager.set_parent(project_id, child, grandchild).is_err());
    }

    #[test]
    fn default_concurrency_cap_rejects_a_fifth_active_child() {
        let mut manager = ProjectManager::default();
        let root = AgentSessionId::new();
        manager.create_project(root).unwrap();
        let project_id = ProjectId::from_uuid(*root.as_uuid());

        assert!(
            manager
                .validate_concurrency(project_id, DEFAULT_MAX_CONCURRENT_AGENTS - 1)
                .is_ok()
        );
        assert!(
            manager
                .validate_concurrency(project_id, DEFAULT_MAX_CONCURRENT_AGENTS)
                .is_err()
        );
    }

    #[test]
    fn reparenting_moves_a_member_and_rejects_a_subtree_that_exceeds_max_depth() {
        let mut manager = ProjectManager::default();
        let root = AgentSessionId::new();
        let project_id = manager.create_project(root).unwrap().id;
        let left = AgentSessionId::new();
        let right = AgentSessionId::new();
        let leaf = AgentSessionId::new();
        manager.add_agent(project_id, left, root).unwrap();
        manager.add_agent(project_id, right, root).unwrap();
        manager.add_agent(project_id, leaf, left).unwrap();

        let moved = manager.set_parent(project_id, leaf, right).unwrap();
        assert_eq!(moved.parent_session_id, Some(right));
        assert_eq!(moved.depth, 3);
        assert_eq!(manager.membership(project_id, left).unwrap().depth, 2);

        let mut too_deep = ProjectManager::default();
        let root = AgentSessionId::new();
        let project_id = too_deep.create_project(root).unwrap().id;
        let branch = AgentSessionId::new();
        let parent = AgentSessionId::new();
        let leaf = AgentSessionId::new();
        too_deep.add_agent(project_id, branch, root).unwrap();
        too_deep.add_agent(project_id, parent, root).unwrap();
        too_deep.add_agent(project_id, leaf, branch).unwrap();
        assert_eq!(
            too_deep
                .set_parent(project_id, branch, parent)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn rejects_malformed_project_manager_snapshots() {
        let mut source = ProjectManager::default();
        let root = AgentSessionId::new();
        let project_id = source.create_project(root).unwrap().id;
        let child = AgentSessionId::new();
        source.add_agent(project_id, child, root).unwrap();
        let valid = source.export_state();

        let mut wrong_project_key = valid.clone();
        let project = wrong_project_key.projects.remove(&project_id).unwrap();
        wrong_project_key.projects.insert(ProjectId::new(), project);
        assert!(matches!(
            ProjectManager::from_state(wrong_project_key)
                .unwrap_err()
                .code,
            ErrorCode::MalformedPayload
        ));

        let mut missing_parent = valid.clone();
        missing_parent
            .projects
            .get_mut(&project_id)
            .unwrap()
            .members
            .get_mut(&child)
            .unwrap()
            .parent_session_id = None;
        assert_eq!(
            ProjectManager::from_state(missing_parent).unwrap_err().code,
            ErrorCode::MalformedPayload
        );

        let mut absent_parent = valid.clone();
        absent_parent
            .projects
            .get_mut(&project_id)
            .unwrap()
            .members
            .get_mut(&child)
            .unwrap()
            .parent_session_id = Some(AgentSessionId::new());
        assert_eq!(
            ProjectManager::from_state(absent_parent).unwrap_err().code,
            ErrorCode::MalformedPayload
        );

        let mut wrong_depth = valid.clone();
        wrong_depth
            .projects
            .get_mut(&project_id)
            .unwrap()
            .members
            .get_mut(&child)
            .unwrap()
            .depth = 3;
        assert_eq!(
            ProjectManager::from_state(wrong_depth).unwrap_err().code,
            ErrorCode::MalformedPayload
        );

        let mut wrong_member_key = valid.clone();
        let project = wrong_member_key.projects.get_mut(&project_id).unwrap();
        let membership = project.members.remove(&child).unwrap();
        project.members.insert(AgentSessionId::new(), membership);
        assert_eq!(
            ProjectManager::from_state(wrong_member_key)
                .unwrap_err()
                .code,
            ErrorCode::MalformedPayload
        );

        let mut invalid_root = valid.clone();
        invalid_root
            .projects
            .get_mut(&project_id)
            .unwrap()
            .members
            .get_mut(&root)
            .unwrap()
            .depth = 2;
        assert_eq!(
            ProjectManager::from_state(invalid_root).unwrap_err().code,
            ErrorCode::MalformedPayload
        );

        let mut invalid_membership_project = valid.clone();
        invalid_membership_project
            .projects
            .get_mut(&project_id)
            .unwrap()
            .members
            .get_mut(&child)
            .unwrap()
            .project_id = ProjectId::new();
        assert_eq!(
            ProjectManager::from_state(invalid_membership_project)
                .unwrap_err()
                .code,
            ErrorCode::MalformedPayload
        );

        let mut zero_depth = valid.clone();
        zero_depth
            .projects
            .get_mut(&project_id)
            .unwrap()
            .members
            .get_mut(&child)
            .unwrap()
            .depth = 0;
        assert_eq!(
            ProjectManager::from_state(zero_depth).unwrap_err().code,
            ErrorCode::MalformedPayload
        );

        let mut excessive_depth = valid.clone();
        excessive_depth
            .projects
            .get_mut(&project_id)
            .unwrap()
            .members
            .get_mut(&child)
            .unwrap()
            .depth = MAX_AGENT_DEPTH + 1;
        assert_eq!(
            ProjectManager::from_state(excessive_depth)
                .unwrap_err()
                .code,
            ErrorCode::MalformedPayload
        );

        let mut second_manager = ProjectManager::default();
        let second_root = AgentSessionId::new();
        let second_project = second_manager.create_project(second_root).unwrap();
        let mut duplicate_member_state = valid.clone();
        let mut second_project = second_project;
        second_project.members.insert(
            child,
            AgentMembership {
                project_id: second_project.id,
                session_id: child,
                parent_session_id: Some(second_root),
                depth: 2,
                created_at: Timestamp::now(),
            },
        );
        duplicate_member_state
            .projects
            .insert(second_project.id, second_project);
        assert_eq!(
            ProjectManager::from_state(duplicate_member_state)
                .unwrap_err()
                .code,
            ErrorCode::MalformedPayload
        );
    }

    #[test]
    fn project_manager_rejects_missing_members_and_duplicate_registration() {
        let root = AgentSessionId::new();
        let mut manager = ProjectManager::default();
        let project = manager.create_project(root).unwrap();
        let project_id = project.id;
        assert_eq!(
            manager.create_project(root).unwrap_err().code,
            ErrorCode::Conflict
        );
        let child = AgentSessionId::new();
        manager.add_agent(project_id, child, root).unwrap();
        assert_eq!(
            manager.add_agent(project_id, child, root).unwrap_err().code,
            ErrorCode::Conflict
        );
        assert_eq!(
            manager
                .add_agent(ProjectId::new(), AgentSessionId::new(), root)
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
        assert_eq!(
            manager
                .set_parent(project_id, root, child)
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            manager
                .set_parent(project_id, child, AgentSessionId::new())
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            manager
                .set_parent(project_id, AgentSessionId::new(), root)
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
        assert_eq!(
            manager.get(ProjectId::new()).unwrap_err().code,
            ErrorCode::NotFound
        );
        assert_eq!(
            manager
                .membership(project_id, AgentSessionId::new())
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
        assert_eq!(
            manager
                .validate_concurrency(ProjectId::new(), 0)
                .unwrap_err()
                .code,
            ErrorCode::NotFound
        );
    }
}
