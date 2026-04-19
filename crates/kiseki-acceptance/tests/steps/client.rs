//! Step definitions for native-client.feature.

use crate::KisekiWorld;
use cucumber::given;

#[given("a compute node on the Slingshot fabric")]
async fn given_compute_node(_w: &mut KisekiWorld) {}

#[given(regex = r#"^tenant "(\S+)" with an active workload "(\S+)"$"#)]
async fn given_tenant_workload(w: &mut KisekiWorld, tenant: String, _workload: String) {
    w.ensure_tenant(&tenant);
}

#[given(regex = r#"^tenant KEK "(\S+)" available via tenant KMS$"#)]
async fn given_tenant_kek(_w: &mut KisekiWorld, _kek: String) {}

#[given("native client library linked into the workload process")]
async fn given_native_client(_w: &mut KisekiWorld) {}
