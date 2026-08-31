//! Transport-free management operations shared by desktop and embedded hosts.
//!
//! The crate intentionally owns no sockets, task runtimes, filesystem policy,
//! or FFI. Hosts own one [`RuntimeApi`] and call its bounded operations from
//! their serial runtime owner.

mod document;
mod operations;
mod revisions;

pub use document::*;
pub use operations::*;
pub use revisions::*;

pub use logiksmith_core as core;

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;

    fn document(source: &str) -> AutomationDocument {
        AutomationDocument {
            signals: vec![],
            http_polls: vec![],
            webhook_inputs: vec![],
            blocks: vec![AutomationBlock {
                id: "main".into(),
                revision: 1,
                enabled: true,
                inputs: vec![AutomationEndpoint {
                    name: "trigger".into(),
                    dpt: "1.001".into(),
                }],
                outputs: vec![AutomationEndpoint {
                    name: "light".into(),
                    dpt: "1.001".into(),
                }],
                knx_bindings: vec![
                    KnxBinding {
                        endpoint: "trigger".into(),
                        group_address: "1/2/3".into(),
                    },
                    KnxBinding {
                        endpoint: "light".into(),
                        group_address: "1/2/4".into(),
                    },
                ],
                signal_bindings: vec![],
                http_bindings: vec![],
                webhook_bindings: vec![],
                source: source.into(),
                schedules: vec![],
            }],
        }
    }

    #[test]
    fn document_build_preserves_core_and_binding_identity() {
        let doc =
            document("function handle(event) return { outputs = { light = event.value } } end");
        let built = build_automation(doc.clone()).unwrap();
        assert_eq!(built.core_config.blocks[0].id.as_str(), "main");
        assert_eq!(
            built.core_config.blocks[0].endpoints[0].name.as_str(),
            "trigger"
        );
        assert_eq!(
            built.core_config.blocks[0].endpoints[1].name.as_str(),
            "light"
        );
        let trigger = "main.trigger";
        let light = "main.light";
        assert_eq!(
            format!(
                "{}.{}",
                built.address_to_inputs[&GroupAddress::parse("1/2/3").unwrap()][0].block_id,
                built.address_to_inputs[&GroupAddress::parse("1/2/3").unwrap()][0].endpoint
            ),
            trigger
        );
        assert_eq!(
            built.output_to_address[&(built.blocks[0].id.clone(), "light".parse().unwrap())]
                .to_string(),
            "1/2/4"
        );
        let rebuilt = build_automation(
            parse_automation_toml(&serialize_automation_toml(&doc).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(built.core_config, rebuilt.core_config);
        assert_eq!(
            built
                .address_to_inputs
                .keys()
                .collect::<std::collections::HashSet<_>>(),
            rebuilt
                .address_to_inputs
                .keys()
                .collect::<std::collections::HashSet<_>>()
        );
        let _ = light;
    }

    #[test]
    fn embedded_capabilities_report_stable_feature_errors() {
        let mut doc = document("function handle() end");
        doc.http_polls.push(HttpPoll {
            name: "weather".into(),
            url: "https://example.invalid".into(),
            every: "1m".into(),
            timeout: "1s".into(),
            stale_after: "2m".into(),
            headers: vec![],
            values: vec![],
        });
        let errors =
            build_automation_with_profile(doc, logiksmith_core::RuntimeProfile::EmbeddedBaseline)
                .unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.path == "http_polls" && e.message.starts_with("feature_disabled:"))
        );
    }

    #[test]
    fn simulation_is_effect_free_and_stale_activation_does_not_mutate() {
        let doc =
            document("function handle(event) return { outputs = { light = event.value } } end");
        let mut api =
            RuntimeApi::from_document(doc, logiksmith_core::RuntimeProfile::Desktop).unwrap();
        let before = api.projection(0);
        let result = api
            .simulate_input(SimulationRequest {
                block_id: "main".into(),
                source: None,
                trigger_endpoint: "trigger".into(),
                trigger_value: ValueMessage::Bool(true),
                previous: None,
                inputs: vec![SimulationInput {
                    endpoint: "trigger".into(),
                    value: Some(ValueMessage::Bool(true)),
                    valid: true,
                    age_ms: Some(0),
                }],
                state: BTreeMap::new(),
                pending_timers: vec![],
                now_ms: 1,
            })
            .unwrap();
        assert_eq!(result.outcome.outputs[0].value, ValueMessage::Bool(true));
        assert_eq!(api.projection(0), before);
        let error = api
            .activate(ActivationRequest {
                block_id: "main".into(),
                source: Some("function handle( ".into()),
                enabled: None,
                expected_document_revision: Revision(0),
                expected_structural_revision: Revision(api.revisions().1.0),
                expected_block_revision: Revision(0),
                new_document_revision: Revision(1),
            })
            .unwrap_err();
        assert!(matches!(error, OperationError::Stale { .. }));
        assert_eq!(api.projection(0), before);
    }

    #[test]
    fn revisions_are_decimal_strings_including_optionals_and_numbers_are_rejected() {
        #[derive(Serialize)]
        struct Shape {
            revision: Revision,
            #[serde(skip_serializing_if = "Option::is_none")]
            nested: Option<Revision>,
        }
        #[derive(Deserialize)]
        struct Read {
            revision: Revision,
            nested: Option<Revision>,
        }
        let json = serde_json::to_value(Shape {
            revision: Revision(9),
            nested: Some(Revision(10)),
        })
        .unwrap();
        assert_eq!(json, serde_json::json!({"revision":"9","nested":"10"}));
        let read: Read = serde_json::from_value(json).unwrap();
        assert_eq!(read.revision.0, 9);
        assert_eq!(read.nested.unwrap().0, 10);
        assert!(
            serde_json::from_value::<Read>(serde_json::json!({"revision":9,"nested":null}))
                .is_err()
        );
    }

    #[test]
    fn activation_cancels_only_block_timers_and_enablement_is_quiet() {
        let source = "function handle(event) if event.type == 'input' then return { timers = { off = { after = 10 } } } end end";
        let mut api =
            RuntimeApi::from_document(document(source), core::RuntimeProfile::Desktop).unwrap();
        api.deliver_input("main", "trigger", ValueMessage::Bool(true), 1)
            .unwrap();
        assert_eq!(api.projection(1).blocks[0].pending_timers.len(), 1);
        let structural = api.revisions().1;
        let activation = api
            .activate(ActivationRequest {
                block_id: "main".into(),
                source: Some("function handle() return {} end".into()),
                enabled: None,
                expected_document_revision: Revision(0),
                expected_structural_revision: structural,
                expected_block_revision: Revision(1),
                new_document_revision: Revision(1),
            })
            .unwrap();
        assert_eq!(activation.blocks[0].cancelled_timers, vec!["off"]);
        assert!(api.projection(1).blocks[0].pending_timers.is_empty());
        let structural = api.revisions().1;
        let disabled = api
            .set_enabled(
                "main",
                false,
                Revision(1),
                structural,
                Revision(2),
                Revision(2),
            )
            .unwrap();
        assert!(disabled.blocks[0].cancelled_timers.is_empty());
        assert_eq!(api.projection(1).blocks[0].health, "disabled");
        api.resume("main", Revision(2), structural, Revision(3))
            .unwrap();
        assert_eq!(api.projection(1).blocks[0].health, "disabled");
    }
}
