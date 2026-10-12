use hule::{
    CleanupProfile::{self, Aggressive, Low, Normal},
    ProjectMode::{self, Ephemeral, Persistent},
    RetentionDecision::{self, Eligible, KeepPersistent, KeepProtected, SkipUnregistered},
    decide_retention,
};

#[test]
fn retention_policy_covers_every_combination() {
    let cases = [
        // Unregistered projects are never eligible
        (Low, None, SkipUnregistered),
        (Normal, None, SkipUnregistered),
        (Aggressive, None, SkipUnregistered),
        // Persistent projects retain artifacts except under aggressive cleanup
        (Low, Some(Persistent), KeepPersistent),
        (Normal, Some(Persistent), KeepPersistent),
        (Aggressive, Some(Persistent), Eligible),
        // Ephemeral artifacts pass the project-retention rule
        (Low, Some(Ephemeral), Eligible),
        (Normal, Some(Ephemeral), Eligible),
        (Aggressive, Some(Ephemeral), Eligible),
    ];

    for (profile, mode, expected) in cases {
        assert_eq!(
            decide_retention(profile, mode, false),
            expected,
            "Unprotected: profile: {profile:?}, mode: {mode:?}"
        );
        assert_eq!(
            decide_retention(profile, mode, true),
            KeepProtected,
            "Protected: profile: {profile:?}, mode: {mode:?}"
        );
    }
}

#[test]
fn protected_artifacts_are_kept_even_under_agressive_cleanup() {
    let decision = decide_retention(
        CleanupProfile::Aggressive,
        Some(ProjectMode::Ephemeral),
        true,
    );
    assert_eq!(decision, RetentionDecision::KeepProtected);
}

#[test]
fn unregistered_projects_are_skipped_in_every_profile() {
    let profiles = [
        CleanupProfile::Low,
        CleanupProfile::Normal,
        CleanupProfile::Aggressive,
    ];

    for profile in profiles {
        let decision = decide_retention(profile, None, false);
        assert_eq!(
            decision,
            RetentionDecision::SkipUnregistered,
            "profile: {profile:?}"
        );
    }
}

#[test]
fn presistent_artifacts_are_kept_under_low_and_normal_cleanup() {
    for profile in [CleanupProfile::Low, CleanupProfile::Normal] {
        let decision = decide_retention(profile, Some(ProjectMode::Persistent), false);
        assert_eq!(
            decision,
            RetentionDecision::KeepPersistent,
            "profile: {profile:?}"
        );
    }
}

#[test]

fn unprotected_persisted_artifacts_are_eligible_under_aggressive_cleanup() {
    let decision = decide_retention(
        CleanupProfile::Aggressive,
        Some(ProjectMode::Persistent),
        false,
    );

    assert_eq!(decision, RetentionDecision::Eligible);
}
