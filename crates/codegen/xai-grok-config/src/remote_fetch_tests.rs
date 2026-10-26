use super::*;
use crate::ConfigLayers;

fn features_remote_fetch(v: bool) -> toml::Value {
    toml::from_str(&format!("[features]\nremote_fetch = {v}\n"))
        .expect("remote_fetch fixture is valid TOML")
}

// ZMY SELF-ADDED CODE START
fn stock_remote_fetch_enabled_from_layers(layers: &ConfigLayers) -> bool {
    remote_fetch_enabled_from_layers_with_distribution(layers, Distribution::STOCK)
}

fn stock_remote_fetch_enabled_from_policy_layers(
    merged_requirements: Option<&toml::Value>,
    managed: Option<&toml::Value>,
    system_managed: Option<&toml::Value>,
) -> bool {
    remote_fetch_enabled_from_policy_layers_with_distribution(
        Distribution::STOCK,
        merged_requirements,
        managed,
        system_managed,
    )
}
// ZMY SELF-ADDED CODE END

#[test]
fn remote_fetch_defaults_to_true_when_absent() {
    // ZMY SELF-ADDED CODE START
    let layers = ConfigLayers::default();
    assert!(stock_remote_fetch_enabled_from_layers(&layers));
    assert!(!remote_fetch_enabled_from_layers(&layers));
    // ZMY SELF-ADDED CODE END
}

#[test]
fn remote_fetch_reads_user_config() {
    let layers = ConfigLayers {
        user: features_remote_fetch(false),
        ..ConfigLayers::default()
    };
    // ZMY SELF-ADDED CODE START
    assert!(!stock_remote_fetch_enabled_from_layers(&layers));
    // ZMY SELF-ADDED CODE END
}

#[test]
fn remote_fetch_managed_overrides_user() {
    let mut layers = ConfigLayers {
        user: features_remote_fetch(true),
        managed: features_remote_fetch(false),
        ..ConfigLayers::default()
    };
    assert!(
        // ZMY SELF-ADDED CODE START
        !stock_remote_fetch_enabled_from_layers(&layers),
        // ZMY SELF-ADDED CODE END
        "managed=false must beat user=true"
    );

    layers.user = features_remote_fetch(false);
    layers.managed = features_remote_fetch(true);
    assert!(
        // ZMY SELF-ADDED CODE START
        stock_remote_fetch_enabled_from_layers(&layers),
        // ZMY SELF-ADDED CODE END
        "managed=true must beat user=false"
    );
}

#[test]
fn remote_fetch_requirements_beat_managed_and_user() {
    let mut layers = ConfigLayers {
        user: features_remote_fetch(true),
        managed: features_remote_fetch(true),
        user_requirements: Some(features_remote_fetch(false)),
        ..ConfigLayers::default()
    };
    assert!(
        // ZMY SELF-ADDED CODE START
        !stock_remote_fetch_enabled_from_layers(&layers),
        // ZMY SELF-ADDED CODE END
        "requirements=false must beat managed and user"
    );

    layers.user = features_remote_fetch(false);
    layers.managed = features_remote_fetch(false);
    layers.user_requirements = Some(features_remote_fetch(true));
    assert!(
        // ZMY SELF-ADDED CODE START
        stock_remote_fetch_enabled_from_layers(&layers),
        // ZMY SELF-ADDED CODE END
        "requirements=true must beat managed and user"
    );
}

#[test]
fn remote_fetch_env_overlay_is_ignored_in_both_directions() {
    let layers = ConfigLayers {
        user: features_remote_fetch(false),
        env_overlay: Some(features_remote_fetch(true)),
        ..ConfigLayers::default()
    };
    // ZMY SELF-ADDED CODE START
    assert!(!stock_remote_fetch_enabled_from_layers(&layers));
    // ZMY SELF-ADDED CODE END

    let layers = ConfigLayers {
        env_overlay: Some(features_remote_fetch(false)),
        ..ConfigLayers::default()
    };
    // ZMY SELF-ADDED CODE START
    assert!(stock_remote_fetch_enabled_from_layers(&layers));
    // ZMY SELF-ADDED CODE END
}

#[test]
fn remote_fetch_prefers_managed_then_system_managed_then_user() {
    let mut layers = ConfigLayers {
        system_managed: features_remote_fetch(true),
        managed: features_remote_fetch(false),
        ..ConfigLayers::default()
    };
    // ZMY SELF-ADDED CODE START
    assert!(!stock_remote_fetch_enabled_from_layers(&layers));
    // ZMY SELF-ADDED CODE END

    layers.managed = features_remote_fetch(true);
    layers.system_managed = features_remote_fetch(false);
    // ZMY SELF-ADDED CODE START
    assert!(stock_remote_fetch_enabled_from_layers(&layers));
    // ZMY SELF-ADDED CODE END

    let layers = ConfigLayers {
        user: features_remote_fetch(true),
        system_managed: features_remote_fetch(false),
        ..ConfigLayers::default()
    };
    // ZMY SELF-ADDED CODE START
    assert!(!stock_remote_fetch_enabled_from_layers(&layers));
    // ZMY SELF-ADDED CODE END
}

#[test]
fn remote_fetch_prefers_mdm_then_system_then_user_requirements() {
    let mut layers = ConfigLayers {
        user_requirements: Some(features_remote_fetch(true)),
        system_requirements: Some(features_remote_fetch(false)),
        ..ConfigLayers::default()
    };
    // ZMY SELF-ADDED CODE START
    assert!(!stock_remote_fetch_enabled_from_layers(&layers));
    // ZMY SELF-ADDED CODE END

    layers.mdm_requirements = Some(features_remote_fetch(true));
    // ZMY SELF-ADDED CODE START
    assert!(stock_remote_fetch_enabled_from_layers(&layers));
    // ZMY SELF-ADDED CODE END

    layers.mdm_requirements = Some(features_remote_fetch(false));
    layers.system_requirements = Some(features_remote_fetch(true));
    // ZMY SELF-ADDED CODE START
    assert!(!stock_remote_fetch_enabled_from_layers(&layers));
    // ZMY SELF-ADDED CODE END
}

#[test]
fn remote_fetch_layer_load_failure_still_honors_policy_layers() {
    let off = features_remote_fetch(false);
    let on = features_remote_fetch(true);

    // ZMY SELF-ADDED CODE START
    assert!(!stock_remote_fetch_enabled_from_policy_layers(
        Some(&off),
        None,
        None
    ));
    // ZMY SELF-ADDED CODE END

    // ZMY SELF-ADDED CODE START
    assert!(!stock_remote_fetch_enabled_from_policy_layers(
        None,
        None,
        Some(&off)
    ));
    // ZMY SELF-ADDED CODE END

    // ZMY SELF-ADDED CODE START
    assert!(stock_remote_fetch_enabled_from_policy_layers(
        Some(&on),
        Some(&off),
        Some(&off)
    ));
    assert!(!stock_remote_fetch_enabled_from_policy_layers(
        None,
        Some(&off),
        Some(&on)
    ));

    assert!(
        stock_remote_fetch_enabled_from_policy_layers(None, None, None),
        "genuinely absent policy fails open"
    );
    assert!(!remote_fetch_enabled_from_policy_layers(
        Some(&on),
        Some(&on),
        Some(&on)
    ));
    // ZMY SELF-ADDED CODE END
}

#[test]
fn a_distribution_that_withholds_remote_fetch_beats_every_layer() {
    let on = features_remote_fetch(true);
    assert!(!remote_fetch_enabled_first_match(
        Distribution::withholding(&[Capability::RemoteFetch]),
        [Some(&on), None]
    ));
    assert!(remote_fetch_enabled_first_match(
        Distribution::STOCK,
        [Some(&on), None]
    ));
}

// ZMY SELF-ADDED CODE START
#[test]
fn fork_custom_remote_fetch_uses_stock_capability_but_keeps_explicit_policy() {
    let layers = ConfigLayers::default();
    assert!(remote_fetch_enabled_from_layers_with_distribution(
        &layers,
        Distribution::STOCK
    ));
    assert!(!remote_fetch_enabled_from_layers_with_distribution(
        &ConfigLayers {
            user: features_remote_fetch(false),
            ..ConfigLayers::default()
        },
        Distribution::STOCK
    ));
}
// ZMY SELF-ADDED CODE END
