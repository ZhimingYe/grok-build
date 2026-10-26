use super::{Capability, Distribution};
use strum::IntoEnumIterator;
#[test]
fn stock_allows_every_capability() {
    for capability in Capability::iter() {
        assert!(Distribution::STOCK.allows(capability), "{capability:?}");
    }
}
#[test]
fn a_distribution_withholds_exactly_what_it_names() {
    for withheld in Capability::iter() {
        let distribution = Distribution::withholding(&[withheld]);
        for capability in Capability::iter() {
            assert_eq!(
                capability != withheld,
                distribution.allows(capability),
                "{withheld:?} {capability:?}"
            );
        }
    }
}
// ZMY SELF-ADDED CODE START
#[test]
fn fork_withholds_reporting_but_preserves_model_services() {
    for capability in Capability::iter() {
        assert_eq!(
            Distribution::current().allows(capability),
            matches!(capability, Capability::AccountLogin),
            "{capability:?}"
        );
    }
}
// ZMY SELF-ADDED CODE END
#[test]
fn only_a_withheld_capability_that_a_surface_offers_has_a_refusal() {
    for capability in Capability::iter() {
        assert_eq!(
            None,
            Distribution::STOCK.refusal(capability),
            "{capability:?}"
        );
        let withheld = Distribution::withholding(&[capability]);
        assert_eq!(
            [
                Capability::AccountLogin,
                Capability::RemoteControl,
                Capability::Voice,
            ]
            .contains(&capability),
            withheld.refusal(capability).is_some(),
            "{capability:?}"
        );
    }
}
