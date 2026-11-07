use std::collections::BTreeMap;

use loom_core::ProjectChildModelPolicy;

use super::*;

/// The `kind` a project's default child model is stored under.
const DEFAULT_CHILD_MODEL_KIND: &str = "";

impl FilePersistence {
    /// Loads the child model policy a project root stores; an empty policy
    /// when it stores none.
    pub fn load_project_child_model_policy(
        &self,
        project_id: ProjectId,
    ) -> Result<ProjectChildModelPolicy> {
        if !self.path.exists() {
            return Ok(ProjectChildModelPolicy::default());
        }
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT kind, model_id FROM project_child_models
                 WHERE project_id=?1 ORDER BY kind",
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not prepare project child model read: {error}"),
                    true,
                )
            })?;
        let rows = statement
            .query_map([project_id.as_uuid().as_bytes().as_slice()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| {
                persistence_error(
                    format!("could not read project child models: {error}"),
                    true,
                )
            })?;
        let mut policy = ProjectChildModelPolicy {
            default_model_id: None,
            by_kind: BTreeMap::new(),
        };
        for row in rows {
            let (kind, model_id) = row.map_err(|error| {
                persistence_error(
                    format!("could not read project child models: {error}"),
                    true,
                )
            })?;
            if kind == DEFAULT_CHILD_MODEL_KIND {
                policy.default_model_id = Some(model_id);
            } else {
                policy.by_kind.insert(kind, model_id);
            }
        }
        Ok(policy)
    }

    /// Replaces the child model policy a project root stores. The session must
    /// exist and must not be a delegated member of another project; an empty
    /// policy removes every row.
    pub fn replace_project_child_model_policy(
        &self,
        project_id: ProjectId,
        policy: &ProjectChildModelPolicy,
    ) -> Result<()> {
        let connection = self.connection_for_write()?;
        let transaction = connection.unchecked_transaction().map_err(|error| {
            persistence_error(
                format!("could not begin project child model write: {error}"),
                true,
            )
        })?;
        let project = project_id.as_uuid().as_bytes().as_slice();
        let is_root: bool = transaction
            .query_row(
                // A session written before project hierarchies has no
                // hierarchy row and is a project root of its own.
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)
                    AND NOT EXISTS(
                        SELECT 1 FROM sessions_hierarchy WHERE session_id=?1 AND depth > 1
                    )",
                [project],
                |row| row.get(0),
            )
            .map_err(|error| {
                persistence_error(format!("could not inspect the project root: {error}"), true)
            })?;
        if !is_root {
            return Err(LoomError::new(
                ErrorCode::NotFound,
                format!("project {project_id} does not exist"),
                false,
            ));
        }
        transaction
            .execute(
                "DELETE FROM project_child_models WHERE project_id=?1",
                [project],
            )
            .map_err(|error| {
                persistence_error(
                    format!("could not clear project child models: {error}"),
                    true,
                )
            })?;
        let rows = policy
            .default_model_id
            .iter()
            .map(|model| (DEFAULT_CHILD_MODEL_KIND, model.as_str()))
            .chain(
                policy
                    .by_kind
                    .iter()
                    .map(|(kind, model)| (kind.as_str(), model.as_str())),
            );
        for (kind, model_id) in rows {
            transaction
                .execute(
                    "INSERT INTO project_child_models(project_id, kind, model_id)
                     VALUES (?1, ?2, ?3)",
                    params![project, kind, model_id],
                )
                .map_err(|error| {
                    persistence_error(
                        format!("could not record project child model '{kind}': {error}"),
                        true,
                    )
                })?;
        }
        transaction.commit().map_err(|error| {
            persistence_error(
                format!("could not commit project child models: {error}"),
                true,
            )
        })
    }
}
