//! Live tests against the real Apple Foundation Models bridge. They SKIP
//! (loudly, and are never counted as inference) when the bridge is not built or
//! the model is not ready. Build the bridge first; see the README.

use cli2::semantic::status::{model_status, model_test};
use cli2::semantic::ResolverSettings;

fn ready() -> Option<ResolverSettings> {
    let s = ResolverSettings::default();
    let st = model_status(&s);
    if st.ready {
        Some(s)
    } else {
        eprintln!(
            "SKIPPED live test: model not ready ({}); no inference was performed",
            st.state
        );
        None
    }
}

#[test]
fn live_status_never_reports_ready_without_the_framework_saying_so() {
    let st = model_status(&ResolverSettings::default());
    assert_eq!(
        st.ready,
        st.model.availability.as_deref() == Some("available") && st.state == "ready"
    );
}

#[test]
fn live_inference_returns_a_validated_structured_result() {
    let Some(s) = ready() else { return };
    let t = model_test(&s);
    assert!(t.passed, "{t:?}");
    assert!(t.outcome.is_some());
}
