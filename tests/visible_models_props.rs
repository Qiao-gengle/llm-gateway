//! Property 12: 可见模型列表
//!
//! **Validates: Requirements 4.2, 4.3, 4.4, 4.5**

use std::collections::HashMap;
use std::sync::Arc;

use llm_gateway::registry::{Fingerprint, RouteEntry, RouteTarget, Snapshot};
use proptest::prelude::*;
use uuid::Uuid;

fn cases() -> u32 {
    let env = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    env.max(256)
}

fn model_name(i: usize) -> String {
    format!("model-{i:03}")
}

/// 构造一个 Snapshot：每个下标 `i` 对应一个 Public_Model，其 Route 含若干 Route_Target，
/// `targets[i]` 为对应 Target 的 enabled 标志列表。
fn build_snapshot(targets: &[Vec<bool>]) -> Snapshot {
    let mut routes_by_model = HashMap::new();
    for (i, flags) in targets.iter().enumerate() {
        let name = model_name(i);
        let route_targets: Vec<RouteTarget> = flags
            .iter()
            .enumerate()
            .map(|(j, &enabled)| RouteTarget {
                id: Uuid::new_v4(),
                provider_id: Uuid::new_v4(),
                upstream_model: format!("upstream-{i}-{j}"),
                priority: 0,
                weight: 1,
                default_max_tokens: None,
                enabled,
            })
            .collect();
        routes_by_model.insert(
            name.clone(),
            Arc::new(RouteEntry { id: Uuid::new_v4(), public_model: name, targets: route_targets }),
        );
    }
    Snapshot { providers: HashMap::new(), routes_by_model, fingerprint: Fingerprint::default() }
}

/// 独立于被测实现，按需求文字重新计算期望结果。
fn expected_models(targets: &[Vec<bool>], allowed: &Option<Vec<String>>) -> Vec<String> {
    let mut out: Vec<String> = targets
        .iter()
        .enumerate()
        .filter(|(_, flags)| flags.iter().any(|&b| b))
        .map(|(i, _)| model_name(i))
        .filter(|m| match allowed {
            None => true,
            Some(a) => a.iter().any(|x| x == m),
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

fn arb_case() -> impl Strategy<Value = (Vec<Vec<bool>>, Option<Vec<usize>>)> {
    (0usize..8).prop_flat_map(|n| {
        let targets = prop::collection::vec(prop::collection::vec(any::<bool>(), 1..=3), n);
        let allowed = prop::option::of(prop::collection::vec(0usize..(n + 3), 0..6));
        (targets, allowed)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    // Feature: llm-gateway, Property 12: 可见模型列表
    /// **Validates: Requirements 4.2, 4.3, 4.4, 4.5**
    #[test]
    fn p12_visible_models((targets, allowed_idx) in arb_case()) {
        let snapshot = build_snapshot(&targets);
        let allowed_models: Option<Vec<String>> = allowed_idx.map(|idx| idx.into_iter().map(model_name).collect());

        let actual = snapshot.visible_models(allowed_models.as_deref());
        let expected = expected_models(&targets, &allowed_models);

        prop_assert_eq!(actual.iter().map(|s| s.to_string()).collect::<Vec<_>>(), expected);

        // 严格升序且无重复。
        for w in actual.windows(2) {
            prop_assert!(w[0] < w[1], "not strictly ascending: {:?}", actual);
        }
    }
}

#[test]
fn p12_empty_registry_yields_empty_list() {
    let snapshot = Snapshot::empty();
    assert!(snapshot.visible_models(None).is_empty());
    assert!(snapshot.visible_models(Some(&["anything".to_string()])).is_empty());
}

#[test]
fn p12_route_without_enabled_target_is_omitted() {
    let snapshot = build_snapshot(&[vec![false, false]]);
    assert!(snapshot.visible_models(None).is_empty());
}

#[test]
fn p12_allowed_models_referencing_unknown_model_is_ignored() {
    let snapshot = build_snapshot(&[vec![true]]);
    let allowed = vec!["model-000".to_string(), "no-such-model".to_string()];
    assert_eq!(snapshot.visible_models(Some(&allowed)), vec!["model-000"]);
}
