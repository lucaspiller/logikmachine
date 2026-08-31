use super::*;

pub fn structural_revision(document: &AutomationDocument) -> u64 {
    let mut structure = document.clone();
    for block in &mut structure.blocks {
        block.source.clear();
        block.revision = 1;
        block.enabled = true;
    }
    let bytes = toml::to_string(&structure).unwrap_or_default();
    automation_revision(bytes.as_bytes())
}
pub fn automation_revision(source: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for b in source {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
pub fn document_logic_revision(document: &AutomationDocument) -> u64 {
    let mut bytes = Vec::new();
    for b in &document.blocks {
        bytes.extend_from_slice(b.id.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(b.source.as_bytes());
        bytes.push(0);
    }
    automation_revision(&bytes)
}
pub fn block_revision(document: &AutomationDocument, id: &str) -> u64 {
    document
        .blocks
        .iter()
        .find(|b| b.id == id)
        .map(|b| b.revision.max(1))
        .unwrap_or(1)
}
pub fn source_fingerprint(source: &str) -> String {
    let mut hash = 2166136261u32;
    for b in source.as_bytes() {
        hash ^= u32::from(*b);
        hash = hash.wrapping_mul(16777619);
    }
    format!("fnv1a-{hash:08x}")
}
pub fn parse_automation_toml(source: &str) -> Result<AutomationDocument, toml::de::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StoredAutomation {
        #[serde(default)]
        #[allow(dead_code)]
        revision: u16,
        #[serde(flatten)]
        document: AutomationDocument,
    }
    toml::from_str::<StoredAutomation>(source).map(|stored| stored.document)
}
pub fn parse_automation(source: &str) -> Result<AutomationDocument, toml::de::Error> {
    parse_automation_toml(source)
}
pub fn serialize_automation_toml(
    document: &AutomationDocument,
) -> Result<String, toml::ser::Error> {
    toml::to_string(document)
}
pub fn serialize_automation(document: &AutomationDocument) -> Result<String, toml::ser::Error> {
    serialize_automation_toml(document)
}
pub fn build_automation(
    document: AutomationDocument,
) -> Result<AutomationRuntime, Vec<FieldError>> {
    build_automation_with_capabilities(document, RuntimeProfile::Desktop, Capabilities::desktop())
}
pub fn build_automation_with_profile(
    document: AutomationDocument,
    profile: RuntimeProfile,
) -> Result<AutomationRuntime, Vec<FieldError>> {
    build_automation_with_capabilities(
        document,
        profile,
        if profile == RuntimeProfile::EmbeddedBaseline {
            Capabilities::embedded()
        } else {
            Capabilities::desktop()
        },
    )
}
pub fn build_automation_with_limits(
    document: AutomationDocument,
    limits: RuntimeLimits,
) -> Result<AutomationRuntime, Vec<FieldError>> {
    build_automation_inner(document, limits, Capabilities::desktop())
}
pub fn build_automation_with_capabilities(
    document: AutomationDocument,
    profile: RuntimeProfile,
    capabilities: Capabilities,
) -> Result<AutomationRuntime, Vec<FieldError>> {
    build_automation_inner(document, profile.limits(), capabilities)
}
fn build_automation_inner(
    document: AutomationDocument,
    limits: RuntimeLimits,
    capabilities: Capabilities,
) -> Result<AutomationRuntime, Vec<FieldError>> {
    let mut errors = Vec::new();
    if !capabilities.http_inputs
        && (!document.http_polls.is_empty()
            || document.blocks.iter().any(|b| !b.http_bindings.is_empty()))
    {
        errors.push(field(
            "http_polls",
            "feature_disabled: HTTP inputs are unavailable",
        ));
    }
    if !capabilities.webhook_inputs
        && (!document.webhook_inputs.is_empty()
            || document
                .blocks
                .iter()
                .any(|b| !b.webhook_bindings.is_empty()))
    {
        errors.push(field(
            "webhook_inputs",
            "feature_disabled: webhook inputs are unavailable",
        ));
    }
    if !capabilities.schedules && document.blocks.iter().any(|b| !b.schedules.is_empty()) {
        errors.push(field(
            "blocks.schedules",
            "feature_disabled: schedules are unavailable",
        ));
    }
    if document.blocks.is_empty() {
        errors.push(field("blocks", "must contain at least one block"));
    }
    if document.blocks.len() > MAX_BLOCKS {
        errors.push(field(
            "blocks",
            format!("must contain at most {MAX_BLOCKS} blocks"),
        ));
    }
    if document.signals.len() > MAX_SIGNALS {
        errors.push(field(
            "signals",
            format!("must contain at most {MAX_SIGNALS} signals"),
        ));
    }
    let mut signals = Vec::new();
    let mut signal_dpts: HashMap<SignalName, Dpt> = HashMap::new();
    for (i, s) in document.signals.iter().enumerate() {
        let p = format!("signals[{i}]");
        let Ok(name) = source_name(&format!("{p}.name"), &s.name) else {
            errors.push(field(
                format!("{p}.name"),
                "must be a valid canonical identifier",
            ));
            continue;
        };
        let Ok(dpt) = parse_dpt(&format!("{p}.dpt"), &s.dpt) else {
            errors.push(field(format!("{p}.dpt"), "must be 1.001, 5.001, or 9.001"));
            continue;
        };
        let name: SignalName = name.parse().expect("validated signal name");
        if signal_dpts.insert(name.clone(), dpt).is_some() {
            errors.push(field(format!("{p}.name"), "must be unique"));
        } else {
            signals.push(SignalRuntime { name, dpt });
        }
    }
    let mut source_dpts: HashMap<String, Dpt> = HashMap::new();
    let mut http_polls = Vec::new();
    let mut webhook_inputs = Vec::new();
    if document.http_polls.len() + document.webhook_inputs.len() > MAX_EXTERNAL_SOURCES {
        errors.push(field(
            "http_polls",
            format!("must contain at most {MAX_EXTERNAL_SOURCES} sources"),
        ));
    }
    for (i, poll) in document.http_polls.iter().enumerate() {
        let path = format!("http_polls[{i}]");
        let Ok(name) = source_name(&format!("{path}.name"), &poll.name) else {
            continue;
        };
        if !(poll.url.starts_with("http://") || poll.url.starts_with("https://"))
            || poll.url.chars().any(char::is_whitespace)
        {
            errors.push(field(format!("{path}.url"), "must be a valid HTTP URL"));
        }
        let every = parse_duration(&poll.every, false).ok();
        let timeout = poll
            .timeout
            .strip_suffix("ms")
            .and_then(|v| v.parse::<u64>().ok())
            .map_or_else(
                || {
                    parse_duration(&poll.timeout, false)
                        .ok()
                        .map(|v| v as u64 * 1000)
                },
                Some,
            );
        let stale_after = parse_duration(&poll.stale_after, false).ok();
        if every.is_none() || timeout.is_none() || stale_after.is_none() {
            errors.push(field(path.clone(), "poll durations must be valid"));
        }
        let mut values = Vec::new();
        for (vi, value) in poll.values.iter().enumerate() {
            let vp = format!("{path}.values[{vi}]");
            let Ok(value_name) = source_name(&format!("{vp}.name"), &value.name) else {
                continue;
            };
            let Ok(dpt) = parse_dpt(&format!("{vp}.dpt"), &value.dpt) else {
                continue;
            };
            validate_pointer(
                &format!("{vp}.json_pointer"),
                &value.json_pointer,
                &mut errors,
            );
            if source_dpts.insert(value_name.clone(), dpt).is_some() {
                errors.push(field(
                    format!("{vp}.name"),
                    "must be unique across external sources",
                ));
            }
            values.push(HttpPollValueRuntime {
                name: value_name,
                dpt,
                json_pointer: value.json_pointer.clone(),
            });
        }
        if let (Some(every), Some(timeout), Some(stale_after)) = (every, timeout, stale_after) {
            http_polls.push(HttpPollRuntime {
                name,
                url: poll.url.clone(),
                every: Duration::from_secs(every as u64),
                timeout: Duration::from_micros(timeout),
                stale_after: Duration::from_secs(stale_after as u64),
                headers: poll
                    .headers
                    .iter()
                    .filter_map(|h| h.value.clone().map(|v| (h.name.clone(), v)))
                    .collect(),
                values,
            });
        }
    }
    for (i, source) in document.webhook_inputs.iter().enumerate() {
        let path = format!("webhook_inputs[{i}]");
        let Ok(name) = source_name(&format!("{path}.name"), &source.name) else {
            continue;
        };
        let Ok(dpt) = parse_dpt(&format!("{path}.dpt"), &source.dpt) else {
            continue;
        };
        validate_pointer(
            &format!("{path}.json_pointer"),
            &source.json_pointer,
            &mut errors,
        );
        if source_dpts.insert(name.clone(), dpt).is_some() {
            errors.push(field(
                format!("{path}.name"),
                "must be unique across external sources",
            ));
        }
        webhook_inputs.push(WebhookInputRuntime {
            name,
            dpt,
            json_pointer: source.json_pointer.clone(),
            bearer_token: None,
        });
    }
    let mut blocks = Vec::new();
    let mut ids = HashSet::new();
    let mut address_to_inputs: HashMap<_, Vec<_>> = HashMap::new();
    let mut output_to_address = HashMap::new();
    let mut signal_to_inputs: HashMap<_, Vec<_>> = HashMap::new();
    let mut output_to_signal = HashMap::new();
    let mut address_dpts = HashMap::new();
    let mut signal_producers = HashSet::new();
    let mut signal_binding_count = 0;
    let mut http_to_inputs: HashMap<String, Vec<BlockExternalInputBinding>> = HashMap::new();
    let mut webhook_to_inputs: HashMap<String, Vec<BlockExternalInputBinding>> = HashMap::new();
    for (bi, b) in document.blocks.iter().enumerate() {
        let bp = format!("blocks[{bi}]");
        let id = match b.id.parse::<BlockId>() {
            Ok(v) => v,
            Err(e) => {
                errors.push(field(format!("{bp}.id"), e.to_string()));
                continue;
            }
        };
        if !ids.insert(id.clone()) {
            errors.push(field(format!("{bp}.id"), "must be unique"));
            continue;
        }
        let mut endpoint_dpts = HashMap::new();
        let mut directions = HashMap::new();
        let mut endpoints = Vec::new();
        let mut names = HashSet::new();
        for (direction, list, label) in [
            (EndpointDirection::Input, &b.inputs, "inputs"),
            (EndpointDirection::Output, &b.outputs, "outputs"),
        ] {
            for (i, e) in list.iter().enumerate() {
                let p = format!("{bp}.{label}[{i}]");
                let Ok(name) = endpoint_name(&format!("{p}.name"), &e.name) else {
                    errors.push(field(format!("{p}.name"), "must be a valid endpoint name"));
                    continue;
                };
                if !names.insert(name.clone()) {
                    errors.push(field(
                        format!("{p}.name"),
                        "must be unique within this block",
                    ));
                    continue;
                }
                let Ok(dpt) = parse_dpt(&format!("{p}.dpt"), &e.dpt) else {
                    errors.push(field(format!("{p}.dpt"), "must be 1.001, 5.001, or 9.001"));
                    continue;
                };
                endpoint_dpts.insert(name.clone(), dpt);
                directions.insert(name.clone(), direction);
                endpoints.push(Endpoint::new(name, direction, dpt));
            }
        }
        if b.source.is_empty() {
            errors.push(field(format!("{bp}.source"), "must not be empty"));
        }
        if b.source.len() > limits.max_logic_source_bytes_per_block {
            errors.push(field(
                format!("{bp}.source"),
                format!(
                    "must not exceed {} bytes",
                    limits.max_logic_source_bytes_per_block
                ),
            ));
        }
        let mut endpoint_to_address = HashMap::new();
        let mut endpoint_to_signal = HashMap::new();
        let mut endpoint_to_external = HashMap::new();
        for (i, binding) in b.knx_bindings.iter().enumerate() {
            let p = format!("{bp}.knx_bindings[{i}]");
            let Ok(name) = endpoint_name(&format!("{p}.endpoint"), &binding.endpoint) else {
                errors.push(field(
                    format!("{p}.endpoint"),
                    "must reference an existing endpoint",
                ));
                continue;
            };
            let Some(&dpt) = endpoint_dpts.get(&name) else {
                errors.push(field(
                    format!("{p}.endpoint"),
                    "must reference an existing endpoint",
                ));
                continue;
            };
            let address = match GroupAddress::parse(&binding.group_address) {
                Ok(a) => a,
                Err(e) => {
                    errors.push(field(format!("{p}.group_address"), e.to_string()));
                    continue;
                }
            };
            if endpoint_to_address.contains_key(&name) {
                errors.push(field(
                    format!("{p}.endpoint"),
                    "must have exactly one KNX binding",
                ));
                continue;
            }
            if let Some(prev) = address_dpts.get(&address)
                && prev != &dpt
            {
                errors.push(field(
                    format!("{p}.group_address"),
                    format!("DPT conflicts with existing binding at {address}"),
                ));
                continue;
            }
            address_dpts.insert(address, dpt);
            endpoint_to_address.insert(name.clone(), address);
            if directions.get(&name) == Some(&EndpointDirection::Input) {
                address_to_inputs
                    .entry(address)
                    .or_default()
                    .push(BlockInputBinding {
                        block_id: id.clone(),
                        endpoint: name,
                        dpt,
                        address,
                    });
            } else {
                output_to_address.insert((id.clone(), name), address);
            }
        }
        for (i, binding) in b.signal_bindings.iter().enumerate() {
            signal_binding_count += 1;
            let p = format!("{bp}.signal_bindings[{i}]");
            let Ok(name) = endpoint_name(&format!("{p}.endpoint"), &binding.endpoint) else {
                errors.push(field(
                    format!("{p}.endpoint"),
                    "must reference an existing endpoint",
                ));
                continue;
            };
            let Some(&dpt) = endpoint_dpts.get(&name) else {
                errors.push(field(
                    format!("{p}.endpoint"),
                    "must reference an existing endpoint",
                ));
                continue;
            };
            let Ok(signal) = binding.signal.parse::<SignalName>() else {
                errors.push(field(
                    format!("{p}.signal"),
                    "must reference an existing signal",
                ));
                continue;
            };
            let Some(&sdpt) = signal_dpts.get(&signal) else {
                errors.push(field(
                    format!("{p}.signal"),
                    "must reference an existing signal",
                ));
                continue;
            };
            if dpt != sdpt {
                errors.push(field(format!("{p}.signal"), "DPT conflicts with endpoint"));
                continue;
            }
            if endpoint_to_address.contains_key(&name) || endpoint_to_signal.contains_key(&name) {
                errors.push(field(
                    format!("{p}.endpoint"),
                    "must have exactly one binding",
                ));
                continue;
            }
            endpoint_to_signal.insert(name.clone(), signal.clone());
            if directions.get(&name) == Some(&EndpointDirection::Input) {
                signal_to_inputs
                    .entry(signal)
                    .or_default()
                    .push(BlockSignalInputBinding {
                        block_id: id.clone(),
                        endpoint: name,
                        dpt,
                        signal: binding.signal.parse().unwrap(),
                    });
            } else if !signal_producers.insert(signal.clone()) {
                errors.push(field(
                    format!("{p}.signal"),
                    "must have at most one producer",
                ));
            } else {
                output_to_signal.insert((id.clone(), name), signal);
            }
        }
        for (i, binding) in b.http_bindings.iter().enumerate() {
            let p = format!("{bp}.http_bindings[{i}]");
            match endpoint_name(&format!("{p}.endpoint"), &binding.endpoint) {
                Ok(name) => {
                    if endpoint_dpts.get(&name).is_none() {
                        errors.push(field(
                            format!("{p}.endpoint"),
                            "must reference an existing endpoint",
                        ));
                    } else if directions.get(&name) != Some(&EndpointDirection::Input) {
                        errors.push(field(
                            format!("{p}.endpoint"),
                            "must reference an input endpoint",
                        ));
                    } else if let Some(&dpt) = endpoint_dpts.get(&name) {
                        if source_dpts.get(&binding.source) != Some(&dpt) {
                            errors.push(field(
                                format!("{p}.source"),
                                "must reference an HTTP poll value with matching DPT",
                            ));
                        } else {
                            endpoint_to_external.insert(name.clone(), binding.source.clone());
                            http_to_inputs
                                .entry(binding.source.clone())
                                .or_default()
                                .push(BlockExternalInputBinding {
                                    block_id: id.clone(),
                                    endpoint: name,
                                    dpt,
                                    source: binding.source.clone(),
                                });
                        }
                    }
                }
                Err(e) => errors.push(e),
            }
        }
        for (i, binding) in b.webhook_bindings.iter().enumerate() {
            let p = format!("{bp}.webhook_bindings[{i}]");
            match endpoint_name(&format!("{p}.endpoint"), &binding.endpoint) {
                Ok(name) => {
                    if endpoint_dpts.get(&name).is_none() {
                        errors.push(field(
                            format!("{p}.endpoint"),
                            "must reference an existing endpoint",
                        ));
                    } else if directions.get(&name) != Some(&EndpointDirection::Input) {
                        errors.push(field(
                            format!("{p}.endpoint"),
                            "must reference an input endpoint",
                        ));
                    } else if let Some(&dpt) = endpoint_dpts.get(&name) {
                        if source_dpts.get(&binding.source) != Some(&dpt) {
                            errors.push(field(
                                format!("{p}.source"),
                                "must reference a webhook input with matching DPT",
                            ));
                        } else {
                            endpoint_to_external.insert(name.clone(), binding.source.clone());
                            webhook_to_inputs
                                .entry(binding.source.clone())
                                .or_default()
                                .push(BlockExternalInputBinding {
                                    block_id: id.clone(),
                                    endpoint: name,
                                    dpt,
                                    source: binding.source.clone(),
                                });
                        }
                    }
                }
                Err(e) => errors.push(e),
            }
        }
        for e in &endpoints {
            if !endpoint_to_address.contains_key(&e.name)
                && !endpoint_to_signal.contains_key(&e.name)
                && !endpoint_to_external.contains_key(&e.name)
            {
                errors.push(field(
                    format!(
                        "{bp}.{}",
                        if e.direction == EndpointDirection::Input {
                            "inputs"
                        } else {
                            "outputs"
                        }
                    ),
                    format!("endpoint {} must have exactly one binding", e.name),
                ));
            }
        }
        let mut schedules = Vec::new();
        if b.schedules.len() > limits.max_schedules_per_block {
            errors.push(field(format!("{bp}.schedules"), "too many schedules"));
        }
        for (i, s) in b.schedules.iter().enumerate() {
            let p = format!("{bp}.schedules[{i}]");
            let Ok(name) = s.name.parse::<ScheduleName>() else {
                errors.push(field(format!("{p}.name"), "must be a valid schedule name"));
                continue;
            };
            if let Some(rule) = schedule_rule(s, &p, &mut errors) {
                schedules.push(BlockSchedule {
                    name,
                    enabled: s.enabled,
                    rule,
                });
            }
        }
        let engine_config = EngineConfig::new(endpoints, b.source.clone());
        if let Err(e) = engine_config.validate_with_limits(&limits) {
            errors.push(field(format!("{bp}.source"), e.to_string()));
        }
        blocks.push(BlockRuntime {
            id: id.clone(),
            revision: b.revision.max(1),
            enabled: b.enabled,
            engine_config,
            endpoint_to_address,
            endpoint_to_signal,
            endpoint_to_external,
            endpoint_dpts,
            schedules,
        });
    }
    if signal_binding_count > MAX_SIGNAL_BINDINGS {
        errors.push(field(
            "blocks.signal_bindings",
            format!("must contain at most {MAX_SIGNAL_BINDINGS} bindings"),
        ));
    }
    let core_blocks = blocks
        .iter()
        .map(|b| {
            let mut cfg = BlockConfig::with_schedules(
                b.id.clone(),
                b.enabled,
                b.engine_config.endpoints.clone(),
                b.engine_config.logic.source.clone(),
                b.schedules.clone(),
            );
            cfg.signal_bindings = b
                .endpoint_to_signal
                .iter()
                .filter_map(|(endpoint, signal)| {
                    Some(CoreSignalBinding::new(endpoint.clone(), signal.clone()))
                })
                .collect();
            cfg
        })
        .collect();
    let core = CoreRuntimeConfig::with_signals(
        core_blocks,
        signals
            .iter()
            .map(|s| SignalConfig::new(s.name.clone(), s.dpt))
            .collect(),
    );
    if let Err(e) = core.validate_with_limits(&limits) {
        errors.push(field("blocks", e.to_string()));
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(AutomationRuntime {
        structural_revision: structural_revision(&document),
        document,
        signals,
        core_config: core,
        blocks,
        address_to_inputs,
        output_to_address,
        signal_to_inputs,
        output_to_signal,
        http_to_inputs,
        webhook_to_inputs,
        http_polls,
        webhook_inputs,
        address_dpts,
        document_revision: 0,
    })
}
