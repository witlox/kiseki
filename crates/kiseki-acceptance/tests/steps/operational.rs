//! Step definitions for operational.feature.

use crate::KisekiWorld;
use cucumber::given;

#[given(regex = r#"^tenant "(\S+)" with compliance tags \[([^\]]+)\]$"#)]
async fn given_compliance(w: &mut KisekiWorld, tenant: String, _tags: String) {
    w.ensure_tenant(&tenant);
}
