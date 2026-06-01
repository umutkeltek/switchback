#[derive(Clone, Debug)]
pub struct AuditContext {
    pub source: String,
    pub detail: String,
    pub object_id: Option<String>,
    pub actor_role: Option<String>,
    pub actor_tenant: Option<String>,
    pub actor_project: Option<String>,
}

impl AuditContext {
    pub fn new(source: impl Into<String>, detail: impl Into<String>) -> Self {
        let source = source.into();
        Self {
            detail: detail.into(),
            object_id: None,
            actor_role: None,
            actor_tenant: None,
            actor_project: None,
            source,
        }
    }

    pub fn with_object_id(mut self, object_id: impl Into<String>) -> Self {
        self.object_id = Some(object_id.into());
        self
    }

    pub fn with_actor(
        mut self,
        role: impl Into<String>,
        tenant: Option<String>,
        project: Option<String>,
    ) -> Self {
        self.actor_role = Some(role.into());
        self.actor_tenant = tenant;
        self.actor_project = project;
        self
    }
}
