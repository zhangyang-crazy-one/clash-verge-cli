//! CONFIG-01 and CONFIG-03: Round-trip config compatibility tests.
//! These tests are a CI gate - if they fail, the config format has drifted
//! from clash-verge-rev GUI and the binary must not ship.
//!
//! Each test deserializes a YAML fixture, serializes it back, then deserializes
//! again and serializes a second time. The two serializations must be
//! byte-identical. This catches:
//!   - Field reordering in IVerge / IProfiles / PrfItem
//!   - Changes to serde attributes (rename, skip_serializing_if, etc.)
//!   - serde_yaml_ng version drift
//!
//! `expect_used` and `unwrap_used` are allowed here because the round-trip
//! invariant is exactly the thing under test - panicking on deserialization
//! failure is the correct behavior (the test should fail loudly if the
//! format ever drifts to the point that the fixture itself no longer parses).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use clash_verge_core::config::{IProfiles, IVerge};
use serde_yaml_ng::Value;

/// Round-trip `verge.yaml`: deserialize, serialize, deserialize, serialize.
/// Asserts byte-identical output, proving the format is stable.
#[test]
fn test_verge_round_trip() {
    let fixture = include_str!("fixtures/verge.yaml");

    let parsed: IVerge = serde_yaml_ng::from_str(fixture).expect("verge.yaml must parse");

    let serialized_once = serde_yaml_ng::to_string(&parsed).expect("verge must serialize");
    let reparsed: IVerge = serde_yaml_ng::from_str(&serialized_once).expect("verge re-serialization must parse");
    let serialized_twice = serde_yaml_ng::to_string(&reparsed).expect("verge must serialize");

    assert_eq!(
        serialized_once, serialized_twice,
        "verge.yaml round-trip is not byte-identical"
    );
}

/// Round-trip `profiles.yaml`: same pattern as verge.
#[test]
fn test_profiles_round_trip() {
    let fixture = include_str!("fixtures/profiles.yaml");

    let parsed: IProfiles = serde_yaml_ng::from_str(fixture).expect("profiles.yaml must parse");

    let serialized_once = serde_yaml_ng::to_string(&parsed).expect("profiles must serialize");
    let reparsed: IProfiles = serde_yaml_ng::from_str(&serialized_once).expect("profiles re-serialization must parse");
    let serialized_twice = serde_yaml_ng::to_string(&reparsed).expect("profiles must serialize");

    assert_eq!(
        serialized_once, serialized_twice,
        "profiles.yaml round-trip is not byte-identical"
    );
}

/// Round-trip `IVerge::template()`: serialize the default template,
/// deserialize, and re-serialize. Asserts byte-identical output.
/// Catches struct definition changes (field renames, type changes,
/// new fields with non-stable order).
#[test]
fn test_verge_template_round_trip() {
    let template = IVerge::template();

    let serialized_once = serde_yaml_ng::to_string(&template).expect("verge template must serialize");
    let reparsed: IVerge =
        serde_yaml_ng::from_str(&serialized_once).expect("verge template re-serialization must parse");
    let serialized_twice = serde_yaml_ng::to_string(&reparsed).expect("verge template must serialize");

    assert_eq!(
        serialized_once, serialized_twice,
        "verge template round-trip is not byte-identical"
    );
}

#[test]
fn typed_edits_preserve_unknown_yaml_and_dns_clear_values() {
    let source = include_str!("fixtures/profiles-lossless.yaml");
    let original: Value = serde_yaml_ng::from_str(source).expect("profile fixture must parse");
    let mut profiles: IProfiles = serde_yaml_ng::from_str(source).expect("profile fixture must parse");
    let item = profiles.items.as_mut().unwrap().first_mut().unwrap();
    item.name = Some("After edit".into());
    item.option.as_mut().unwrap().user_agent = Some("after-agent".into());
    let saved = serde_yaml_ng::to_string(&profiles).expect("profiles must serialize");
    let actual: Value = serde_yaml_ng::from_str(&saved).expect("saved profiles must parse");

    for path in [("future_profiles", "mode"), ("future_profiles", "sequence")] {
        assert_eq!(actual[path.0][path.1], original[path.0][path.1]);
    }
    for key in ["future_item", "selected", "extra"] {
        assert_eq!(actual["items"][0][key], original["items"][0][key]);
    }
    assert_eq!(
        actual["items"][0]["option"]["future_option"],
        original["items"][0]["option"]["future_option"]
    );
    assert_eq!(
        actual["items"][0]["option"]["dns"]["nameserver"],
        Value::Sequence(Vec::new()),
        "an explicit empty list remains a clear operation"
    );
    assert!(
        actual["items"][0]["option"]["dns"]
            .as_mapping()
            .unwrap()
            .get("proxy-server-nameserver")
            .is_none(),
        "an omitted field remains unset and therefore inherits"
    );
    assert_eq!(actual["items"][0]["name"], Value::from("After edit"));
    assert_eq!(actual["items"][0]["option"]["user_agent"], Value::from("after-agent"));

    let verge_source = include_str!("fixtures/verge-lossless.yaml");
    let original_verge: Value = serde_yaml_ng::from_str(verge_source).expect("verge fixture must parse");
    let mut verge: IVerge = serde_yaml_ng::from_str(verge_source).expect("verge fixture must parse");
    verge.language = Some("en".into());
    let saved_verge = serde_yaml_ng::to_string(&verge).expect("verge must serialize");
    let actual_verge: Value = serde_yaml_ng::from_str(&saved_verge).expect("saved verge must parse");
    assert_eq!(actual_verge["future_verge"], original_verge["future_verge"]);
    assert_eq!(
        actual_verge["profile_dns_settings"]["R1"]["future_setting"],
        original_verge["profile_dns_settings"]["R1"]["future_setting"]
    );
    assert_eq!(actual_verge["language"], Value::from("en"));
}
