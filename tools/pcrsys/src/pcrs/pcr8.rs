//! PCR 8: Settings Measurement
//!
//! PCR 8 measures the node settings (user-data merged with image defaults).
//! rottweiler extends PCR 8 with the SHA-256 of the canonical JSON representation
//! of the merged settings, excluding `settings.updates.seed` and
//! `settings.network.hostname`.
//!
//! This module reproduces that measurement offline given:
//! 1. The image defaults TOML (extracted from ROOT-A)
//! 2. The user-data TOML (provided via `--user-data`)

use crate::error::Result;
use crate::parsers::bootconfig;
use crate::predict::{extend_pcr, PcrContext, PcrIndex, PcrRecord, PCR_INIT_VAL};
use crate::schnauzer;

use olpc_cjson::CanonicalFormatter;
use serde_json::Value;
use sha2::{Digest, Sha256};
use snafu::ResultExt;

/// Settings keys excluded from the PCR 8 measurement by rottweiler.
const EXCLUDED_KEYS: &[&[&str]] = &[
    &["settings", "updates", "seed"],
    &["settings", "network", "hostname"],
];

/// Predict PCR 8 value.
///
/// Returns `Ok(None)` when user-data is not provided (PCR 8 cannot be predicted
/// without knowing what settings will be applied at boot).
pub async fn predict(ctx: &PcrContext<'_>) -> Result<Option<(PcrIndex, PcrRecord)>> {
    let user_data = match ctx.user_data {
        Some(ud) => ud,
        None => {
            println!("no user data provided");
            return Ok(None);
        }
    };

    let settings_defaults = match ctx.settings_defaults {
        Some(sd) => sd,
        None => {
            println!("no settings defaults provided");
            return Ok(None);
        }
    };

    // Parse the defaults TOML and extract the [settings] table
    let defaults_table: Value = toml::from_str(settings_defaults)
        .whatever_context("failed to parse settings defaults TOML")?;

    let defaults_settings = defaults_table
        .get("settings")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    // Parse the user-data TOML and extract the [settings] table
    let user_table: Value =
        toml::from_str(user_data).whatever_context("failed to parse user-data TOML")?;

    let user_settings = user_table
        .get("settings")
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()));

    // Deep merge: user-data overrides defaults
    let mut merged = defaults_settings;
    deep_merge(&mut merged, &user_settings);

    // Generate boot settings from bootconfig.data (simulates prairiedog generate-boot-settings)
    if !ctx.bootconfig.is_empty() {
        if let Ok(params) = bootconfig::parse(ctx.bootconfig) {
            let boot_settings = boot_settings_from_bootconfig(&params);
            deep_merge(&mut merged, &boot_settings);
        }
    }

    // Simulate schnauzer setting-generators (ECR URLs, TUF URLs, aws-config, send-metrics)
    if let Some(region) = ctx.region {
        // settings.aws.region is populated by IMDS at boot; inject it for PCR 8 prediction
        if merged.get("aws").and_then(|a| a.get("region")).is_none() {
            let aws = merged
                .as_object_mut()
                .unwrap()
                .entry("aws")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            aws.as_object_mut()
                .unwrap()
                .entry("region")
                .or_insert_with(|| Value::String(region.to_string()));
        }

        let image_info = schnauzer::ImageInfo {
            variant_id: ctx.variant_id.unwrap_or("unknown").to_string(),
            arch: ctx.arch.unwrap_or("x86_64").to_string(),
        };
        schnauzer::apply_generators(&mut merged, settings_defaults, region, &image_info).await;
    }

    // Wrap in {"settings": ...} to match apiclient output format
    let mut root = serde_json::Map::new();
    root.insert("settings".to_string(), merged);
    let mut root_value = Value::Object(root);

    // Exclude non-deterministic keys
    for path in EXCLUDED_KEYS {
        remove_nested_key(&mut root_value, path);
    }

    // Serialize to canonical JSON (OLPC canonical form: sorted keys, no whitespace)
    let canonical_json = canonical_json(&root_value)?;

    if std::env::var_os("RUST_LOG").is_some_and(|v| v == "debug") {
        eprintln!("[debug] PCR 8 canonical JSON: {}", canonical_json);
    }

    // Extend PCR 8: SHA-256(init_value || SHA-256(canonical_json))
    let settings_digest: [u8; 32] = Sha256::digest(canonical_json.as_bytes()).into();
    let pcr8 = extend_pcr(&PCR_INIT_VAL, &settings_digest);

    Ok(Some((PcrIndex::Pcr8, PcrRecord::new(pcr8))))
}

/// Serialize a JSON value to OLPC Canonical JSON format.
///
/// Uses `olpc_cjson::CanonicalFormatter` which produces sorted-key, no-whitespace JSON
/// matching the output of `apiclient get settings --canonicalize`.
fn canonical_json(value: &Value) -> Result<String> {
    let mut buf = Vec::new();
    let mut serializer =
        serde_json::Serializer::with_formatter(&mut buf, CanonicalFormatter::new());
    serde::Serialize::serialize(value, &mut serializer)
        .whatever_context("failed to serialize canonical JSON")?;
    String::from_utf8(buf).whatever_context("canonical JSON is not valid UTF-8")
}

/// Convert parsed bootconfig params into `{"boot":{"kernel":{KEY:[VALUES]}}}` JSON structure.
/// This simulates what `prairiedog generate-boot-settings` produces at boot.
fn boot_settings_from_bootconfig(params: &bootconfig::BootconfigParams) -> Value {
    let mut kernel_map = serde_json::Map::new();
    for (key, value) in &params.kernel {
        let values: Vec<Value> = if value.is_empty() {
            vec![]
        } else {
            value.split(',').map(|v| Value::String(v.to_string())).collect()
        };
        kernel_map.insert(key.clone(), Value::Array(values));
    }

    let mut boot = serde_json::Map::new();
    if !kernel_map.is_empty() {
        boot.insert("kernel".to_string(), Value::Object(kernel_map));
    }

    let mut settings = serde_json::Map::new();
    if !boot.is_empty() {
        settings.insert("boot".to_string(), Value::Object(boot));
    }

    Value::Object(settings)
}

/// Deep merge two JSON values. `overlay` values override `base` values.
/// For objects, keys are merged recursively. For all other types, overlay replaces base.
fn deep_merge(base: &mut Value, overlay: &Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, overlay_val) in overlay_map {
                let entry = base_map.entry(key.clone()).or_insert(Value::Null);
                deep_merge(entry, overlay_val);
            }
        }
        (base, overlay) => {
            *base = overlay.clone();
        }
    }
}

/// Remove a nested key from a JSON value given a path of keys.
fn remove_nested_key(value: &mut Value, path: &[&str]) {
    if path.is_empty() {
        return;
    }
    if path.len() == 1 {
        if let Value::Object(map) = value {
            map.remove(path[0]);
        }
        return;
    }
    if let Value::Object(map) = value {
        if let Some(child) = map.get_mut(path[0]) {
            remove_nested_key(child, &path[1..]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Platform;
    use crate::predict::test_support::MockCtx;

    #[tokio::test]
    async fn test_predict_returns_none_without_user_data() {
        let m = MockCtx::new();
        let ctx = m.build(Platform::Aws);
        let result = predict(&ctx).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_predict_returns_none_without_settings_defaults() {
        let m = MockCtx::new();
        let ctx = PcrContext::builder()
            .platform(Platform::Aws)
            .efi_vars(&m.efi_vars)
            .partitions(&m.layout)
            .user_data("[settings.kubernetes]\ncluster-name = \"test\"")
            .build();
        let result = predict(&ctx).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_predict_basic() {
        let defaults = r#"
[settings]
[settings.motd]
message = "Welcome to Bottlerocket!"

[settings.updates]
seed = 42

[settings.network]
hostname = "default-host"
"#;

        let user_data = r#"
[settings.kubernetes]
cluster-name = "my-cluster"
"#;

        let m = MockCtx::new();
        let ctx = PcrContext::builder()
            .platform(Platform::Aws)
            .efi_vars(&m.efi_vars)
            .partitions(&m.layout)
            .user_data(user_data)
            .settings_defaults(defaults)
            .build();

        let result = predict(&ctx).await.unwrap().unwrap();
        assert_eq!(result.0, PcrIndex::Pcr8);
        assert_eq!(result.1.sha256.len(), 1);
        // The hash should be deterministic
        assert!(!result.1.sha256[0].is_empty());
    }

    #[tokio::test]
    async fn test_predict_excludes_seed_and_hostname() {
        // Changing only the excluded keys (seed, hostname) should not affect PCR value
        let defaults = r#"
[settings.motd]
message = "hello"

[settings.updates]
seed = 1
channel = "default"

[settings.network]
hostname = "host-a"
"#;

        let user_data_seed_a = r#"
[settings.updates]
seed = 100
"#;

        let user_data_seed_b = r#"
[settings.updates]
seed = 999

[settings.network]
hostname = "totally-different-host"
"#;

        let m = MockCtx::new();
        let ctx1 = PcrContext::builder()
            .platform(Platform::Aws)
            .efi_vars(&m.efi_vars)
            .partitions(&m.layout)
            .user_data(user_data_seed_a)
            .settings_defaults(defaults)
            .build();

        let ctx2 = PcrContext::builder()
            .platform(Platform::Aws)
            .efi_vars(&m.efi_vars)
            .partitions(&m.layout)
            .user_data(user_data_seed_b)
            .settings_defaults(defaults)
            .build();

        let result1 = predict(&ctx1).await.unwrap().unwrap();
        let result2 = predict(&ctx2).await.unwrap().unwrap();
        // Only seed and hostname differ between the two, and those are excluded
        assert_eq!(result1.1.sha256[0], result2.1.sha256[0]);
    }

    #[tokio::test]
    async fn test_predict_user_data_overrides_defaults() {
        let defaults = r#"
[settings.motd]
message = "default message"
"#;

        let user_data = r#"
[settings.motd]
message = "custom message"
"#;

        let m = MockCtx::new();
        let ctx = PcrContext::builder()
            .platform(Platform::Aws)
            .efi_vars(&m.efi_vars)
            .partitions(&m.layout)
            .user_data(user_data)
            .settings_defaults(defaults)
            .build();

        let result = predict(&ctx).await.unwrap().unwrap();

        // Verify by computing the expected value manually
        let mut expected_settings = serde_json::Map::new();
        let mut motd = serde_json::Map::new();
        motd.insert(
            "message".to_string(),
            Value::String("custom message".to_string()),
        );
        expected_settings.insert("motd".to_string(), Value::Object(motd));
        let mut root = serde_json::Map::new();
        root.insert("settings".to_string(), Value::Object(expected_settings));
        let root_value = Value::Object(root);

        let expected_json = canonical_json(&root_value).unwrap();
        let expected_digest: [u8; 32] = Sha256::digest(expected_json.as_bytes()).into();
        let expected_pcr = extend_pcr(&PCR_INIT_VAL, &expected_digest);

        assert_eq!(result.1.sha256[0], hex::encode(expected_pcr));
    }

    #[tokio::test]
    async fn test_deep_merge_nested() {
        let mut base: Value = serde_json::from_str(r#"{"a": {"b": 1, "c": 2}, "d": 3}"#).unwrap();
        let overlay: Value = serde_json::from_str(r#"{"a": {"b": 10, "e": 5}, "f": 6}"#).unwrap();

        deep_merge(&mut base, &overlay);

        assert_eq!(base["a"]["b"], 10);
        assert_eq!(base["a"]["c"], 2);
        assert_eq!(base["a"]["e"], 5);
        assert_eq!(base["d"], 3);
        assert_eq!(base["f"], 6);
    }

    #[tokio::test]
    async fn test_remove_nested_key() {
        let mut value: Value = serde_json::from_str(
            r#"{"settings": {"updates": {"seed": 42, "channel": "default"}, "motd": "hi"}}"#,
        )
        .unwrap();

        remove_nested_key(&mut value, &["settings", "updates", "seed"]);

        assert!(value["settings"]["updates"].get("seed").is_none());
        assert_eq!(value["settings"]["updates"]["channel"], "default");
        assert_eq!(value["settings"]["motd"], "hi");
    }

    #[tokio::test]
    async fn test_canonical_json_sorted_keys() {
        let value: Value =
            serde_json::from_str(r#"{"z": 1, "a": 2, "m": {"b": 3, "a": 4}}"#).unwrap();
        let result = canonical_json(&value).unwrap();
        assert_eq!(result, r#"{"a":2,"m":{"a":4,"b":3},"z":1}"#);
    }

    #[tokio::test]
    async fn test_canonical_json_no_whitespace() {
        let value: Value = serde_json::from_str(r#"{"key": "value"}"#).unwrap();
        let result = canonical_json(&value).unwrap();
        assert!(!result.contains(' '));
        assert_eq!(result, r#"{"key":"value"}"#);
    }

    #[tokio::test]
    async fn test_predict_deterministic() {
        let defaults = r#"
[settings.motd]
message = "hello"

[settings.container-runtime]
max-container-log-line-size = 16384
"#;

        let user_data = r#"
[settings.kubernetes]
cluster-name = "test-cluster"
"#;

        let m = MockCtx::new();
        let ctx1 = PcrContext::builder()
            .platform(Platform::Aws)
            .efi_vars(&m.efi_vars)
            .partitions(&m.layout)
            .user_data(user_data)
            .settings_defaults(defaults)
            .build();

        let ctx2 = PcrContext::builder()
            .platform(Platform::Aws)
            .efi_vars(&m.efi_vars)
            .partitions(&m.layout)
            .user_data(user_data)
            .settings_defaults(defaults)
            .build();

        let result1 = predict(&ctx1).await.unwrap().unwrap();
        let result2 = predict(&ctx2).await.unwrap().unwrap();
        assert_eq!(result1.1.sha256[0], result2.1.sha256[0]);
    }

    #[tokio::test]
    async fn test_predict_with_schnauzer_simulation() {
        let defaults = r#"
[settings.motd]
message = "hello"

[settings.host-containers.admin]
enabled = true

[settings.host-containers.control]
enabled = true

[metadata.settings.host-containers.admin.source.setting-generator]
command = "schnauzer-v2 render --requires 'aws@v1(helpers=[ecr-prefix])' --template '{{ ecr-prefix settings.aws.region }}/bottlerocket-admin:v0.21.0'"
strength = "weak"

[metadata.settings.host-containers.control.source.setting-generator]
command = "schnauzer-v2 render --requires 'aws@v1(helpers=[ecr-prefix])' --template '{{ ecr-prefix settings.aws.region }}/bottlerocket-control:v0.21.0'"
strength = "weak"

[metadata.settings.updates.targets-base-url]
setting-generator = "schnauzer-v2 render --requires 'aws@v1' --requires 'updates@v1(helpers=[tuf-prefix])' --template '{{ tuf-prefix settings.aws.region }}/targets/'"

[metadata.settings.aws.config]
setting-generator = "schnauzer-v2 render --requires 'aws@v1(helpers=[aws-config])' --template '{{ aws-config settings.aws.config settings.aws.profile }}'"

[metadata.settings.metrics.send-metrics]
setting-generator = "shibaken is-partition --partition aws --partition aws-us-gov"
"#;

        let user_data = r#"
[settings.kubernetes]
cluster-name = "test-cluster"
"#;

        let m = MockCtx::new();
        let ctx = PcrContext::builder()
            .platform(Platform::Aws)
            .efi_vars(&m.efi_vars)
            .partitions(&m.layout)
            .user_data(user_data)
            .settings_defaults(defaults)
            .region("us-west-2")
            .variant_id("aws-ecs-2")
            .arch("x86_64")
            .build();

        let result = predict(&ctx).await.unwrap().unwrap();
        assert_eq!(result.0, PcrIndex::Pcr8);

        // Verify schnauzer outputs are present by checking a second prediction
        // without region produces a different hash (generators not applied)
        let ctx_no_region = PcrContext::builder()
            .platform(Platform::Aws)
            .efi_vars(&m.efi_vars)
            .partitions(&m.layout)
            .user_data(user_data)
            .settings_defaults(defaults)
            .build();

        let result_no_region = predict(&ctx_no_region).await.unwrap().unwrap();
        assert_ne!(result.1.sha256[0], result_no_region.1.sha256[0]);
    }

    #[tokio::test]
    async fn test_predict_schnauzer_user_data_overrides_generator() {
        let defaults = r#"
[settings.motd]
message = "hello"

[metadata.settings.aws.config]
setting-generator = "schnauzer-v2 render --requires 'aws@v1(helpers=[aws-config])' --template '{{ aws-config settings.aws.config settings.aws.profile }}'"
"#;

        // User explicitly sets aws.config - generator should NOT override it
        let user_data = r#"
[settings.aws]
config = "custom-config-value"
region = "us-west-2"
"#;

        let m = MockCtx::new();
        let ctx = PcrContext::builder()
            .platform(Platform::Aws)
            .efi_vars(&m.efi_vars)
            .partitions(&m.layout)
            .user_data(user_data)
            .settings_defaults(defaults)
            .region("us-west-2")
            .variant_id("aws-ecs-2")
            .arch("x86_64")
            .build();

        let result = predict(&ctx).await.unwrap().unwrap();

        // Same prediction without region should produce same hash since user-data
        // already sets both aws.config and aws.region
        let ctx_no_region = PcrContext::builder()
            .platform(Platform::Aws)
            .efi_vars(&m.efi_vars)
            .partitions(&m.layout)
            .user_data(user_data)
            .settings_defaults(defaults)
            .build();

        let result_no_region = predict(&ctx_no_region).await.unwrap().unwrap();
        assert_eq!(result.1.sha256[0], result_no_region.1.sha256[0]);
    }

    #[tokio::test]
    #[ignore] // requires real disk image at pcr8-testing/
    async fn test_predict_with_real_disk() {
        let disk_path =
            std::path::Path::new("../../pcr8-testing/snap-aws-ecs-4-x86_64-dev-civ-xvda");
        if !disk_path.exists() {
            eprintln!("skipping: disk image not found");
            return;
        }

        let mut disk = std::fs::File::open(disk_path).unwrap();
        let partitions = crate::gpt::find_partitions(&mut disk).unwrap();
        let defaults = crate::diskfs::extract_settings_defaults(disk_path, &partitions).unwrap();
        let bootconfig = crate::diskfs::extract_bootconfig(&mut disk, &partitions).unwrap();

        let user_data = std::fs::read_to_string("../../pcr8-testing/test-userdata.toml").unwrap();

        let m = MockCtx::new();
        let ctx = PcrContext::builder()
            .platform(Platform::Aws)
            .efi_vars(&m.efi_vars)
            .partitions(&m.layout)
            .user_data(user_data.as_str())
            .settings_defaults(defaults.toml_content.as_str())
            .bootconfig(&bootconfig)
            .region("us-west-2")
            .variant_id(defaults.variant_id.as_str())
            .arch(defaults.arch.as_str())
            .build();

        let result = predict(&ctx).await.unwrap().unwrap();
        assert_eq!(result.0, PcrIndex::Pcr8);

        // Print canonical JSON for manual comparison
        eprintln!("PCR 8 hash: {}", result.1.sha256[0]);
    }
}
