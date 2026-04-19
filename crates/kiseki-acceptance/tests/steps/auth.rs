//! Step definitions for authentication.feature.

use crate::KisekiWorld;
use cucumber::given;

#[given(regex = r#"^a Kiseki cluster with Cluster CA "(\S+)"$"#)]
async fn given_cluster_ca(_world: &mut KisekiWorld, _ca_name: String) {
    // CA setup is implicit in transport config.
}

#[given(regex = r#"^a Kiseki cluster managed by cluster admin "(\S+)"$"#)]
async fn given_cluster_admin(_world: &mut KisekiWorld, _admin: String) {}

#[given(regex = r#"^tenant "(\S+)" managed by tenant admin "(\S+)"$"#)]
async fn given_tenant_admin(world: &mut KisekiWorld, tenant: String, _admin: String) {
    world.ensure_tenant(&tenant);
}
