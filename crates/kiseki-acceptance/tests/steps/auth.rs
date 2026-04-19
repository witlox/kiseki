//! Step definitions for authentication.feature.
//! Most scenarios need real TLS connections — only background steps defined.

use crate::KisekiWorld;
use cucumber::given;

#[given(regex = r#"^a Kiseki cluster with Cluster CA "(\S+)"$"#)]
async fn given_ca(_w: &mut KisekiWorld, _ca: String) {}

#[given(regex = r#"^a Kiseki cluster managed by cluster admin "(\S+)"$"#)]
async fn given_admin(_w: &mut KisekiWorld, _admin: String) {}

#[given(regex = r#"^tenant "(\S+)" managed by tenant admin "(\S+)"$"#)]
async fn given_tenant_admin(w: &mut KisekiWorld, t: String, _admin: String) {
    w.ensure_tenant(&t);
}

#[given(regex = r#"^tenant "(\S+)" with certificate "(\S+)" signed by "(\S+)"$"#)]
async fn given_tenant_cert(w: &mut KisekiWorld, t: String, _cert: String, _ca: String) {
    w.ensure_tenant(&t);
}
